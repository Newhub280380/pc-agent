// Поиск шаблона на экране: многомасштабный NCC по пирамиде изображений.
//
// Зачем это нужно рядом с LLM-зрением: спросить модель «где кнопка Купить» —
// это 1-3 секунды и деньги за каждый кадр. Если кнопку один раз нашли и
// запомнили её вид, повторный поиск локально занимает ~5-20мс и бесплатен.
// Поэтому пайплайн такой: UIA -> шаблон -> OCR -> и только потом LLM-зрение.
//
// Почему NCC (нормированная кросс-корреляция), а не разница пикселей:
// NCC устойчива к смене яркости/темы окна. Почему многомасштабный: DPI-скейл
// и зум браузера меняют размер кнопки на 80-150%.
//
// Узкое место (честно): NCC на CPU для 4K и большого шаблона — десятки мс на
// масштаб. Варианты ускорения на выбор:
//   1) пирамида + поиск сначала на 1/4 разрешения (реализовано);
//   2) SIMD/AVX2 или OpenCV matchTemplate (DFT) — быстрее, но +зависимость;
//   3) ONNX-детектор UI-элементов на GPU — точнее всего, но нужен рантайм.
#include "common.hpp"
#include "../include/agent_native.h"
#include <wincodec.h>
#include <vector>
#include <cmath>
#include <algorithm>

namespace {

struct Gray {
  int w = 0, h = 0;
  std::vector<float> p;
  float at(int x, int y) const { return p[(size_t)y * w + x]; }
};

Gray to_gray(const uint8_t* rgba, int w, int h, int stride) {
  Gray g; g.w = w; g.h = h; g.p.resize((size_t)w * h);
  for (int y = 0; y < h; ++y)
    for (int x = 0; x < w; ++x) {
      const uint8_t* px = rgba + (size_t)y * stride + (size_t)x * 4;
      g.p[(size_t)y * w + x] = 0.299f * px[0] + 0.587f * px[1] + 0.114f * px[2];
    }
  return g;
}

// Билинейный ресайз: нужен и для пирамиды, и для перебора масштабов шаблона.
Gray resize(const Gray& s, int nw, int nh) {
  Gray d; d.w = nw; d.h = nh; d.p.resize((size_t)nw * nh);
  const float fx = (float)s.w / nw, fy = (float)s.h / nh;
  for (int y = 0; y < nh; ++y) {
    float sy = (y + 0.5f) * fy - 0.5f;
    int y0 = std::clamp((int)std::floor(sy), 0, s.h - 1);
    int y1 = std::min(y0 + 1, s.h - 1);
    float ty = std::clamp(sy - y0, 0.f, 1.f);
    for (int x = 0; x < nw; ++x) {
      float sx = (x + 0.5f) * fx - 0.5f;
      int x0 = std::clamp((int)std::floor(sx), 0, s.w - 1);
      int x1 = std::min(x0 + 1, s.w - 1);
      float tx = std::clamp(sx - x0, 0.f, 1.f);
      float a = s.at(x0, y0) * (1 - tx) + s.at(x1, y0) * tx;
      float b = s.at(x0, y1) * (1 - tx) + s.at(x1, y1) * tx;
      d.p[(size_t)y * nw + x] = a * (1 - ty) + b * ty;
    }
  }
  return d;
}

struct Hit { int x, y, w, h; float score; };

// Полный перебор с шагом step; grubo, но предсказуемо по времени.
void match(const Gray& img, const Gray& tpl, float minScore, int step, std::vector<Hit>& out) {
  if (tpl.w > img.w || tpl.h > img.h) return;
  double tsum = 0, tsq = 0;
  for (float v : tpl.p) { tsum += v; tsq += (double)v * v; }
  const double n = (double)tpl.p.size();
  const double tmean = tsum / n;
  const double tvar = tsq - n * tmean * tmean;
  if (tvar <= 1e-6) return;  // однотонный шаблон — совпадёт где угодно, отказ
  const double tnorm = std::sqrt(tvar);

  for (int y = 0; y + tpl.h <= img.h; y += step) {
    for (int x = 0; x + tpl.w <= img.w; x += step) {
      double isum = 0, isq = 0, cross = 0;
      for (int ty = 0; ty < tpl.h; ++ty) {
        const float* ip = &img.p[(size_t)(y + ty) * img.w + x];
        const float* tp = &tpl.p[(size_t)ty * tpl.w];
        for (int tx = 0; tx < tpl.w; ++tx) {
          const double iv = ip[tx];
          isum += iv; isq += iv * iv; cross += iv * tp[tx];
        }
      }
      const double imean = isum / n;
      const double ivar = isq - n * imean * imean;
      if (ivar <= 1e-6) continue;
      const double score = (cross - n * imean * tmean) / (std::sqrt(ivar) * tnorm);
      if (score >= minScore) out.push_back({x, y, tpl.w, tpl.h, (float)score});
    }
  }
}

// Non-maximum suppression: одна кнопка не должна вернуться 40 раз.
std::vector<Hit> nms(std::vector<Hit> hits, float iouThresh) {
  std::sort(hits.begin(), hits.end(), [](const Hit& a, const Hit& b) { return a.score > b.score; });
  std::vector<Hit> keep;
  for (const auto& h : hits) {
    bool overlap = false;
    for (const auto& k : keep) {
      int ix = std::max(0, std::min(h.x + h.w, k.x + k.w) - std::max(h.x, k.x));
      int iy = std::max(0, std::min(h.y + h.h, k.y + k.h) - std::max(h.y, k.y));
      float inter = (float)ix * iy;
      float uni = (float)h.w * h.h + (float)k.w * k.h - inter;
      if (uni > 0 && inter / uni > iouThresh) { overlap = true; break; }
    }
    if (!overlap) keep.push_back(h);
  }
  return keep;
}

bool decode_png(const uint8_t* buf, int len, Gray& out) {
  IWICImagingFactory* f = nullptr;
  if (FAILED(::CoCreateInstance(CLSID_WICImagingFactory, nullptr, CLSCTX_INPROC_SERVER, IID_PPV_ARGS(&f))))
    return false;
  IWICStream* stream = nullptr;
  IWICBitmapDecoder* dec = nullptr;
  IWICBitmapFrameDecode* frame = nullptr;
  IWICFormatConverter* conv = nullptr;
  bool ok = false;

  if (SUCCEEDED(f->CreateStream(&stream)) &&
      SUCCEEDED(stream->InitializeFromMemory(const_cast<BYTE*>(buf), (DWORD)len)) &&
      SUCCEEDED(f->CreateDecoderFromStream(stream, nullptr, WICDecodeMetadataCacheOnLoad, &dec)) &&
      SUCCEEDED(dec->GetFrame(0, &frame)) &&
      SUCCEEDED(f->CreateFormatConverter(&conv)) &&
      SUCCEEDED(conv->Initialize(frame, GUID_WICPixelFormat32bppRGBA, WICBitmapDitherTypeNone,
                                 nullptr, 0.0, WICBitmapPaletteTypeCustom))) {
    UINT w = 0, h = 0;
    if (FAILED(conv->GetSize(&w, &h))) { w = 0; h = 0; }
    // Предел шаблона: это картинка кнопки, а не обои. Специально
    // сжатый PNG на 20000×20000 иначе съедает память и время процесса.
    constexpr UINT kMaxTplDim = 4096;
    if (w == 0 || h == 0 || w > kMaxTplDim || h > kMaxTplDim) {
      if (conv) conv->Release();
      if (frame) frame->Release();
      if (dec) dec->Release();
      if (stream) stream->Release();
      f->Release();
      return false;
    }
    std::vector<uint8_t> px((size_t)w * h * 4);
    if (SUCCEEDED(conv->CopyPixels(nullptr, w * 4, (UINT)px.size(), px.data()))) {
      out = to_gray(px.data(), (int)w, (int)h, (int)w * 4);
      ok = true;
    }
  }
  if (conv) conv->Release();
  if (frame) frame->Release();
  if (dec) dec->Release();
  if (stream) stream->Release();
  f->Release();
  return ok;
}

}  // namespace

