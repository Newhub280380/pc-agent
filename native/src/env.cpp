// Окружение: инициализация, окна, буфер обмена, простой пользователя.
#include "common.hpp"
#include "../include/agent_native.h"
#include <shellscalingapi.h>

int32_t uia_init();
void uia_shutdown();

extern "C" int32_t an_init(void) {
  // Apartment-threaded COM: требование UIA и WIC.
  HRESULT hr = ::CoInitializeEx(nullptr, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
  if (FAILED(hr) && hr != RPC_E_CHANGED_MODE) { an_set_error("CoInitializeEx failed"); return -1; }

  // Per-Monitor-V2 DPI: без этого на 150% масштабе Windows врёт координаты —
  // скриншот приходит растянутым, а клики уезжают на десятки пикселей.
  // Это одна из главных причин, почему «бот кликает мимо» у конкурентов.
  ::SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

  uia_init();  // не фатально: без UIA работаем на зрении
  return 0;
}

extern "C" void an_shutdown(void) {
  uia_shutdown();
  ::CoUninitialize();
}

extern "C" int32_t an_foreground_window(uint64_t* hwnd, char** out_title) {
  if (!hwnd || !out_title) { an_set_error("foreground_window: out == null"); return -1; }
  HWND h = ::GetForegroundWindow();
  if (!h) { an_set_error("нет активного окна"); return -1; }
  wchar_t buf[512]{};
  // Предпоследний аргумент — размер буфера ВМЕСТЕ с \0, поэтому честный
  // ARRAYSIZE, а не магическое 511.
  int n = ::GetWindowTextW(h, buf, ARRAYSIZE(buf));
  char* title = an_dup_cstr(wide_to_utf8(std::wstring(buf, buf + (n > 0 ? n : 0))));
  if (!title) { an_set_error("OOM"); return -2; }
  *hwnd = reinterpret_cast<uint64_t>(h);
  *out_title = title;
  return 0;
}

extern "C" int32_t an_focus_window(uint64_t hwnd) {
  HWND h = reinterpret_cast<HWND>(hwnd);
  if (!::IsWindow(h)) { an_set_error("невалидный HWND"); return -1; }
  if (::IsIconic(h)) ::ShowWindow(h, SW_RESTORE);
  // Windows блокирует SetForegroundWindow из фонового процесса. Обход:
  // временно привязываемся к потоку активного окна. Это то же, что делают
  // легитимные утилиты (AltTab-менеджеры), никаких хаков ядра.
  DWORD fgTid = ::GetWindowThreadProcessId(::GetForegroundWindow(), nullptr);
  DWORD myTid = ::GetCurrentThreadId();
  ::AttachThreadInput(myTid, fgTid, TRUE);
  ::SetForegroundWindow(h);
  ::BringWindowToTop(h);
  ::AttachThreadInput(myTid, fgTid, FALSE);
  return 0;
}

namespace {
struct FindCtx {
  std::wstring needle;   // уже в нижнем регистре
  HWND hit = nullptr;
  std::wstring title;
};

std::wstring lower_w(std::wstring s) {
  for (auto& c : s) c = (wchar_t)::towlower(c);
  return s;
}

BOOL CALLBACK enum_proc(HWND h, LPARAM lp) {
  auto* ctx = reinterpret_cast<FindCtx*>(lp);
  // Только реальные окна: невидимые и служебные (tool windows) пропускаем,
  // иначе агент «фокусируется» на скрытом окне и думает, что всё хорошо.
  if (!::IsWindowVisible(h)) return TRUE;
  if (::GetWindow(h, GW_OWNER) != nullptr) return TRUE;
  if (::GetWindowLongPtrW(h, GWL_EXSTYLE) & WS_EX_TOOLWINDOW) return TRUE;
  wchar_t buf[512]{};
  int n = ::GetWindowTextW(h, buf, 511);
  if (n <= 0) return TRUE;
  std::wstring title(buf, buf + n);
  if (lower_w(title).find(ctx->needle) == std::wstring::npos) return TRUE;
  ctx->hit = h;
  ctx->title = title;
  return FALSE;  // первое совпадение — обычно самое верхнее в Z-порядке
}
}  // namespace

// Поиск окна по подстроке заголовка. Нужен, чтобы агент мог переключаться
// между браузером, мессенджером и Excel, а не только «видеть» активное окно.
// Альтернатива — обход UIA-дерева рабочего стола, но он в разы медленнее
// (десятки мс против сотен микросекунд) и требует COM в каждом потоке.
extern "C" int32_t an_find_window(const char* title_substr, uint64_t* out_hwnd, char** out_title) {
  if (!out_hwnd || !out_title) { an_set_error("find_window: out == null"); return -1; }
  FindCtx ctx;
  ctx.needle = lower_w(utf8_to_wide(title_substr));
  ::EnumWindows(&enum_proc, reinterpret_cast<LPARAM>(&ctx));
  if (!ctx.hit) { an_set_error("окно с таким заголовком не найдено"); return -1; }
  char* title = an_dup_cstr(wide_to_utf8(ctx.title));
  if (!title) { an_set_error("OOM"); return -2; }
  *out_hwnd = reinterpret_cast<uint64_t>(ctx.hit);
  *out_title = title;
  return 0;
}

extern "C" int32_t an_clipboard_get(char** out_utf8) {
  if (!out_utf8) { an_set_error("clipboard_get: out == null"); return -1; }
  if (!::OpenClipboard(nullptr)) { an_set_error("буфер обмена занят"); return -1; }
  HANDLE h = ::GetClipboardData(CF_UNICODETEXT);
  std::string s;
  if (h) {
    auto* p = static_cast<wchar_t*>(::GlobalLock(h));
    if (p) {
      // Ограничиваем длину размером самого блока: чужое приложение могло
      // положить в буфер текст без терминатора.
      const size_t cap = ::GlobalSize(h) / sizeof(wchar_t);
      size_t len = 0;
      while (len < cap && p[len] != L'\0') ++len;
      s = wide_to_utf8(std::wstring(p, p + len));
      ::GlobalUnlock(h);
    }
  }
  ::CloseClipboard();
  char* dup = an_dup_cstr(s);
  if (!dup) { an_set_error("OOM"); return -2; }
  *out_utf8 = dup;
  return 0;
}

extern "C" int32_t an_clipboard_set(const char* utf8) {
  std::wstring w = utf8_to_wide(utf8);
  if (!::OpenClipboard(nullptr)) { an_set_error("буфер обмена занят"); return -1; }
  ::EmptyClipboard();
  const size_t bytes = (w.size() + 1) * sizeof(wchar_t);
  HGLOBAL g = ::GlobalAlloc(GMEM_MOVEABLE, bytes);
  if (!g) { ::CloseClipboard(); an_set_error("OOM"); return -2; }
  void* dst = ::GlobalLock(g);
  if (!dst) { ::GlobalFree(g); ::CloseClipboard(); an_set_error("GlobalLock failed"); return -3; }
  memcpy(dst, w.c_str(), bytes);
  ::GlobalUnlock(g);
  // Владение переходит системе ТОЛЬКО при успехе; иначе память наша
  // и её нужно освободить — иначе течь на каждой неудачной вставке.
  if (!::SetClipboardData(CF_UNICODETEXT, g)) {
    ::GlobalFree(g);
    ::CloseClipboard();
    an_set_error("SetClipboardData failed");
    return -4;
  }
  ::CloseClipboard();
  return 0;
}

extern "C" int32_t an_screen_size(int32_t* w, int32_t* h) {
  if (!w || !h) { an_set_error("screen_size: out == null"); return -1; }
  *w = ::GetSystemMetrics(SM_CXVIRTUALSCREEN);
  *h = ::GetSystemMetrics(SM_CYVIRTUALSCREEN);
  return 0;
}

extern "C" int32_t an_user_idle_seconds(void) {
  LASTINPUTINFO li{sizeof(LASTINPUTINFO), 0};
  if (!::GetLastInputInfo(&li)) return -1;
  return (int32_t)((::GetTickCount64() - li.dwTime) / 1000);
}
