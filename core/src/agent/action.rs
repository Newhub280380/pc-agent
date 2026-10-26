//! Словарь действий агента — единственный «язык», на котором LLM управляет
//! компьютером и телефоном.
//!
//! Почему закрытый enum, а не «модель пишет код/шелл»:
//!   - безопасность: агент физически не может выполнить произвольную команду;
//!   - валидация: неизвестное действие отсекается до выполнения;
//!   - воспроизводимость: каждый шаг сериализуем в журнал и повторяем.
//!
//! Минус — ограниченный набор возможностей; поэтому набор расширяемый и
//! включает `Shell` под явным разрешением пользователя (см. Permissions).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    /// Клик по элементу, найденному по имени в UI-дереве (предпочтительно:
    /// точнее координат, переживает смену позиции и размера).
    ClickElement {
        name: String,
        #[serde(default)]
        control_type: String,
    },
    /// Клик по координатам — фолбэк, когда элемент не в UI-дереве (Canvas, игры).
    ClickXY {
        x: i32,
        y: i32,
        #[serde(default)]
        button: String,
        #[serde(default)]
        double: bool,
    },
    TypeText {
        text: String,
        #[serde(default)]
        press_enter: bool,
    },
    KeyCombo {
        combo: String,
    },
    Scroll {
        clicks: i32,
        #[serde(default)]
        horizontal: bool,
    },
    Drag {
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
    },
    /// Открыть URL в браузере пользователя (его профиль, его куки —
    /// требование «режим Человек»).
    OpenUrl {
        url: String,
    },
    LaunchApp {
        path: String,
        #[serde(default)]
        args: String,
    },
    FocusWindow {
        title_contains: String,
    },
    Wait {
        seconds: f64,
        #[serde(default)]
        reason: String,
    },
    /// Запомнить факт в текущую полку памяти.
    Remember {
        title: String,
        body: String,
        #[serde(default = "half")]
        importance: f64,
    },
    /// Спросить человека (2FA, SMS-код, спорное решение) и ждать ответа.
    AskUser {
        question: String,
        #[serde(default)]
        secret: bool,
    },
    /// Android через ADB.
    AdbTap {
        x: i32,
        y: i32,
    },
    AdbSwipe {
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
        #[serde(default)]
        ms: i32,
    },
    AdbText {
        text: String,
    },
    AdbKey {
        keycode: String,
    },
    AdbOpenApp {
        package: String,
    },
    AdbShell {
        cmd: String,
    },
    /// Подзадача решена — переходим к следующей.
    SubtaskDone {
        result: String,
    },
    /// Задача целиком выполнена.
    Finish {
        report: String,
    },
    /// Агент понял, что ему не хватает инструмента/доступа. Не выдумывает и
    /// не отказывается — просит конкретную вещь.
    NeedTool {
        what: String,
        why: String,
        how_to_get: String,
    },
}

fn half() -> f64 {
    0.5
}

/// То, что модель обязана вернуть на каждом шаге.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    /// Что видно на экране прямо сейчас (проверка, что модель реально смотрит).
    #[serde(default)]
    pub observation: String,
    /// Рассуждение на 1-3 шага вперёд.
    #[serde(default)]
    pub thought: String,
    pub next: Action,
    /// Уверенность 0..1. Ниже порога (react::MIN_CONFIDENCE) агент не действует,
    /// а задаёт уточняющий вопрос — неверный клик дороже вопроса.
    #[serde(default = "half")]
    pub confidence: f64,
    /// Что именно спросить у человека при низкой уверенности. Модель
    /// формулирует вопрос точнее, чем автошаблон «что делать дальше?».
    #[serde(default)]
    pub question: String,
}