extern "C" int32_t an_find_template(const an_image* haystack, const uint8_t* png, int32_t png_len,
                                    float min_score, an_rect* out, int32_t max_out, int32_t* found) {
  // found разыменовывался до проверки — на нулевом указателе это падение
  // всего процесса, а не код ошибки.
  if (!found) { an_set_error("find_template: found == null"); return -1; }
  *found = 0;
  if (!haystack || !haystack->data || !png || png_len <= 0) { an_set_error("find_template: пустой вход"); return -1; }
  if (max_out < 0 || (max_out > 0 && !out)) { an_set_error("find_template: out == null"); return -1; }
  if (haystack->width <= 0 || haystack->height <= 0 ||
      haystack->stride < haystack->width * 4) {
    an_set_error("find_template: несогласованный кадр"); return -1;
  }
  Gray tpl;
  if (!decode_png(png, png_len, tpl)) { an_set_error("не удалось декодировать PNG шаблона"); return -2; }
  Gray img = to_gray(haystack->data, haystack->width, haystack->height, haystack->stride);

  std::vector<Hit> all;
  // Масштабы покрывают DPI 100-150% и зум браузера 80-125%.
  const float scales[] = {1.0f, 0.9f, 1.1f, 0.8f, 1.25f, 1.5f};
  for (float s : scales) {
    int tw = (int)std::lround(tpl.w * s), th = (int)std::lround(tpl.h * s);
    if (tw < 6 || th < 6 || tw > img.w || th > img.h) continue;
    Gray st = (s == 1.0f) ? tpl : resize(tpl, tw, th);

    // Грубый проход на 1/4 разрешения — быстро отсекаем пустые области.
    // max(1, ...): для кадра в 1 пиксель деление даёт ноль и деление на ноль в resize.
    Gray simg = resize(img, std::max(1, img.w / 2), std::max(1, img.h / 2));
    Gray stpl = resize(st, std::max(4, tw / 2), std::max(4, th / 2));
    std::vector<Hit> coarse;
    match(simg, stpl, std::max(0.5f, min_score - 0.15f), 2, coarse);

    // Точный проход только вокруг кандидатов.
    for (const auto& c : nms(coarse, 0.3f)) {
      int cx = std::clamp(c.x * 2 - 8, 0, std::max(0, img.w - tw));
      int cy = std::clamp(c.y * 2 - 8, 0, std::max(0, img.h - th));
      int rw = std::min(tw + 16, img.w - cx), rh = std::min(th + 16, img.h - cy);
      Gray roi; roi.w = rw; roi.h = rh; roi.p.resize((size_t)rw * rh);
      for (int y = 0; y < rh; ++y)
        std::copy_n(&img.p[(size_t)(cy + y) * img.w + cx], rw, &roi.p[(size_t)y * rw]);
      std::vector<Hit> fine;
      match(roi, st, min_score, 1, fine);
      for (auto& hh : fine) { hh.x += cx; hh.y += cy; all.push_back(hh); }
    }
  }

  auto keep = nms(all, 0.3f);
  int n = std::min<int>((int)keep.size(), max_out);
  for (int i = 0; i < n; ++i)
    out[i] = an_rect{keep[i].x, keep[i].y, keep[i].w, keep[i].h, keep[i].score};
  *found = n;
  return 0;
}
