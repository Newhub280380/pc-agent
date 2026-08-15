// Захват экрана и PNG-кодирование.
//
// Почему GDI BitBlt, а не DXGI Desktop Duplication:
//   - BitBlt работает всегда: RDP, свёрнутые сессии, виртуалки, любые GPU;
//   - для агента важнее надёжность, чем 60 FPS — мы снимаем 1-2 кадра на шаг.
// Когда стоит перейти на DXGI (см. v2 roadmap): если нужен поток кадров
// (наблюдение за анимацией/видео) — там BitBlt даст ~30-80мс на 4K, а
// Desktop Duplication ~5мс. Узкое место названо честно.
#include "common.hpp"
#include "../include/agent_native.h"
#include <shcore.h>
#include <wincodec.h>
#include <vector>

#pragma comment(lib, "shcore.lib")
#pragma comment(lib, "windowscodecs.lib")

namespace {

struct MonitorList { std::vector<RECT> rects; };

BOOL CALLBACK enum_mon(HMONITOR h, HDC, LPRECT r, LPARAM lp) {
  (void)h;
  reinterpret_cast<MonitorList*>(lp)->rects.push_back(*r);
  return TRUE;
}

// Общий путь: DC источника -> DIB 32bpp -> RGBA.
int32_t capture_dc(HDC src, int x, int y, int w, int h, an_image* out) {
  if (w <= 0 || h <= 0) { an_set_error("capture: пустая область"); return -1; }
  HDC mem = ::CreateCompatibleDC(src);
  BITMAPINFO bi{};
  bi.bmiHeader.biSize = sizeof(BITMAPINFOHEADER);
  bi.bmiHeader.biWidth = w;
  bi.bmiHeader.biHeight = -h;  // top-down: иначе картинка придёт вверх ногами
  bi.bmiHeader.biPlanes = 1;
  bi.bmiHeader.biBitCount = 32;
  bi.bmiHeader.biCompression = BI_RGB;

  void* bits = nullptr;
  HBITMAP bmp = ::CreateDIBSection(src, &bi, DIB_RGB_COLORS, &bits, nullptr, 0);
  if (!bmp) { ::DeleteDC(mem); an_set_error("CreateDIBSection failed"); return -2; }
  HGDIOBJ old = ::SelectObject(mem, bmp);

  // CAPTUREBLT нужен, чтобы попадали слоистые окна (подсказки, меню, курсорные
  // оверлеи) — без него агент «не видит» часть UI.
  BOOL ok = ::BitBlt(mem, 0, 0, w, h, src, x, y, SRCCOPY | CAPTUREBLT);
  if (!ok) {
    ::SelectObject(mem, old); ::DeleteObject(bmp); ::DeleteDC(mem);
    an_set_error("BitBlt failed"); return -3;
  }

  const size_t bytes = (size_t)w * h * 4;
  auto* dst = static_cast<uint8_t*>(::CoTaskMemAlloc(bytes));
  if (!dst) {
    ::SelectObject(mem, old); ::DeleteObject(bmp); ::DeleteDC(mem);
    an_set_error("OOM"); return -4;
  }
  // GDI отдаёт BGRA; LLM и PNG ждут RGBA — переставляем каналы на месте.
  const auto* srcp = static_cast<const uint8_t*>(bits);
  for (size_t i = 0; i < bytes; i += 4) {
    dst[i + 0] = srcp[i + 2];
    dst[i + 1] = srcp[i + 1];
    dst[i + 2] = srcp[i + 0];
    dst[i + 3] = 255;
  }
  ::SelectObject(mem, old); ::DeleteObject(bmp); ::DeleteDC(mem);

  out->data = dst; out->width = w; out->height = h; out->stride = w * 4;
  return 0;
}

}  // namespace

extern "C" int32_t an_capture_screen(an_image* out) {
  int x = ::GetSystemMetrics(SM_XVIRTUALSCREEN);
  int y = ::GetSystemMetrics(SM_YVIRTUALSCREEN);
  int w = ::GetSystemMetrics(SM_CXVIRTUALSCREEN);
  int h = ::GetSystemMetrics(SM_CYVIRTUALSCREEN);
  HDC screen = ::GetDC(nullptr);
  int32_t rc = capture_dc(screen, x, y, w, h, out);
  ::ReleaseDC(nullptr, screen);
  return rc;
}

