//! Self-verify: Do → Verify → Report.
//!
//! Правило: действие не считается выполненным, пока это не подтвердилось
//! ПОВТОРНЫМ наблюдением тем же инструментом, которым агент смотрит на мир.
//! Без этого шага агент верит своему «нажал» — а нажатие могло уйти в другое
//! окно, элемент мог быть неактивен, страница могла не догрузиться.
//!
//! Как это устроено: для каждого действия есть ожидание (`Expectation`),
//! выраженное в наблюдаемых терминах, и вердикт по двум снимкам — до и после.
//!
//! Почему не спрашиваем LLM «получилось ли?»: это +1 вызов и +1 источник
//! галлюцинаций на каждый шаг. Детерминированная проверка дешевле и честнее;
//! там, где её недостаточно, вердикт — `Unknown`, и решение остаётся за
//! критерием готовности подзадачи (`done_when`), который проверяет модель.

use super::action::Action;
use super::grounding::Evidence;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expectation {
    /// После действия на экране должен быть виден этот текст/элемент.
    Visible(String),
    /// Заголовок активного окна должен содержать подстроку.
    WindowTitle(String),
    /// Экран обязан измениться (клик, скролл, свайп).
    ScreenChanged,
    /// Проверка невозможна в терминах экрана (ожидание, запись в память).
    NotApplicable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Confirmed(String),
    Refuted(String),
    /// Проверить не удалось — не успех и не провал. Ложный «успех» дороже.
    Unknown(String),
}

impl Verdict {
    pub fn is_confirmed(&self) -> bool {
        matches!(self, Verdict::Confirmed(_))
    }
    pub fn is_refuted(&self) -> bool {
        matches!(self, Verdict::Refuted(_))
    }
    pub fn human(&self) -> String {
        match self {
            Verdict::Confirmed(s) => format!("подтверждено: {s}"),
            Verdict::Refuted(s) => format!("НЕ подтверждено: {s}"),
            Verdict::Unknown(s) => format!("проверка не дала ответа: {s}"),
        }
    }
}

/// Что должно стать видно после действия.
pub fn expectation_of(act: &Action) -> Expectation {
    match act {
        Action::ClickElement { .. }
        | Action::ClickXY { .. }
        | Action::Drag { .. }
        | Action::Scroll { .. }
        | Action::KeyCombo { .. }
        | Action::AdbTap { .. }
        | Action::AdbSwipe { .. }
        | Action::AdbKey { .. } => Expectation::ScreenChanged,
        // Введённый текст обычно виден в поле — это самая надёжная проверка.
        // Секреты (короткие коды 2FA) не ищем: их поля показывают точки.
        Action::TypeText { text, .. } | Action::AdbText { text } => {
            if text.chars().count() >= 4 && text.chars().count() <= 60 {
                Expectation::Visible(text.clone())
            } else {
                Expectation::ScreenChanged
            }
        }
        Action::OpenUrl { url } => Expectation::Visible(host_of(url)),
        Action::FocusWindow { title_contains } => Expectation::WindowTitle(title_contains.clone()),
        Action::LaunchApp { path, .. } => Expectation::Visible(exe_name(path)),
        Action::AdbOpenApp { package } => Expectation::Visible(package.clone()),
        Action::AdbShell { .. } => Expectation::NotApplicable,
        Action::Wait { .. }
        | Action::Remember { .. }
        | Action::AskUser { .. }
        | Action::NeedTool { .. }
        | Action::SubtaskDone { .. }
        | Action::Finish { .. } => Expectation::NotApplicable,
    }
}

