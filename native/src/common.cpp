#include "common.hpp"
#include <cstring>

static thread_local std::string g_last_error;

void an_set_error(const std::string& msg) { g_last_error = msg; }

char* an_dup_cstr(const std::string& s) {
  // Потолок: всё, что уходит в Rust через C ABI, — это JSON дерева UI
  // или текст OCR; 64 МиБ заведомо больше любого здравого случая.
  if (s.size() > (64u << 20)) return nullptr;
  char* p = static_cast<char*>(::CoTaskMemAlloc(s.size() + 1));
  if (!p) return nullptr;
  std::memcpy(p, s.data(), s.size());
  p[s.size()] = '\0';
  return p;
}

std::wstring utf8_to_wide(const char* s) {
  if (!s) return L"";
  // MB_ERR_INVALID_CHARS: битый UTF-8 от модели лучше отклонить, чем тихо
  // превратить в U+FFFD и напечатать мусор в чужую форму.
  int n = ::MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, s, -1, nullptr, 0);
  if (n <= 1) return L"";  // 1 = только терминатор, <=0 = ошибка
  // Буфер ровно на n символов вместе с \0: раньше WinAPI писала n символов
  // в строку длиной n-1, то есть за конец собственного буфера.
  std::wstring w(static_cast<size_t>(n), L'\0');
  int got = ::MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, s, -1, w.data(), n);
  if (got <= 0) return L"";
  w.resize(static_cast<size_t>(got) - 1);  // срезаем терминатор
  return w;
}

std::string wide_to_utf8(const std::wstring& w) {
  if (w.empty()) return "";
  int n = ::WideCharToMultiByte(CP_UTF8, 0, w.c_str(), (int)w.size(), nullptr, 0, nullptr, nullptr);
  if (n <= 0) return "";
  std::string s(static_cast<size_t>(n), '\0');
  ::WideCharToMultiByte(CP_UTF8, 0, w.c_str(), (int)w.size(), s.data(), n, nullptr, nullptr);
  return s;
}

std::string json_escape(const std::string& s) {
  std::string o;
  o.reserve(s.size() + 8);
  for (unsigned char c : s) {
    switch (c) {
      case '"':  o += "\\\""; break;
      case '\\': o += "\\\\"; break;
      case '\n': o += "\\n";  break;
      case '\r': o += "\\r";  break;
      case '\t': o += "\\t";  break;
      default:
        if (c < 0x20) { char b[8]; sprintf_s(b, "\\u%04x", c); o += b; }
        else o += (char)c;
    }
  }
  return o;
}

extern "C" const char* an_last_error() { return g_last_error.c_str(); }
extern "C" void an_free(void* ptr) { if (ptr) ::CoTaskMemFree(ptr); }
