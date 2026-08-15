// Внутренние утилиты C++-слоя. В заголовок C ABI это не попадает.
#pragma once
#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>
// WIN32_LEAN_AND_MEAN выкидывает OLE/COM из windows.h, а CoTaskMemAlloc/Free
// нужны в каждом месте, где строка уходит в Rust.
#include <objbase.h>
#include <string>
#include <vector>

// Зачем thread_local: ядро может дёргать зрение и руки из разных потоков
// (перцепция параллельно с действием). Общий буфер ошибки был бы гонкой.
void an_set_error(const std::string& msg);

// Копия строки в кучу процесса, которую вернём в Rust (освобождает an_free).
char* an_dup_cstr(const std::string& s);

std::wstring utf8_to_wide(const char* s);
std::string wide_to_utf8(const std::wstring& w);

// Экранирование для ручной сборки JSON.
// Альтернатива — nlohmann/json, но это +1 зависимость и +секунды сборки
// ради трёх мест, где мы сериализуем плоские структуры.
std::string json_escape(const std::string& s);