/// Вердикт по двум наблюдениям. `after` снимается после действия — тем же
/// перцептором, что и `before`.
pub fn verify(exp: &Expectation, before: &Evidence, after: &Evidence) -> Verdict {
    if after.is_empty() {
        return Verdict::Unknown("экран не читается (пустое наблюдение)".into());
    }
    match exp {
        Expectation::NotApplicable => Verdict::Unknown("действие без наблюдаемого следа".into()),
        Expectation::Visible(what) => {
            if what.trim().is_empty() {
                return Verdict::Unknown("нечего искать на экране".into());
            }
            if after.mentions(what) {
                Verdict::Confirmed(format!("на экране видно «{}»", short(what)))
            } else if changed(before, after) {
                Verdict::Unknown(format!("экран изменился, но «{}» не видно", short(what)))
            } else {
                Verdict::Refuted(format!("«{}» не появилось, экран тот же", short(what)))
            }
        }
        Expectation::WindowTitle(t) => {
            if after.window_title.contains(&t.to_lowercase()) {
                Verdict::Confirmed(format!("активно окно «{}»", short(&after.window_title)))
            } else {
                Verdict::Refuted(format!(
                    "активно «{}», а нужно «{}»",
                    short(&after.window_title),
                    short(t)
                ))
            }
        }
        Expectation::ScreenChanged => {
            if changed(before, after) {
                Verdict::Confirmed("состояние экрана изменилось".into())
            } else {
                Verdict::Refuted("экран не изменился — действие не дало эффекта".into())
            }
        }
    }
}

/// Сравнение состояний. Считаем по множеству элементов и заголовку: точное
/// сравнение строк ловит любую мелочь (часы, счётчики) и всегда даёт «изменился».
fn changed(before: &Evidence, after: &Evidence) -> bool {
    if before.window_title != after.window_title {
        return true;
    }
    if before.elements.len() != after.elements.len() {
        return true;
    }
    let same_elements = before
        .elements
        .iter()
        .zip(after.elements.iter())
        .all(|(a, b)| a == b);
    if !same_elements {
        return true;
    }
    // Текст меняется чаще всего (введённые символы, статусы) — сверяем и его.
    before.texts != after.texts
}

fn host_of(url: &str) -> String {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("")
        .trim_start_matches("www.")
        .to_string()
}

fn exe_name(path: &str) -> String {
    path.rsplit(['\\', '/'])
        .next()
        .unwrap_or(path)
        .trim_end_matches(".exe")
        .to_string()
}

fn short(s: &str) -> String {
    if s.chars().count() <= 40 {
        s.to_string()
    } else {
        s.chars().take(40).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(title: &str, els: &[&str]) -> Evidence {
        Evidence {
            window_title: title.to_lowercase(),
            elements: els.iter().map(|e| e.to_lowercase()).collect(),
            texts: vec![],
        }
    }

    #[test]
    fn click_without_screen_change_is_refuted() {
        let before = ev("Steam", &["Установить"]);
        let after = before.clone();
        let exp = expectation_of(&Action::ClickElement {
            name: "Установить".into(),
            control_type: String::new(),
        });
        assert_eq!(exp, Expectation::ScreenChanged);
        assert!(verify(&exp, &before, &after).is_refuted());
    }

    #[test]
    fn click_with_new_elements_is_confirmed() {
        let before = ev("Steam", &["Установить"]);
        let after = ev("Steam", &["Установить", "Отмена", "Загрузка 3%"]);
        assert!(verify(&Expectation::ScreenChanged, &before, &after).is_confirmed());
    }

    #[test]
    fn typed_text_must_be_visible() {
        let before = ev("Блокнот", &["Файл"]);
        let after = ev("Блокнот", &["Файл", "привет мир"]);
        let exp = expectation_of(&Action::TypeText {
            text: "привет мир".into(),
            press_enter: false,
        });
        assert!(verify(&exp, &before, &after).is_confirmed());
        let after_bad = ev("Блокнот", &["Файл", "другое"]);
        assert!(matches!(
            verify(&exp, &before, &after_bad),
            Verdict::Unknown(_)
        ));
    }

    #[test]
    fn open_url_checks_host_only() {
        let exp = expectation_of(&Action::OpenUrl {
            url: "https://www.facebook.com/adsmanager?x=1".into(),
        });
        assert_eq!(exp, Expectation::Visible("facebook.com".into()));
        let before = ev("Chrome", &[]);
        let after = ev(
            "Facebook Ads Manager — Chrome",
            &["facebook.com/adsmanager"],
        );
        assert!(verify(&exp, &before, &after).is_confirmed());
    }

    #[test]
    fn empty_observation_is_unknown_not_success() {
        let v = verify(
            &Expectation::ScreenChanged,
            &ev("a", &["x"]),
            &Evidence::default(),
        );
        assert!(!v.is_confirmed() && !v.is_refuted());
    }
}
