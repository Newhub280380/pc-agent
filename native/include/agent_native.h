/*
 * agent_native.h — C ABI между Rust-ядром и C++ "руками и глазами".
 *
 * Зачем именно C ABI, а не C++-интерфейс: у Rust нет стабильного ABI для
 * C++ классов. Плоские extern "C" функции + сырые буферы — единственный
 * способ линковаться без прослоек вроде cxx/autocxx (которые добавляют
 * генерацию кода и время сборки).
 *
 * Соглашение по памяти: любую строку/буфер, который вернула эта библиотека,
 * освобождает ТОЛЬКО an_free(). Rust никогда не вызывает free() сам.
 */
#ifndef AGENT_NATIVE_H
#define AGENT_NATIVE_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct {
  uint8_t* data;   /* RGBA, ширина*высота*4 */
  int32_t width;
  int32_t height;
  int32_t stride;
} an_image;

typedef struct {
  int32_t x, y, w, h;
  float score;      /* 0..1, уверенность совпадения */
} an_rect;

/* ---- жизненный цикл ---- */
int32_t an_init(void);                 /* DPI awareness, COM, WinRT. 0 = ok */
void    an_shutdown(void);
void    an_free(void* ptr);
const char* an_last_error(void);       /* строка последней ошибки, не освобождать */

/* ---- глаза ---- */
/* Скриншот всего виртуального рабочего стола (все мониторы). */
int32_t an_capture_screen(an_image* out);
/* Скриншот одного монитора по индексу. */
int32_t an_capture_monitor(int32_t index, an_image* out);
/* Скриншот конкретного окна по HWND (работает и для перекрытых окон). */
int32_t an_capture_window(uint64_t hwnd, an_image* out);
void    an_free_image(an_image* img);
/* PNG-кодирование для отправки в LLM. Возвращает буфер, освободить an_free. */
int32_t an_encode_png(const an_image* img, uint8_t** out_buf, int32_t* out_len);

/* OCR через Windows.Media.Ocr (встроен в Windows 10/11, внешних зависимостей нет).
 * Возвращает JSON: [{"text":"Купить","x":..,"y":..,"w":..,"h":..,"conf":..}] */
int32_t an_ocr(const an_image* img, const char* lang_bcp47, char** out_json);

/* Поиск шаблона (иконки/кнопки) на скриншоте: многомасштабный NCC.
 * Нужен как быстрый и детерминированный путь, когда элемент уже известен. */
int32_t an_find_template(const an_image* haystack, const uint8_t* png, int32_t png_len,
                         float min_score, an_rect* out, int32_t max_out, int32_t* found);

/* Дерево UI Automation активного/указанного окна в JSON.
 * Зачем: это точные координаты и роли элементов БЕЗ распознавания картинки —
 * на порядок надёжнее и дешевле, чем спрашивать LLM «где кнопка». */
int32_t an_ui_tree(uint64_t hwnd, int32_t max_depth, char** out_json);
/* Поиск элемента по имени/роли через UIA. */
int32_t an_ui_find(const char* name_substr, const char* control_type, an_rect* out);

/* ---- руки ---- */
/* Все действия идут через SendInput на уровне сессии пользователя:
 * для приложения это неотличимо от живой мыши и клавиатуры. */
int32_t an_mouse_move_human(int32_t x, int32_t y, int32_t duration_ms);
int32_t an_mouse_click(int32_t button, int32_t double_click); /* 0=left 1=right 2=middle */
int32_t an_mouse_drag(int32_t x1, int32_t y1, int32_t x2, int32_t y2, int32_t duration_ms);
int32_t an_scroll(int32_t clicks, int32_t horizontal);
int32_t an_type_text_human(const char* utf8, int32_t wpm);
int32_t an_key_combo(const char* combo);    /* "ctrl+shift+t", "enter", "f5" */

/* ---- окружение ---- */
int32_t an_foreground_window(uint64_t* hwnd, char** out_title);
int32_t an_focus_window(uint64_t hwnd);
/* Ищет верхнеуровневое видимое окно по подстроке заголовка (регистронезависимо). */
int32_t an_find_window(const char* title_substr, uint64_t* out_hwnd, char** out_title);
int32_t an_clipboard_get(char** out_utf8);
int32_t an_clipboard_set(const char* utf8);
int32_t an_screen_size(int32_t* w, int32_t* h);
/* Секунды с последнего физического ввода пользователя.
 * Зачем: агент не должен драться с человеком за мышь. */
int32_t an_user_idle_seconds(void);

#ifdef __cplusplus
}
#endif
#endif /* AGENT_NATIVE_H */
