#include "common.hpp"
#include <cstring>

static thread_local std::string g_last_error;

void an_set_error(const std::string& msg) { g_last_error = msg; }

char* an_dup_cstr(const std::string& s) {
  char* p = static_cast<char*>(::CoTaskMemAlloc(s.size() + 1));
  if (!p) return nullptr;
  std::memcpy(p, s.data(), s.size());
  p[s.size()] = '\0';
  return p;
}

std::wstring utf8_to_wide(const char* s) {
  if (!s) return L"";
  int n = ::MultiByteToWideChar(CP_UTF8, 0, s, -1, nullptr, 0);
  std::wstring w(n > 0 ? n - 1 : 0, L'\0');
  if (n > 0) ::MultiByteToWideChar(CP_UTF8, 0, s, -1, w.data(), n);
  return w;
}

std::string wide_to_utf8(const std::wstring& w) {
  if (w.empty()) return "";
  int n = ::WideCharToMultiByte(CP_UTF8, 0, w.c_str(), (int)w.size(), nullptr, 0, nullptr, nullptr);
  std::string s(n, '\0');
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
