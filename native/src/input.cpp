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

// Навигационные и редакторские клавиши — «расширенные» (E0-префикс на
// физической клавиатуре). Без KEYEVENTF_EXTENDEDKEY SendInput отдаёт их
// как цифровой блок (VK_LEFT ≡ Num4 и т. д.): приложения видят Numpad-стрелку,
// и Shift+стрелка не выделяет текст, а просто двигает курсор
// (vercel-labs/agent-desktop#513). Ctrl/Alt/Shift правые тоже extended,
// но мы шлём левые.
bool is_extended_vk(WORD vk) {
  switch (vk) {
    case VK_LEFT: case VK_RIGHT: case VK_UP: case VK_DOWN:
    case VK_HOME: case VK_END: case VK_PRIOR: case VK_NEXT:
    case VK_INSERT: case VK_DELETE: case VK_DIVIDE: case VK_NUMLOCK:
    case VK_SNAPSHOT: case VK_RWIN: case VK_LWIN: case VK_APPS:
    case VK_RCONTROL: case VK_RMENU:
      return true;
    default:
      return false;
  }
}

void tap_vk(WORD vk, bool up) {
  INPUT in{};
  in.type = INPUT_KEYBOARD;
  in.ki.wVk = vk;
  // Скан-код обязателен: часть приложений (игры, Java/Swing, RDP-клиент)
  // читает именно его, а с wScan=0 клавиша для них «пустая».
  in.ki.wScan = (WORD)::MapVirtualKeyW(vk, MAPVK_VK_TO_VSC);
  in.ki.dwFlags = up ? KEYEVENTF_KEYUP : 0;
  if (is_extended_vk(vk)) in.ki.dwFlags |= KEYEVENTF_EXTENDEDKEY;
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

// Результат разбора одного имени клавиши: VK плюс модификаторы, которые
// это имя подразумевает само (`(` = Shift+9 на US-раскладке).
struct KeySpec {
  WORD vk = 0;
  bool shift = false, ctrl = false, alt = false;
};

// Символ -> VK через текущую раскладку. VkKeyScanW отдаёт в младшем байте VK,
// в старшем — нужные модификаторы (1=Shift, 2=Ctrl, 4=Alt). -1 = символа в
// раскладке нет (например, кириллица на US) — тогда клавиша недоступна как
// комбинация; для текста есть type_text с KEYEVENTF_UNICODE.
bool vk_from_char(wchar_t ch, KeySpec* out) {
  SHORT r = ::VkKeyScanW(ch);
  if (r == -1) return false;
  out->vk = (WORD)(r & 0xFF);
  const int mods = (r >> 8) & 0xFF;
  out->shift = (mods & 1) != 0;
  out->ctrl = (mods & 2) != 0;
  out->alt = (mods & 4) != 0;
  return out->vk != 0;
}

bool vk_from_name(const std::string& n, KeySpec* out) {
  *out = KeySpec{};
  if (n.size() == 1) {
    char c = (char)toupper((unsigned char)n[0]);
    if ((c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9')) { out->vk = (WORD)c; return true; }
    // Любой одиночный ASCII-символ: `;`, `\`, `(`, `'`, `,`, `-`, `=`, `/` …
    // Раньше всё это было «неизвестная клавиша» (ср. agent-desktop#513), и
    // в приложения без UIA-дерева (PyCharm/Swing) двоеточие было не набрать.
    return vk_from_char((wchar_t)(unsigned char)n[0], out);
  }
  static const struct { const char* k; WORD v; } table[] = {
    {"ctrl", VK_CONTROL}, {"control", VK_CONTROL}, {"alt", VK_MENU}, {"menu", VK_MENU},
    {"shift", VK_SHIFT}, {"win", VK_LWIN}, {"super", VK_LWIN}, {"cmd", VK_LWIN}, {"meta", VK_LWIN},
    {"enter", VK_RETURN}, {"return", VK_RETURN},
    {"tab", VK_TAB}, {"esc", VK_ESCAPE}, {"escape", VK_ESCAPE}, {"space", VK_SPACE},
    {"backspace", VK_BACK}, {"delete", VK_DELETE}, {"del", VK_DELETE}, {"forwarddelete", VK_DELETE},
    {"home", VK_HOME}, {"end", VK_END},
    {"pageup", VK_PRIOR}, {"pgup", VK_PRIOR}, {"pagedown", VK_NEXT}, {"pgdn", VK_NEXT},
    {"up", VK_UP}, {"down", VK_DOWN}, {"left", VK_LEFT}, {"right", VK_RIGHT},
    {"arrowup", VK_UP}, {"arrowdown", VK_DOWN}, {"arrowleft", VK_LEFT}, {"arrowright", VK_RIGHT},
    {"insert", VK_INSERT}, {"ins", VK_INSERT}, {"printscreen", VK_SNAPSHOT}, {"prtsc", VK_SNAPSHOT},
    {"capslock", VK_CAPITAL}, {"numlock", VK_NUMLOCK}, {"scrolllock", VK_SCROLL}, {"pause", VK_PAUSE},
    {"apps", VK_APPS}, {"contextmenu", VK_APPS},
    // Знаки препинания по именам — тот же словарь, что у macOS-адаптера
    // agent-desktop, чтобы один и тот же план работал на обеих платформах.
    {"comma", VK_OEM_COMMA}, {"period", VK_OEM_PERIOD}, {"dot", VK_OEM_PERIOD},
    {"semicolon", VK_OEM_1}, {"quote", VK_OEM_7}, {"apostrophe", VK_OEM_7},
    {"slash", VK_OEM_2}, {"backslash", VK_OEM_5}, {"minus", VK_OEM_MINUS}, {"dash", VK_OEM_MINUS},
    {"equal", VK_OEM_PLUS}, {"equals", VK_OEM_PLUS}, {"plus", VK_OEM_PLUS},
    {"leftbracket", VK_OEM_4}, {"bracketleft", VK_OEM_4}, {"rightbracket", VK_OEM_6}, {"bracketright", VK_OEM_6},
    {"grave", VK_OEM_3}, {"backquote", VK_OEM_3}, {"tilde", VK_OEM_3},
    {"f1", VK_F1}, {"f2", VK_F2}, {"f3", VK_F3}, {"f4", VK_F4}, {"f5", VK_F5}, {"f6", VK_F6},
    {"f7", VK_F7}, {"f8", VK_F8}, {"f9", VK_F9}, {"f10", VK_F10}, {"f11", VK_F11}, {"f12", VK_F12},
    {"f13", VK_F13}, {"f14", VK_F14}, {"f15", VK_F15}, {"f16", VK_F16},
    {"numpad0", VK_NUMPAD0}, {"numpad1", VK_NUMPAD1}, {"numpad2", VK_NUMPAD2}, {"numpad3", VK_NUMPAD3},
    {"numpad4", VK_NUMPAD4}, {"numpad5", VK_NUMPAD5}, {"numpad6", VK_NUMPAD6}, {"numpad7", VK_NUMPAD7},
    {"numpad8", VK_NUMPAD8}, {"numpad9", VK_NUMPAD9},
    {"multiply", VK_MULTIPLY}, {"add", VK_ADD}, {"subtract", VK_SUBTRACT}, {"decimal", VK_DECIMAL}, {"divide", VK_DIVIDE},
    {"volumeup", VK_VOLUME_UP}, {"volumedown", VK_VOLUME_DOWN}, {"volumemute", VK_VOLUME_MUTE},
  };
  for (auto& e : table) if (n == e.k) { out->vk = e.v; return true; }
  // Имя вида "plus" уже обработано; символ "+" как разделитель не приходит
  // сюда вовсе — an_key_combo принимает его как "plus".
  return false;
}

}  // namespace

extern "C" int32_t an_mouse_move_human(int32_t x, int32_t y, int32_t duration_ms) {
  POINT p{};
  ::GetCursorPos(&p);
  // Координаты приходят от модели и бывают любыми. Ограничиваем виртуальным
  // экраном: иначе hypot на INT_MAX даёт огромный duration и агент встаёт.
  const int vx = ::GetSystemMetrics(SM_XVIRTUALSCREEN);
  const int vy = ::GetSystemMetrics(SM_YVIRTUALSCREEN);
  const int vw = std::max(1, ::GetSystemMetrics(SM_CXVIRTUALSCREEN));
  const int vh = std::max(1, ::GetSystemMetrics(SM_CYVIRTUALSCREEN));
  x = std::clamp(x, vx, vx + vw - 1);
  y = std::clamp(y, vy, vy + vh - 1);
  duration_ms = std::clamp(duration_ms, 0, 10000);
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
  // Лимит дублируется в Rust, но C ABI должен быть безопасен сам по себе:
  // clicks = 100000 — это час зависания потока.
  clicks = std::clamp(clicks, -50, 50);
  if (clicks == 0) return 0;
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
  if (!utf8) { an_set_error("type_text: null"); return -1; }
  std::wstring w = utf8_to_wide(utf8);
  // Потолок на длину: при человеческой скорости 100k символов — это часы
  // забитой клавиатуры, и остановить это можно только убив процесс.
  if (w.size() > 20000) { an_set_error("type_text: слишком длинный текст"); return -1; }
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
  // Пробелы вокруг «+» и регистр — не ошибка модели.
  s.erase(std::remove_if(s.begin(), s.end(), [](unsigned char c) { return c == ' ' || c == '\t'; }), s.end());
  std::transform(s.begin(), s.end(), s.begin(), [](unsigned char c) { return (char)::tolower(c); });
  if (s.empty()) { an_set_error("пустая комбинация клавиш"); return -1; }

  // Разбор по «+», но сам «+» как клавиша тоже допустим: "ctrl++" и "shift+="
  // раньше распадались на пустые части и падали. Правило: пустая часть,
  // за которой стоит ещё один «+», — это клавиша «+».
  std::vector<std::string> parts;
  {
    std::string cur;
    for (size_t i = 0; i < s.size(); ++i) {
      if (s[i] == '+') {
        if (cur.empty() && i + 1 < s.size() && s[i + 1] == '+') { parts.push_back("+"); ++i; continue; }
        if (cur.empty() && i + 1 == s.size()) { parts.push_back("+"); break; }
        if (!cur.empty()) { parts.push_back(cur); cur.clear(); }
        continue;
      }
      cur += s[i];
    }
    if (!cur.empty()) parts.push_back(cur);
  }
  if (parts.empty()) { an_set_error("пустая комбинация клавиш"); return -1; }

  // Модификаторы отдельно от основных клавиш: порядок в строке у модели
  // бывает любой ("a+ctrl"), а нажимать нужно модификаторы первыми.
  bool shift = false, ctrl = false, alt = false, win = false;
  std::vector<WORD> keys;
  for (const std::string& part : parts) {
    KeySpec ks;
    if (!vk_from_name(part, &ks)) { an_set_error("неизвестная клавиша: " + part); return -1; }
    switch (ks.vk) {
      case VK_SHIFT: case VK_LSHIFT: case VK_RSHIFT: shift = true; continue;
      case VK_CONTROL: case VK_LCONTROL: case VK_RCONTROL: ctrl = true; continue;
      case VK_MENU: case VK_LMENU: case VK_RMENU: alt = true; continue;
      case VK_LWIN: case VK_RWIN: win = true; continue;
      default: break;
    }
    // Символ требует своих модификаторов ("(" = Shift+9): добавляем их к
    // явным — иначе уходит "9". Ctrl/Alt из VkKeyScan (AltGr-раскладки)
    // тоже честно нажимаем, иначе символа не будет.
    shift |= ks.shift; ctrl |= ks.ctrl; alt |= ks.alt;
    keys.push_back(ks.vk);
  }

  std::vector<WORD> order;
  if (ctrl) order.push_back(VK_CONTROL);
  if (alt) order.push_back(VK_MENU);
  if (shift) order.push_back(VK_SHIFT);
  if (win) order.push_back(VK_LWIN);
  order.insert(order.end(), keys.begin(), keys.end());
  if (order.empty()) { an_set_error("в комбинации только модификаторы: " + s); return -1; }

  for (WORD k : order) { tap_vk(k, false); sleep_ms(rnd(15, 45)); }
  sleep_ms(rnd(30, 80));
  for (auto it = order.rbegin(); it != order.rend(); ++it) { tap_vk(*it, true); sleep_ms(rnd(10, 30)); }
  return 0;
}
