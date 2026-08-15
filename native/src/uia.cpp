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
#include "common.hpp"
#include "../include/agent_native.h"
#include <uiautomation.h>
#include <atlbase.h>
#include <mutex>
#include <string>
#include <vector>

#pragma comment(lib, "uiautomationcore.lib")

namespace {

IUIAutomation* g_uia = nullptr;
// GUI-поток и поток агента ходят в UIA одновременно (лог и шаг цикла),
// а g_uia — глобальный указатель: без мьютекса это гонка на инициализации
// и use-after-free при shutdown.
std::mutex g_uia_mu;

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
    default: return "other";
  }
}

void walk(IUIAutomationElement* el, IUIAutomationTreeWalker* walker, int depth, int maxDepth,
          std::string& json, bool& first, int& budget) {
  if (!el || !walker || depth > maxDepth || budget <= 0) return;

  CComBSTR name, autoId;
  CONTROLTYPEID ct = 0;
  RECT r{};
  BOOL offscreen = TRUE;
  el->get_CurrentName(&name);
  el->get_CurrentAutomationId(&autoId);
  el->get_CurrentControlType(&ct);
  el->get_CurrentBoundingRectangle(&r);
  el->get_CurrentIsOffscreen(&offscreen);

  const int w = r.right - r.left, h = r.bottom - r.top;
  const std::string nm = name ? wide_to_utf8(std::wstring(name, SysStringLen(name))) : "";
  // Фильтруем мусор: невидимое, нулевого размера и безымянные контейнеры.
  // Иначе дерево Chrome — это десятки тысяч узлов и переполненный контекст LLM.
  const bool useful = !offscreen && w > 2 && h > 2 && (!nm.empty() || ct == UIA_EditControlTypeId);
  if (useful) {
    if (!first) json += ",";
    first = false;
    json += "{\"name\":\"" + json_escape(nm) + "\"" +
            ",\"type\":\"" + control_type_name(ct) + "\"" +
            ",\"id\":\"" + json_escape(autoId ? wide_to_utf8(std::wstring(autoId, SysStringLen(autoId))) : "") + "\"" +
            ",\"x\":" + std::to_string(r.left) + ",\"y\":" + std::to_string(r.top) +
            ",\"w\":" + std::to_string(w) + ",\"h\":" + std::to_string(h) +
            ",\"cx\":" + std::to_string(r.left + w / 2) + ",\"cy\":" + std::to_string(r.top + h / 2) + "}";
    --budget;
  }

  CComPtr<IUIAutomationElement> child;
  walker->GetFirstChildElement(el, &child);
  while (child && budget > 0) {
    walk(child, walker, depth + 1, maxDepth, json, first, budget);
    CComPtr<IUIAutomationElement> next;
    walker->GetNextSiblingElement(child, &next);
    child = next;
  }
}

}  // namespace

// Вызывается из an_init или лениво; вызывающий держит g_uia_mu.
int32_t uia_init_locked() {
  if (g_uia) return 0;
  HRESULT hr = ::CoCreateInstance(CLSID_CUIAutomation, nullptr, CLSCTX_INPROC_SERVER,
                                  IID_IUIAutomation, (void**)&g_uia);
  if (FAILED(hr)) { an_set_error("UIAutomation недоступен"); return -1; }
  return 0;
}

int32_t uia_init() {
  std::lock_guard<std::mutex> lk(g_uia_mu);
  return uia_init_locked();
}

void uia_shutdown() {
  std::lock_guard<std::mutex> lk(g_uia_mu);
  if (g_uia) { g_uia->Release(); g_uia = nullptr; }
}

extern "C" int32_t an_ui_tree(uint64_t hwnd, int32_t max_depth, char** out_json) {
  if (!out_json) { an_set_error("ui_tree: out == null"); return -1; }
  std::lock_guard<std::mutex> lk(g_uia_mu);
  if (uia_init_locked() != 0) return -1;
  CComPtr<IUIAutomationElement> root;
  HWND h = hwnd ? reinterpret_cast<HWND>(hwnd) : ::GetForegroundWindow();
  if (FAILED(g_uia->ElementFromHandle(h, &root)) || !root) {
    an_set_error("не удалось получить UIA-элемент окна"); return -2;
  }
  CComPtr<IUIAutomationTreeWalker> walker;
  // ControlViewWalker вместо RawView: raw содержит служебные узлы, которые
  // человеку (и модели) ни о чём не говорят.
  if (FAILED(g_uia->get_ControlViewWalker(&walker)) || !walker) {
    an_set_error("UIA walker недоступен"); return -2;
  }

  std::string json = "[";
  bool first = true;
  int budget = 400;  // жёсткий лимит узлов: контекст LLM не резиновый
  // max_depth сверху ограничен: walk — рекурсия, а дерево Electron бывает
  // патологически глубоким — иначе переполнение стека.
  const int depth = (max_depth > 0 && max_depth <= 64) ? max_depth : 12;
  walk(root, walker, 0, depth, json, first, budget);
  json += "]";
  char* dup = an_dup_cstr(json);
  if (!dup) { an_set_error("OOM"); return -3; }
  *out_json = dup;
  return 0;
}

extern "C" int32_t an_ui_find(const char* name_substr, const char* control_type, an_rect* out) {
  if (!out) { an_set_error("ui_find: out == null"); return -1; }
  std::lock_guard<std::mutex> lk(g_uia_mu);
  if (uia_init_locked() != 0) return -1;
  CComPtr<IUIAutomationElement> root;
  if (FAILED(g_uia->ElementFromHandle(::GetForegroundWindow(), &root)) || !root) return -2;

  CComPtr<IUIAutomationTreeWalker> walker;
  if (FAILED(g_uia->get_ControlViewWalker(&walker)) || !walker) return -2;

  std::string needle = name_substr ? name_substr : "";
  std::string wantType = control_type ? control_type : "";
  std::string json = "[";
  bool first = true;
  int budget = 4000;
  walk(root, walker, 0, 16, json, first, budget);
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
