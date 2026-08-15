// OCR через Windows.Media.Ocr (C++/WinRT).
//
// ВАЖНОЕ РЕШЕНИЕ: движок OCR уже встроен в Windows 10/11 — не нужен ни
// Tesseract, ни PaddleOCR, ни скачивание моделей. Это прямо поддерживает
// требование «1 .exe, запустил и поехал»: внешних файлов ноль.
// Языки берутся из установленных языковых пакетов Windows (ru-RU/en-US
// обычно уже стоят). Если языка нет — падаем на первый доступный.
//
// Альтернативы и когда они лучше:
//   - Tesseract: больше языков, но +30 МБ данных и заметно медленнее;
//   - PaddleOCR/ONNX: точнее на мелком шрифте и скриншотах с шумом,
//     но требует рантайма и GPU для скорости → кандидат в v2 как плагин.
#include "common.hpp"
#include "../include/agent_native.h"

#include <winrt/base.h>
#include <winrt/Windows.Foundation.h>
#include <winrt/Windows.Foundation.Collections.h>
#include <winrt/Windows.Globalization.h>
#include <winrt/Windows.Graphics.Imaging.h>
#include <winrt/Windows.Media.Ocr.h>
#include <winrt/Windows.Storage.Streams.h>
#include <string>
#include <vector>

// IMemoryBufferByteAccess нет ни в одном публичном заголовке C++/WinRT:
// Microsoft в документации предписывает объявлять её вручную. robuffer.h
// содержит только IBufferByteAccess (для IBuffer), а нам нужен доступ
// к сырым байтам IMemoryBufferReference от BitmapBuffer.
MIDL_INTERFACE("5b0d3235-4dba-4d44-865e-8f1d0e4fd04d")
IMemoryBufferByteAccessLocal : ::IUnknown {
  virtual HRESULT STDMETHODCALLTYPE GetBuffer(BYTE** value, UINT32* capacity) = 0;
};

using namespace winrt;
using namespace winrt::Windows::Graphics::Imaging;
using namespace winrt::Windows::Media::Ocr;
using namespace winrt::Windows::Globalization;
using namespace winrt::Windows::Storage::Streams;

namespace {

// Копируем RGBA в SoftwareBitmap BGRA8 — единственный формат, который
// гарантированно принимает OcrEngine.
SoftwareBitmap make_bitmap(const an_image* img) {
  SoftwareBitmap bmp(BitmapPixelFormat::Bgra8, img->width, img->height, BitmapAlphaMode::Premultiplied);
  {
    BitmapBuffer buffer = bmp.LockBuffer(BitmapBufferAccessMode::Write);
    auto ref = buffer.CreateReference();
    auto access = ref.as<IMemoryBufferByteAccessLocal>();
    uint8_t* dst = nullptr;
    uint32_t cap = 0;
    check_hresult(access->GetBuffer(&dst, &cap));
    auto desc = buffer.GetPlaneDescription(0);
    for (int y = 0; y < img->height; ++y) {
      const uint8_t* srow = img->data + (size_t)y * img->stride;
      uint8_t* drow = dst + desc.StartIndex + (size_t)y * desc.Stride;
      for (int x = 0; x < img->width; ++x) {
        drow[x * 4 + 0] = srow[x * 4 + 2];  // B
        drow[x * 4 + 1] = srow[x * 4 + 1];  // G
        drow[x * 4 + 2] = srow[x * 4 + 0];  // R
        drow[x * 4 + 3] = srow[x * 4 + 3];  // A
      }
    }
  }
  return bmp;
}

}  // namespace

extern "C" int32_t an_ocr(const an_image* img, const char* lang_bcp47, char** out_json) {
  if (!img || !img->data || !out_json) { an_set_error("ocr: пустое изображение или out == null"); return -1; }
  // stride меньше строки — это чтение за концом буфера в make_bitmap.
  if (img->width <= 0 || img->height <= 0 || img->stride < img->width * 4) {
    an_set_error("ocr: несогласованные размеры кадра"); return -1;
  }
  try {
    OcrEngine engine{nullptr};
    if (lang_bcp47 && *lang_bcp47) {
      Language lang{utf8_to_wide(lang_bcp47)};
      if (OcrEngine::IsLanguageSupported(lang)) engine = OcrEngine::TryCreateFromLanguage(lang);
    }
    if (!engine) engine = OcrEngine::TryCreateFromUserProfileLanguages();
    if (!engine) {
      // Не молчим: ядро покажет это пользователю и попросит поставить
      // языковой пакет — «просит подключить инструмент», а не выдумывает.
      an_set_error("OCR недоступен: установи языковой пакет Windows (Параметры → Язык → Добавить язык, галочка 'Распознавание текста')");
      return -2;
    }

    auto result = engine.RecognizeAsync(make_bitmap(img)).get();
    std::string json = "[";
    bool first = true;
    for (auto const& line : result.Lines()) {
      for (auto const& word : line.Words()) {
        auto r = word.BoundingRect();
        if (!first) json += ",";
        first = false;
        json += "{\"text\":\"" + json_escape(wide_to_utf8(std::wstring(word.Text()))) + "\"" +
                ",\"x\":" + std::to_string((int)r.X) +
                ",\"y\":" + std::to_string((int)r.Y) +
                ",\"w\":" + std::to_string((int)r.Width) +
                ",\"h\":" + std::to_string((int)r.Height) +
                ",\"line\":\"" + json_escape(wide_to_utf8(std::wstring(line.Text()))) + "\"}";
      }
    }
    json += "]";
    char* dup = an_dup_cstr(json);
    if (!dup) { an_set_error("OOM"); return -4; }
    *out_json = dup;
    return 0;
  } catch (hresult_error const& e) {
    an_set_error("ocr winrt: " + wide_to_utf8(std::wstring(e.message())));
    return -3;
  } catch (std::exception const& e) {
    // Любое исключение, пересекающее C ABI, — это UB и падение процесса.
    an_set_error(std::string("ocr: ") + e.what());
    return -3;
  } catch (...) {
    an_set_error("ocr: неизвестная ошибка");
    return -3;
  }
}
