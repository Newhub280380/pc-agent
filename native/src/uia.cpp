// UI Automation: «читалка» реального дерева интерфейса.
//
// ПРЕДЛОЖЕНИЕ (и оно уже реализовано здесь): большинство агентов на рынке
// смотрят только картинку и потому мажут по кнопкам. UIA даёт точные
// координаты, имена, роли и состояние элементов почти во всех приложениях
// Windows, включая Chrome/Edge (если включён accessibility), Electron,
// WinForms, WPF, UWP. Это:
//   - в 10-50 раз дешевле по токенам, чем скармливать 4K-скриншот;
//   - детерминированно: «кнопка Купить» найдена по имени, а не по пикселям;
//   - устойчиво к смене темы, DPI и позиции элемента.
// Картинка остаётся фолбэком для Canvas/игр/нестандартных UI.
//
// Два архитектурных решения этого файла:
//
// 1. COM и IUIAutomation — ПО ПОТОКАМ, а не глобально. Ядро дёргает дерево
//    из рабочего потока агента, а an_init() вызывался из GUI-потока:
//    глобальный STA-объект из чужого потока — это либо RPC_E_WRONG_THREAD,
//    либо маршалинг через очередь сообщений GUI, которая в этот момент может
//    стоять, то есть дедлок. Каждый поток сам поднимает COM (MTA, чтобы не
//    конфликтовать с winit, который держит STA в GUI-потоке) и свой экземпляр
//    CUIAutomation. Мьютекс и use-after-free при shutdown исчезают.
//
// 2. Без ATL. <atlbase.h> — опциональный компонент MSVC, которого нет в
//    Build Tools по умолчанию, и сборка падала на C1083. CComPtr/CComBSTR
//    заменены двумя RAII-обёртками на 30 строк.
#include "common.hpp"
#include "../include/agent_native.h"
#include <uiautomation.h>
#include <string>
#include <vector>

#pragma comment(lib, "uiautomationcore.lib")