/// Привести имена действия и полей к словарю ядра. Бесплатные модели пишут
/// то же самое своими словами: `click_xy` вместо `click_x_y`, `value`
/// вместо `text`. Терять из-за этого шаг нельзя — значение модель уже
/// сказала, мы только переименовываем ключ.
fn normalize_action(obj: &mut serde_json::Map<String, serde_json::Value>) {
    if !obj.contains_key("action") {
        for k in ["action_name", "tool", "type", "act"] {
            if let Some(v @ serde_json::Value::String(_)) = obj.remove(k) {
                obj.insert("action".into(), v);
                break;
            }
        }
    }
    if let Some(serde_json::Value::String(kind)) = obj.get("action").cloned() {
        let k = kind.trim().to_lowercase().replace([' ', '-'], "_");
        let fixed = match k.as_str() {
            "click_xy" | "clickxy" | "click_coords" | "click_at" => "click_x_y",
            "click" | "clickelement" | "click_on" => "click_element",
            "type" | "typetext" | "type_in" | "input_text" | "write" => "type_text",
            "open" | "open_page" | "goto" | "navigate" | "openurl" => "open_url",
            "launch" | "run_app" | "start_app" | "launchapp" => "launch_app",
            "press" | "hotkey" | "key" | "keypress" => "key_combo",
            "done" | "node_done" | "step_done" => "subtask_done",
            "complete" | "task_done" => "finish",
            other => other,
        };
        obj.insert("action".into(), serde_json::Value::String(fixed.into()));
    }
    let needs = |obj: &serde_json::Map<String, serde_json::Value>, k: &str| !obj.contains_key(k);
    let rename =
        |obj: &mut serde_json::Map<String, serde_json::Value>, want: &str, from: &[&str]| {
            if needs(obj, want) {
                for k in from {
                    if let Some(v) = obj.remove(*k) {
                        obj.insert(want.into(), v);
                        return;
                    }
                }
            }
        };
    match obj.get("action").and_then(|v| v.as_str()).unwrap_or("") {
        "type_text" | "adb_text" => rename(obj, "text", &["value", "content", "input", "keys"]),
        "click_element" => rename(obj, "name", &["element", "target", "selector", "label"]),
        "open_url" => rename(obj, "url", &["link", "address", "href"]),
        "launch_app" => rename(obj, "path", &["app", "program", "command", "exe"]),
        "key_combo" => rename(obj, "combo", &["keys", "key", "shortcut"]),
        "focus_window" => rename(obj, "title_contains", &["title", "window"]),
        "subtask_done" => rename(obj, "result", &["report", "summary", "text"]),
        "finish" => rename(obj, "report", &["result", "summary", "text"]),
        "ask_user" => rename(obj, "question", &["q", "text"]),
        _ => {}
    }
}

/// Разбор ответа модели с починкой типичных промахов формата. На
/// бесплатных моделях каждый второй ответ приходил без поля `next`
/// (действие лежало верхним уровнем или был только вопрос) — и шаг
/// терялся вместе с минутой ожидания. Содержимое не придумываем:
/// переносим то, что модель уже написала.
pub fn decision_from_value(mut v: serde_json::Value) -> Result<Decision, serde_json::Error> {
    if let Some(obj) = v.as_object_mut() {
        if !obj.contains_key("next") {
            let lifted = match obj.remove("action") {
                // {"action":{"action":"click_element",...}}
                Some(a @ serde_json::Value::Object(_)) => Some(a),
                // {"action":"click_element","name":"login"} — действие
                // размазано по верхнему уровню.
                Some(serde_json::Value::String(kind)) => {
                    let mut inner = serde_json::Map::new();
                    inner.insert("action".into(), serde_json::Value::String(kind));
                    for (k, val) in obj.iter() {
                        if !matches!(
                            k.as_str(),
                            "observation" | "thought" | "confidence" | "question"
                        ) {
                            inner.insert(k.clone(), val.clone());
                        }
                    }
                    Some(serde_json::Value::Object(inner))
                }
                _ => None,
            };
            let lifted = lifted.or_else(|| {
                // Остался только вопрос — это ask_user, а не мусор.
                let q = obj.get("question")?.as_str()?.trim().to_string();
                if q.is_empty() {
                    return None;
                }
                Some(serde_json::json!({"action": "ask_user", "question": q}))
            });
            if let Some(next) = lifted {
                obj.insert("next".into(), next);
            }
        }
    }
    if let Some(next) = v.get_mut("next").and_then(|n| n.as_object_mut()) {
        normalize_action(next);
    }
    serde_json::from_value(v)
}

