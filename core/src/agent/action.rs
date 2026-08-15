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
    /// Уверенность 0..1. Ниже порога — агент сначала уточняет экран, а не
    /// делает необратимое действие (оплата, отправка, удаление).
    #[serde(default = "half")]
    pub confidence: f64,
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
}