namespace {

// Минимальный умный указатель на COM-интерфейс.
template <class T>
class ComPtr {
 public:
  ComPtr() = default;
  ComPtr(const ComPtr&) = delete;
  ComPtr& operator=(const ComPtr&) = delete;
  ComPtr(ComPtr&& o) noexcept : p_(o.p_) { o.p_ = nullptr; }
  ComPtr& operator=(ComPtr&& o) noexcept {
    if (this != &o) { reset(); p_ = o.p_; o.p_ = nullptr; }
    return *this;
  }
  ~ComPtr() { reset(); }
  void reset() { if (p_) { p_->Release(); p_ = nullptr; } }
  // Забыть указатель без Release: когда апартамент уже снят чужим
  // CoUninitialize, Release — это обращение к мёртвому объекту (AV при
  // выходе из процесса), а утечка одного указателя при завершении безвредна.
  void detach() { p_ = nullptr; }
  T** put() { reset(); return &p_; }
  void** put_void() { reset(); return reinterpret_cast<void**>(&p_); }
  T* get() const { return p_; }
  T* operator->() const { return p_; }
  explicit operator bool() const { return p_ != nullptr; }
 private:
  T* p_ = nullptr;
};

// BSTR, который сам освобождается.
class Bstr {
 public:
  Bstr() = default;
  Bstr(const Bstr&) = delete;
  Bstr& operator=(const Bstr&) = delete;
  ~Bstr() { if (s_) ::SysFreeString(s_); }
  BSTR* put() { if (s_) { ::SysFreeString(s_); s_ = nullptr; } return &s_; }
  std::wstring str() const { return s_ ? std::wstring(s_, ::SysStringLen(s_)) : L""; }
  bool empty() const { return !s_ || ::SysStringLen(s_) == 0; }
 private:
  BSTR s_ = nullptr;
};

// COM-апартамент текущего потока. Поднимается один раз на поток и живёт
// до его смерти. RPC_E_CHANGED_MODE — чужой код уже выбрал режим апартамента
// в этом потоке (winit в GUI-потоке держит STA): это нормально, COM уже есть,
// просто уничтожать его должны не мы.
//
// Порядок на выходе из процесса важен: thread_local-деструкторы главного потока
// выполняются ПОСЛЕ main() и, значит, после an_shutdown()→CoUninitialize().
// Release() на IUIAutomation в этот момент — обращение к снесённому
// апартаменту: именно это давало 0xC0000005 при выходе из --selfcheck.
// Поэтому: (1) uia_shutdown() явно отпускает объект пока COM ещё жив;
// (2) деструктор не трогает COM, если его уже сняли (`released`).
struct ThreadCom {
  bool owned = false;
  bool released = false;
  ComPtr<IUIAutomation> uia;
  ThreadCom() {
    HRESULT hr = ::CoInitializeEx(nullptr, COINIT_MULTITHREADED | COINIT_DISABLE_OLE1DDE);
    owned = SUCCEEDED(hr);  // S_OK или S_FALSE — наш счётчик, снимем сами
  }
  // Освободить UIA и свой счётчик COM, пока апартамент гарантированно жив.
  void release_now() {
    if (released) return;
    released = true;
    uia.reset();
    if (owned) { ::CoUninitialize(); owned = false; }
  }
  ~ThreadCom() {
    if (released) { uia.detach(); return; }
    // Поток умирает штатно (рабочий поток агента): COM ещё наш — освобождаем.
    release_now();
  }
};

ThreadCom& thread_com() {
  static thread_local ThreadCom tc;
  return tc;
}

// IUIAutomation для текущего потока; создаётся лениво.
IUIAutomation* uia() {
  ThreadCom& tc = thread_com();
  if (tc.released) {
    // После uia_shutdown() в этом потоке UIA больше не поднимаем: мы на выходе.
    an_set_error("UIAutomation уже остановлен в этом потоке");
    return nullptr;
  }
  if (!tc.uia) {
    HRESULT hr = ::CoCreateInstance(CLSID_CUIAutomation, nullptr, CLSCTX_INPROC_SERVER,
                                    IID_IUIAutomation, tc.uia.put_void());
    if (FAILED(hr)) {
      tc.uia.reset();
      an_set_error("UIAutomation недоступен (hr=" + std::to_string((long)hr) + ")");
      return nullptr;
    }
  }
  return tc.uia.get();
}

std::string control_type_name(CONTROLTYPEID id) {
  switch (id) {
    case UIA_ButtonControlTypeId: return "button";
    case UIA_EditControlTypeId: return "edit";
    case UIA_TextControlTypeId: return "text";
    case UIA_CheckBoxControlTypeId: return "checkbox";
    case UIA_ComboBoxControlTypeId: return "combobox";
    case UIA_ListControlTypeId: return "list";
    case UIA_ListItemControlTypeId: return "listitem";
    case UIA_HyperlinkControlTypeId: return "link";
    case UIA_MenuItemControlTypeId: return "menuitem";
    case UIA_TabItemControlTypeId: return "tab";
    case UIA_WindowControlTypeId: return "window";
    case UIA_DocumentControlTypeId: return "document";
    case UIA_ImageControlTypeId: return "image";
    case UIA_RadioButtonControlTypeId: return "radio";
    case UIA_PaneControlTypeId: return "pane";
    case UIA_GroupControlTypeId: return "group";
    default: return "other";
  }
}

// Сколько символов текста документа/поля отдаём модели. Больше — это
// десятки КБ в промпт на каждом шаге; меньше — модель не видит, что
// напечаталось. 4 КБ хватает на экран текста.
constexpr int kMaxTextChars = 4000;

// Текст содержимого элемента: ValuePattern (поля ввода) или TextPattern
// (документы Word/WordPad/браузерные редакторы). Зачем: у Word тело
// документа в UIA — это `document`/`edit` c именем «Страница 1 содержимое»
// и ПУСТЫМ Name; без паттернов пустой документ и документ с текстом
// неотличимы (vercel-labs/agent-desktop#479), и агент не может проверить,
// что его ввод вообще куда-то попал.
std::string element_text(IUIAutomationElement* el, bool* has_text_pattern) {
  *has_text_pattern = false;
  // 1. ValuePattern: обычные поля ввода, адресная строка, Notepad.
  {
    ComPtr<IUnknown> unk;
    if (SUCCEEDED(el->GetCurrentPattern(UIA_ValuePatternId, unk.put())) && unk) {
      ComPtr<IUIAutomationValuePattern> vp;
      if (SUCCEEDED(unk->QueryInterface(IID_IUIAutomationValuePattern, vp.put_void())) && vp) {
        Bstr v;
        if (SUCCEEDED(vp->get_CurrentValue(v.put())) && !v.empty()) {
          *has_text_pattern = true;
          std::wstring w = v.str();
          if (w.size() > (size_t)kMaxTextChars) w.resize(kMaxTextChars);
          return wide_to_utf8(w);
        }
      }
    }
  }
  // 2. TextPattern: документы. Word, WordPad, RichEdit, Chromium contenteditable.
  {
    ComPtr<IUnknown> unk;
    if (SUCCEEDED(el->GetCurrentPattern(UIA_TextPatternId, unk.put())) && unk) {
      ComPtr<IUIAutomationTextPattern> tp;
      if (SUCCEEDED(unk->QueryInterface(IID_IUIAutomationTextPattern, tp.put_void())) && tp) {
        *has_text_pattern = true;
        ComPtr<IUIAutomationTextRange> range;
        if (SUCCEEDED(tp->get_DocumentRange(range.put())) && range) {
          Bstr t;
          // maxLength = -1 означает «весь текст»; ограничиваем сами.
          if (SUCCEEDED(range->GetText(kMaxTextChars, t.put()))) {
            std::wstring w = t.str();
            // Word отдаёт конец абзаца как \r, конец документа как \r\x07 —
            // нормализуем, иначе JSON и промпт полны мусора.
            std::wstring clean;
            clean.reserve(w.size());
            for (wchar_t c : w) {
              if (c == L'\r') { clean += L'\n'; continue; }
              if (c == 0x07 || c == 0x0C) continue;
              clean += c;
            }
            return wide_to_utf8(clean);
          }
        }
      }
    }
  }
  return "";
}

void walk(IUIAutomationElement* el, IUIAutomationTreeWalker* walker, int depth, int maxDepth,
          std::string& json, bool& first, int& budget) {
  if (!el || !walker || depth > maxDepth || budget <= 0) return;

  Bstr name, autoId;
  CONTROLTYPEID ct = 0;
  RECT r{};
  BOOL offscreen = TRUE;
  el->get_CurrentName(name.put());
  el->get_CurrentAutomationId(autoId.put());
  el->get_CurrentControlType(&ct);
  el->get_CurrentBoundingRectangle(&r);
  el->get_CurrentIsOffscreen(&offscreen);

  const int w = r.right - r.left, h = r.bottom - r.top;
  const std::string nm = wide_to_utf8(name.str());

  // Поля ввода и документы читаем всегда: имя у них часто пустое или
  // служебное («Страница 1 содержимое»), а важен именно текст внутри.
  const bool textual = ct == UIA_EditControlTypeId || ct == UIA_DocumentControlTypeId;
  std::string text;
  bool has_pattern = false;
  if (textual) text = element_text(el, &has_pattern);

  // Фильтруем мусор: невидимое, нулевого размера и безымянные контейнеры.
  // Иначе дерево Chrome — это десятки тысяч узлов и переполненный контекст LLM.
  const bool useful = !offscreen && w > 2 && h > 2 && (!nm.empty() || textual);
  if (useful) {
    if (!first) json += ",";
    first = false;
    json += "{\"name\":\"" + json_escape(nm) + "\"" +
            ",\"type\":\"" + control_type_name(ct) + "\"" +
            ",\"id\":\"" + json_escape(wide_to_utf8(autoId.str())) + "\"" +
            ",\"x\":" + std::to_string(r.left) + ",\"y\":" + std::to_string(r.top) +
            ",\"w\":" + std::to_string(w) + ",\"h\":" + std::to_string(h) +
            ",\"cx\":" + std::to_string(r.left + w / 2) + ",\"cy\":" + std::to_string(r.top + h / 2);
    if (textual) {
      // text — что внутри; has_text — элемент умеет отдавать текст вообще.
      // Пустой text при has_text=true означает «документ пуст», при
      // has_text=false — «текст недоступен через accessibility» (#479).
      json += ",\"text\":\"" + json_escape(text) + "\"";
      json += std::string(",\"has_text\":") + (has_pattern ? "true" : "false");
    }
    json += "}";
    --budget;
  }

  ComPtr<IUIAutomationElement> child;
  walker->GetFirstChildElement(el, child.put());
  while (child && budget > 0) {
    walk(child.get(), walker, depth + 1, maxDepth, json, first, budget);
    ComPtr<IUIAutomationElement> next;
    walker->GetNextSiblingElement(child.get(), next.put());
    child = std::move(next);
  }
}

}  // namespace

