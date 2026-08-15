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
  HWND h = ::GetForegroundWindow();
  if (!h) { an_set_error("нет активного окна"); return -1; }
  *hwnd = reinterpret_cast<uint64_t>(h);
  wchar_t buf[512]{};
  ::GetWindowTextW(h, buf, 511);
  *out_title = an_dup_cstr(wide_to_utf8(buf));
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
  FindCtx ctx;
  ctx.needle = lower_w(utf8_to_wide(title_substr));
  ::EnumWindows(&enum_proc, reinterpret_cast<LPARAM>(&ctx));
  if (!ctx.hit) { an_set_error("окно с таким заголовком не найдено"); return -1; }
  *out_hwnd = reinterpret_cast<uint64_t>(ctx.hit);
  *out_title = an_dup_cstr(wide_to_utf8(ctx.title));
  return 0;
}

extern "C" int32_t an_clipboard_get(char** out_utf8) {
  if (!::OpenClipboard(nullptr)) { an_set_error("буфер обмена занят"); return -1; }
  HANDLE h = ::GetClipboardData(CF_UNICODETEXT);
  if (!h) { ::CloseClipboard(); *out_utf8 = an_dup_cstr(""); return 0; }
  auto* p = static_cast<wchar_t*>(::GlobalLock(h));
  std::string s = p ? wide_to_utf8(p) : "";
  ::GlobalUnlock(h);
  ::CloseClipboard();
  *out_utf8 = an_dup_cstr(s);
  return 0;
}

extern "C" int32_t an_clipboard_set(const char* utf8) {
  std::wstring w = utf8_to_wide(utf8);
  if (!::OpenClipboard(nullptr)) { an_set_error("буфер обмена занят"); return -1; }
  ::EmptyClipboard();
  const size_t bytes = (w.size() + 1) * sizeof(wchar_t);
  HGLOBAL g = ::GlobalAlloc(GMEM_MOVEABLE, bytes);
  if (!g) { ::CloseClipboard(); an_set_error("OOM"); return -2; }
  memcpy(::GlobalLock(g), w.c_str(), bytes);
  ::GlobalUnlock(g);
  ::SetClipboardData(CF_UNICODETEXT, g);  // владение переходит системе
  ::CloseClipboard();
  return 0;
}

extern "C" int32_t an_screen_size(int32_t* w, int32_t* h) {
  *w = ::GetSystemMetrics(SM_CXVIRTUALSCREEN);
  *h = ::GetSystemMetrics(SM_CYVIRTUALSCREEN);
  return 0;
}

extern "C" int32_t an_user_idle_seconds(void) {
  LASTINPUTINFO li{sizeof(LASTINPUTINFO), 0};
  if (!::GetLastInputInfo(&li)) return -1;
  return (int32_t)((::GetTickCount64() - li.dwTime) / 1000);
}