extern "C" int32_t an_capture_monitor(int32_t index, an_image* out) {
  MonitorList ml;
  ::EnumDisplayMonitors(nullptr, nullptr, enum_mon, reinterpret_cast<LPARAM>(&ml));
  if (index < 0 || index >= (int32_t)ml.rects.size()) { an_set_error("нет такого монитора"); return -1; }
  RECT r = ml.rects[index];
  HDC screen = ::GetDC(nullptr);
  int32_t rc = capture_dc(screen, r.left, r.top, r.right - r.left, r.bottom - r.top, out);
  ::ReleaseDC(nullptr, screen);
  return rc;
}

extern "C" int32_t an_capture_window(uint64_t hwnd, an_image* out) {
  HWND h = reinterpret_cast<HWND>(hwnd);
  if (!::IsWindow(h)) { an_set_error("невалидный HWND"); return -1; }
  RECT r{};
  ::GetWindowRect(h, &r);
  HDC wdc = ::GetWindowDC(h);
  int32_t rc = capture_dc(wdc, 0, 0, r.right - r.left, r.bottom - r.top, out);
  ::ReleaseDC(h, wdc);
  return rc;
}

extern "C" void an_free_image(an_image* img) {
  if (img && img->data) { ::CoTaskMemFree(img->data); img->data = nullptr; }
}

extern "C" int32_t an_encode_png(const an_image* img, uint8_t** out_buf, int32_t* out_len) {
  // WIC вместо libpng/stb: уже есть в системе, ноль зависимостей и он
  // аппаратно оптимизирован. Минус — только Windows, но нам туда и надо.
  if (!img || !img->data) { an_set_error("png: пустое изображение"); return -1; }
  IWICImagingFactory* factory = nullptr;
  if (FAILED(::CoCreateInstance(CLSID_WICImagingFactory, nullptr, CLSCTX_INPROC_SERVER,
                                IID_PPV_ARGS(&factory)))) {
    an_set_error("WIC factory failed"); return -2;
  }
  IStream* stream = nullptr;
  IWICBitmapEncoder* enc = nullptr;
  IWICBitmapFrameEncode* frame = nullptr;
  int32_t rc = -3;

  if (SUCCEEDED(::CreateStreamOnHGlobal(nullptr, TRUE, &stream)) &&
      SUCCEEDED(factory->CreateEncoder(GUID_ContainerFormatPng, nullptr, &enc)) &&
      SUCCEEDED(enc->Initialize(stream, WICBitmapEncoderNoCache)) &&
      SUCCEEDED(enc->CreateNewFrame(&frame, nullptr)) &&
      SUCCEEDED(frame->Initialize(nullptr))) {
    WICPixelFormatGUID fmt = GUID_WICPixelFormat32bppRGBA;
    frame->SetSize(img->width, img->height);
    frame->SetPixelFormat(&fmt);
    if (SUCCEEDED(frame->WritePixels(img->height, img->stride,
                                     img->stride * img->height, img->data)) &&
        SUCCEEDED(frame->Commit()) && SUCCEEDED(enc->Commit())) {
      HGLOBAL hg = nullptr;
      ::GetHGlobalFromStream(stream, &hg);
      SIZE_T sz = ::GlobalSize(hg);
      void* src = ::GlobalLock(hg);
      auto* buf = static_cast<uint8_t*>(::CoTaskMemAlloc(sz));
      if (buf) {
        memcpy(buf, src, sz);
        *out_buf = buf;
        *out_len = (int32_t)sz;
        rc = 0;
      }
      ::GlobalUnlock(hg);
    }
  }
  if (frame) frame->Release();
  if (enc) enc->Release();
  if (stream) stream->Release();
  factory->Release();
  if (rc != 0) an_set_error("PNG encode failed");
  return rc;
}