// Поднимает COM и UIA в ТЕКУЩЕМ потоке. Из an_init вызывается для GUI-потока;
// рабочие потоки поднимут своё лениво при первом an_ui_tree.
int32_t uia_init() {
  return uia() ? 0 : -1;
}

// Освобождает UIA и COM ТЕКУЩЕГО потока, пока апартамент жив. Вызывается из
// an_shutdown() в главном потоке до его CoUninitialize(); рабочие потоки
// освобождают своё сами при выходе из потока.
void uia_shutdown() { thread_com().release_now(); }

extern "C" int32_t an_ui_tree(uint64_t hwnd, int32_t max_depth, char** out_json) {
  if (!out_json) { an_set_error("ui_tree: out == null"); return -1; }
  IUIAutomation* a = uia();
  if (!a) return -1;
  ComPtr<IUIAutomationElement> root;
  HWND h = hwnd ? reinterpret_cast<HWND>(hwnd) : ::GetForegroundWindow();
  if (!h) { an_set_error("нет активного окна для UIA"); return -2; }
  if (FAILED(a->ElementFromHandle(h, root.put())) || !root) {
    an_set_error("не удалось получить UIA-элемент окна"); return -2;
  }
  ComPtr<IUIAutomationTreeWalker> walker;
  // ControlViewWalker вместо RawView: raw содержит служебные узлы, которые
  // человеку (и модели) ни о чём не говорят.
  if (FAILED(a->get_ControlViewWalker(walker.put())) || !walker) {
    an_set_error("UIA walker недоступен"); return -2;
  }

  std::string json = "[";
  bool first = true;
  int budget = 400;  // жёсткий лимит узлов: контекст LLM не резиновый
  // max_depth сверху ограничен: walk — рекурсия, а дерево Electron бывает
  // патологически глубоким — иначе переполнение стека.
  const int depth = (max_depth > 0 && max_depth <= 64) ? max_depth : 12;
  walk(root.get(), walker.get(), 0, depth, json, first, budget);
  json += "]";
  char* dup = an_dup_cstr(json);
  if (!dup) { an_set_error("OOM"); return -3; }
  *out_json = dup;
  return 0;
}

