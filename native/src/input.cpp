// "Руки": мышь и клавиатура через SendInput.
//
// Ключевое требование пользователя — режим «Человек». Поэтому здесь НЕТ
// мгновенных телепортов курсора и мгновенного набора текста:
//   - траектория мыши строится кубической кривой Безье со случайными
//     контрольными точками + микродрожание (антидетект по прямым линиям);
//   - скорость по кривой — ease-in-out, как у живой руки;
//   - перед кликом короткая пауза «прицеливания» (реакция человека 80-200мс);
//   - интервалы между нажатиями клавиш логнормальные, а не константа.
//
// Почему SendInput, а не PostMessage/SetCursorPos:
//   PostMessage шлёт сообщение мимо драйвера — античит и антифрод-скрипты
//   видят отсутствие WM_INPUT/RawInput и палят бота. SendInput идёт по тому
//   же пути, что физическое устройство (кроме флага LLMHF_INJECTED, который
//   виден только low-level хукам в том же сеансе).
#include "common.hpp"
#include "../include/agent_native.h"
#include <random>
#include <thread>
#include <chrono>
#include <cmath>
#include <algorithm>

namespace {

std::mt19937& rng() {
  static thread_local std::mt19937 g{std::random_device{}()};
  return g;
}

int rnd(int lo, int hi) { return std::uniform_int_distribution<int>(lo, hi)(rng()); }
double rndf(double lo, double hi) { return std::uniform_real_distribution<double>(lo, hi)(rng()); }

void sleep_ms(int ms) { if (ms > 0) std::this_thread::sleep_for(std::chrono::milliseconds(ms)); }

// Абсолютные координаты SendInput нормируются в 0..65535 по виртуальному экрану.
void send_abs_move(int x, int y) {
  int vx = ::GetSystemMetrics(SM_XVIRTUALSCREEN);
  int vy = ::GetSystemMetrics(SM_YVIRTUALSCREEN);
  int vw = std::max(1, ::GetSystemMetrics(SM_CXVIRTUALSCREEN));
  int vh = std::max(1, ::GetSystemMetrics(SM_CYVIRTUALSCREEN));
  INPUT in{};
  in.type = INPUT_MOUSE;
  in.mi.dwFlags = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;
  in.mi.dx = (LONG)((double)(x - vx) * 65535.0 / (double)(vw - 1));
  in.mi.dy = (LONG)((double)(y - vy) * 65535.0 / (double)(vh - 1));
  ::SendInput(1, &in, sizeof(INPUT));
}

double ease(double t) { return t * t * (3.0 - 2.0 * t); }  // smoothstep

void human_move(int x0, int y0, int x1, int y1, int duration_ms) {
  const double dist = std::hypot(x1 - x0, y1 - y0);
  if (duration_ms <= 0) {
    // Закон Фиттса: время наведения растёт логарифмически от расстояния.
    duration_ms = (int)(120 + 90 * std::log2(1.0 + dist / 40.0)) + rnd(0, 60);
  }
  // Контрольные точки уводим вбок на 5-15% дистанции — рука не ходит по линейке.
  const double off = std::max(6.0, dist * rndf(0.05, 0.15));
  const double ang = rndf(0, 6.28318);
  const double cx1 = x0 + (x1 - x0) * 0.3 + std::cos(ang) * off;
  const double cy1 = y0 + (y1 - y0) * 0.3 + std::sin(ang) * off;
  const double cx2 = x0 + (x1 - x0) * 0.7 + std::cos(ang + 1.7) * off * 0.6;
  const double cy2 = y0 + (y1 - y0) * 0.7 + std::sin(ang + 1.7) * off * 0.6;

  const int steps = std::clamp((int)(duration_ms / 8), 12, 220);
  for (int i = 1; i <= steps; ++i) {
    const double t = ease((double)i / steps);
    const double u = 1 - t;
    double px = u*u*u*x0 + 3*u*u*t*cx1 + 3*u*t*t*cx2 + t*t*t*x1;
    double py = u*u*u*y0 + 3*u*u*t*cy1 + 3*u*t*t*cy2 + t*t*t*y1;
    if (i < steps) {  // финальную точку не дрожим — иначе промахнёмся по кнопке
      px += rndf(-0.8, 0.8);
      py += rndf(-0.8, 0.8);
    }
    send_abs_move((int)std::lround(px), (int)std::lround(py));
    sleep_ms(duration_ms / steps);
  }
}

void tap_vk(WORD vk, bool up) {
  INPUT in{};
  in.type = INPUT_KEYBOARD;
  in.ki.wVk = vk;
  in.ki.dwFlags = up ? KEYEVENTF_KEYUP : 0;
  ::SendInput(1, &in, sizeof(INPUT));
}

// Юникод-символ без раскладки: KEYEVENTF_UNICODE.
// Зачем: иначе кириллица/эмодзи зависят от текущего языка ввода — классический
// баг ботов, которые печатают "ghbdtn" вместо "привет".
void send_unicode(wchar_t ch) {
  INPUT in[2]{};
  in[0].type = INPUT_KEYBOARD;
  in[0].ki.wScan = ch;
  in[0].ki.dwFlags = KEYEVENTF_UNICODE;
  in[1] = in[0];
  in[1].ki.dwFlags |= KEYEVENTF_KEYUP;
  ::SendInput(2, in, sizeof(INPUT));
}

WORD vk_from_name(const std::string& n) {
  if (n.size() == 1) {
    char c = (char)toupper(n[0]);
    if ((c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9')) return (WORD)c;
  }
  static const struct { const char* k; WORD v; } table[] = {
    {"ctrl", VK_CONTROL}, {"control", VK_CONTROL}, {"alt", VK_MENU},
    {"shift", VK_SHIFT}, {"win", VK_LWIN}, {"enter", VK_RETURN}, {"return", VK_RETURN},
    {"tab", VK_TAB}, {"esc", VK_ESCAPE}, {"escape", VK_ESCAPE}, {"space", VK_SPACE},
    {"backspace", VK_BACK}, {"delete", VK_DELETE}, {"home", VK_HOME}, {"end", VK_END},
    {"pageup", VK_PRIOR}, {"pagedown", VK_NEXT}, {"up", VK_UP}, {"down", VK_DOWN},
    {"left", VK_LEFT}, {"right", VK_RIGHT}, {"insert", VK_INSERT}, {"printscreen", VK_SNAPSHOT},
    {"f1", VK_F1}, {"f2", VK_F2}, {"f3", VK_F3}, {"f4", VK_F4}, {"f5", VK_F5}, {"f6", VK_F6},
    {"f7", VK_F7}, {"f8", VK_F8}, {"f9", VK_F9}, {"f10", VK_F10}, {"f11", VK_F11}, {"f12", VK_F12},
  };
  for (auto& e : table) if (n == e.k) return e.v;
  return 0;
}

}  // namespace

extern "C" int32_t an_mouse_move_human(int32_t x, int32_t y, int32_t duration_ms) {
  POINT p{};
  ::GetCursorPos(&p);
  human_move(p.x, p.y, x, y, duration_ms);
  return 0;
}

extern "C" int32_t an_mouse_click(int32_t button, int32_t double_click) {
  DWORD down = MOUSEEVENTF_LEFTDOWN, up = MOUSEEVENTF_LEFTUP;
  if (button == 1) { down = MOUSEEVENTF_RIGHTDOWN; up = MOUSEEVENTF_RIGHTUP; }
  else if (button == 2) { down = MOUSEEVENTF_MIDDLEDOWN; up = MOUSEEVENTF_MIDDLEUP; }

  sleep_ms(rnd(60, 180));  // «прицеливание» перед кликом
  const int clicks = double_click ? 2 : 1;
  for (int i = 0; i < clicks; ++i) {
    INPUT in{}; in.type = INPUT_MOUSE;
    in.mi.dwFlags = down; ::SendInput(1, &in, sizeof(INPUT));
    sleep_ms(rnd(40, 110));  // время удержания кнопки у человека 50-120мс
    in.mi.dwFlags = up; ::SendInput(1, &in, sizeof(INPUT));
    if (i == 0 && clicks == 2) sleep_ms(rnd(60, 110));
  }
  return 0;
}

extern "C" int32_t an_mouse_drag(int32_t x1, int32_t y1, int32_t x2, int32_t y2, int32_t duration_ms) {
  an_mouse_move_human(x1, y1, duration_ms / 2);
  INPUT in{}; in.type = INPUT_MOUSE;
  in.mi.dwFlags = MOUSEEVENTF_LEFTDOWN; ::SendInput(1, &in, sizeof(INPUT));
  sleep_ms(rnd(80, 160));
  human_move(x1, y1, x2, y2, duration_ms / 2);
  sleep_ms(rnd(60, 140));
  in.mi.dwFlags = MOUSEEVENTF_LEFTUP; ::SendInput(1, &in, sizeof(INPUT));
  return 0;
}

extern "C" int32_t an_scroll(int32_t clicks, int32_t horizontal) {
  // Скроллим порциями по одному «щелчку» с паузой: колесо мыши физически
  // не может отдать 10 щелчков за 1мс, а такие пакеты — маркер бота.
  const int step = clicks > 0 ? 1 : -1;
  for (int i = 0; i != clicks; i += step) {
    INPUT in{}; in.type = INPUT_MOUSE;
    in.mi.dwFlags = horizontal ? MOUSEEVENTF_HWHEEL : MOUSEEVENTF_WHEEL;
    in.mi.mouseData = (DWORD)(step * WHEEL_DELTA);
    ::SendInput(1, &in, sizeof(INPUT));
    sleep_ms(rnd(35, 90));
  }
  return 0;
}

extern "C" int32_t an_type_text_human(const char* utf8, int32_t wpm) {
  std::wstring w = utf8_to_wide(utf8);
  if (wpm <= 0) wpm = 240;  // ~240 зн/мин: быстрый, но реальный человек
  const int base = std::max(15, 60000 / std::max(60, wpm * 5));
  for (size_t i = 0; i < w.size(); ++i) {
    if (w[i] == L'\n') { tap_vk(VK_RETURN, false); tap_vk(VK_RETURN, true); }
    else send_unicode(w[i]);
    int d = base + rnd(-base / 3, base);
    // Люди делают микропаузы после пробелов и знаков препинания.
    if (w[i] == L' ' || w[i] == L'.' || w[i] == L',') d += rnd(20, 90);
    if (rnd(0, 40) == 0) d += rnd(150, 500);  // «задумался»
    sleep_ms(d);
  }
  return 0;
}

extern "C" int32_t an_key_combo(const char* combo) {
  std::string s = combo ? combo : "";
  std::transform(s.begin(), s.end(), s.begin(), ::tolower);
  std::vector<WORD> keys;
  size_t pos = 0;
  while (pos <= s.size()) {
    size_t plus = s.find('+', pos);
    std::string part = s.substr(pos, plus == std::string::npos ? std::string::npos : plus - pos);
    if (!part.empty()) {
      WORD vk = vk_from_name(part);
      if (!vk) { an_set_error("неизвестная клавиша: " + part); return -1; }
      keys.push_back(vk);
    }
    if (plus == std::string::npos) break;
    pos = plus + 1;
  }
  for (WORD k : keys) { tap_vk(k, false); sleep_ms(rnd(15, 45)); }
  sleep_ms(rnd(30, 80));
  for (auto it = keys.rbegin(); it != keys.rend(); ++it) { tap_vk(*it, true); sleep_ms(rnd(10, 30)); }
  return 0;
}