impl Action {
    /// Необратимые действия: перед ними нужен более высокий порог уверенности
    /// и (по настройке) подтверждение человека.
    pub fn is_destructive(&self) -> bool {
        match self {
            Action::ClickElement { name, .. } => {
                let n = name.to_lowercase();
                [
                    "оплат",
                    "купить",
                    "удалить",
                    "подтверд",
                    "отправ",
                    "pay",
                    "buy",
                    "delete",
                    "submit",
                    "confirm",
                ]
                .iter()
                .any(|k| n.contains(k))
            }
            Action::AdbShell { .. } => true,
            // Запуск произвольного exe с аргументами — это фактически «выполни
            // что угодно» (powershell -enc ...). Модель может получить такую
            // команду со страницы (prompt injection), поэтому спрашиваем человека.
            Action::LaunchApp { .. } => true,
            _ => false,
        }
    }

    /// Короткая запись действия для истории, промпта и журнала в БД.
    /// Содержимое type_text/adb_text намеренно не пишем целиком: там бывают
    /// пароли и коды, а memory.db лежит на диске в открытом виде.
    pub fn short(&self) -> String {
        match self {
            Action::TypeText { text, press_enter } => format!(
                "{{\"action\":\"type_text\",\"len\":{},\"press_enter\":{press_enter}}}",
                text.chars().count()
            ),
            Action::AdbText { text } => format!(
                "{{\"action\":\"adb_text\",\"len\":{}}}",
                text.chars().count()
            ),
            _ => serde_json::to_string(self).unwrap_or_else(|_| "?".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::extract_json;

    #[test]
    fn decision_parses_from_model_output_with_fences() {
        // Модели почти всегда обрамляют JSON текстом — цикл не должен ломаться.
        let raw = r#"Хорошо, вот мой ход:
```json
{"observation":"вижу форму","thought":"кликаю войти","confidence":0.9,
 "next":{"action":"click_element","name":"Войти"}}
```"#;
        let d: Decision = serde_json::from_value(extract_json(raw).unwrap()).unwrap();
        assert!(matches!(d.next, Action::ClickElement { .. }));
        assert!(!d.next.is_destructive());
        // Отсутствие question — норма: при высокой уверенности он не нужен.
        assert!(d.question.is_empty());
        assert!(d.confidence > 0.8);
    }

    #[test]
    fn unknown_action_is_rejected() {
        let raw = r#"{"next":{"action":"rm_rf","path":"C:\\"}}"#;
        assert!(serde_json::from_str::<Decision>(raw).is_err());
    }

    #[test]
    fn adb_actions_round_trip() {
        let a = Action::AdbSwipe {
            x1: 1,
            y1: 2,
            x2: 3,
            y2: 4,
            ms: 300,
        };
        let j = serde_json::to_string(&a).unwrap();
        assert!(j.contains("\"action\":\"adb_swipe\""));
        assert!(matches!(
            serde_json::from_str::<Action>(&j).unwrap(),
            Action::AdbSwipe { ms: 300, .. }
        ));
    }

    #[test]
    fn flat_action_is_lifted_into_next() {
        let v = serde_json::json!({
            "observation":"страница входа",
            "thought":"жму login",
            "confidence":0.9,
            "action":"click_element",
            "name":"login"
        });
        let d = decision_from_value(v).expect("разбор");
        assert_eq!(
            d.next.short(),
            Action::ClickElement {
                name: "login".into(),
                control_type: String::new()
            }
            .short()
        );
    }

    #[test]
    fn question_only_answer_becomes_ask_user() {
        let v =
            serde_json::json!({"thought":"не уверен","confidence":0.4,"question":"какой логин?"});
        let d = decision_from_value(v).expect("разбор");
        assert!(matches!(d.next, Action::AskUser { .. }), "{:?}", d.next);
    }

    #[test]
    fn model_wording_is_normalized() {
        let v = serde_json::json!({
            "thought":"печатаю логин","confidence":0.9,
            "next":{"action":"type","value":"zishel_demo_7120"}
        });
        let d = decision_from_value(v).expect("разбор");
        match d.next {
            Action::TypeText { text, .. } => assert_eq!(text, "zishel_demo_7120"),
            other => panic!("не то действие: {other:?}"),
        }
    }

    #[test]
    fn click_xy_alias_is_accepted() {
        let v = serde_json::json!({
            "thought":"жму","confidence":0.9,
            "next":{"action":"click_xy","x":10,"y":20}
        });
        let d = decision_from_value(v).expect("разбор");
        assert!(
            matches!(d.next, Action::ClickXY { x: 10, y: 20, .. }),
            "{:?}",
            d.next
        );
    }
}