extern "C" int32_t an_ui_find(const char* name_substr, const char* control_type, an_rect* out) {
  if (!out) { an_set_error("ui_find: out == null"); return -1; }
  IUIAutomation* a = uia();
  if (!a) return -1;
  ComPtr<IUIAutomationElement> root;
  if (FAILED(a->ElementFromHandle(::GetForegroundWindow(), root.put())) || !root) return -2;

  ComPtr<IUIAutomationTreeWalker> walker;
  if (FAILED(a->get_ControlViewWalker(walker.put())) || !walker) return -2;

  std::string needle = name_substr ? name_substr : "";
  std::string wantType = control_type ? control_type : "";
  std::string json = "[";
  bool first = true;
  int budget = 4000;
  walk(root.get(), walker.get(), 0, 16, json, first, budget);
  json += "]";

  // Поиск идём по уже сериализованному дереву: так одна логика фильтрации
  // и в Rust, и здесь. Для 4000 узлов это доли миллисекунды.
  size_t pos = 0;
  while ((pos = json.find("{\"name\":\"", pos)) != std::string::npos) {
    size_t end = json.find('}', pos);
    if (end == std::string::npos) break;
    std::string obj = json.substr(pos, end - pos);
    bool nameOk = needle.empty() || obj.find(json_escape(needle)) != std::string::npos;
    bool typeOk = wantType.empty() || obj.find("\"type\":\"" + wantType + "\"") != std::string::npos;
    if (nameOk && typeOk) {
      auto num = [&](const char* key) -> int {
        size_t k = obj.find(std::string("\"") + key + "\":");
        if (k == std::string::npos) return 0;
        return atoi(obj.c_str() + k + strlen(key) + 3);
      };
      *out = an_rect{num("x"), num("y"), num("w"), num("h"), 1.0f};
      return 0;
    }
    pos = end;
  }
  an_set_error("элемент не найден в UIA-дереве");
  return -3;
}
