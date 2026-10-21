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
                Verdict::Confirmed(format!(
                    "на экране видно «{}»",
                    crate::text::truncate(what, 40)
                ))
            } else if changed(before, after) {
                Verdict::Unknown(format!(
                    "экран изменился, но «{}» не видно",
                    crate::text::truncate(what, 40)
                ))
            } else {
                Verdict::Refuted(format!(
                    "«{}» не появилось, экран тот же",
                    crate::text::truncate(what, 40)
                ))
            }
        }
        Expectation::WindowTitle(t) => {
            if after.window_title.contains(&t.to_lowercase()) {
                Verdict::Confirmed(format!(
                    "активно окно «{}»",
                    crate::text::truncate(&after.window_title, 40)
                ))
            } else {
                Verdict::Refuted(format!(
                    "активно «{}», а нужно «{}»",
                    crate::text::truncate(&after.window_title, 40),
                    crate::text::truncate(t, 40)
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

/// В критерии есть дословный фрагмент или числовое сравнение.
/// Такое условие можно проверить по экрану, а не по пересказу модели.
pub fn has_checkable_literal(done_when: &str) -> bool {
    !quoted_fragments(done_when).is_empty() || !comparisons(done_when).is_empty()
}

/// Критерий вообще можно сверить с экраном: есть цитата, сравнение
/// или содержательное слово. «ок» и пустая строка сюда не попадают.
pub fn is_measurable(done_when: &str) -> bool {
    let criterion = done_when.trim();
    if criterion.is_empty() {
        return false;
    }
    has_checkable_literal(criterion) || !significant_tokens(criterion).is_empty()
}

/// Куски задачи, которые проверяются отдельно: цитаты и сравнения.
/// Целая фраза без них остаётся на вызывающем — здесь только то, что
/// уже является буквальным условием.
pub fn measurable_parts(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for q in quoted_fragments(text) {
        out.push(format!("«{q}»"));
    }
    for c in comparisons(text) {
        out.push(comparison_text(&c));
    }
    out
}

fn comparison_text(c: &Comparison) -> String {
    let op = match c.op {
        Cmp::Gt => ">",
        Cmp::Ge => ">=",
        Cmp::Lt => "<",
        Cmp::Le => "<=",
    };
    let n = if c.threshold.fract() == 0.0 {
        format!("{}", c.threshold as i64)
    } else {
        format!("{}", c.threshold)
    };
    format!("{op} {n}")
}

/// Критерий выполнен, только если экран его показывает.
///
/// Правила, все обязательные:
///   - пустой критерий или пустой экран — нет;
///   - каждый фрагмент в кавычках (`«»`, `"`, `'`) должен встретиться дословно;
///   - каждое сравнение (`> 50`, `<= 3`) должно выполняться для какого-то
///     числа на экране;
///   - если кавычек нет, каждое содержательное слово критерия тоже должно
///     быть на экране. Слова короче 4 букв и служебные слова не считаются:
///     из них нельзя собрать доказательство.
pub fn criterion_observed(done_when: &str, evidence: &Evidence) -> bool {
    let criterion = done_when.trim();
    if criterion.is_empty() || evidence.is_empty() {
        return false;
    }
    let quotes = quoted_fragments(criterion);
    if quotes.iter().any(|q| !evidence.mentions(q)) {
        return false;
    }
    let comps = comparisons(criterion);
    let numbers = evidence_numbers(evidence);
    if comps.iter().any(|c| !numbers.iter().any(|n| c.holds(*n))) {
        return false;
    }
    if !quotes.is_empty() {
        return true;
    }
    let tokens = significant_tokens(criterion);
    if comps.is_empty() && tokens.is_empty() {
        return false;
    }
    tokens.iter().all(|t| evidence.mentions(t))
}

#[derive(Clone, Copy)]
enum Cmp {
    Gt,
    Ge,
    Lt,
    Le,
}

struct Comparison {
    op: Cmp,
    threshold: f64,
}

impl Comparison {
    fn holds(&self, value: f64) -> bool {
        match self.op {
            Cmp::Gt => value > self.threshold,
            Cmp::Ge => value >= self.threshold,
            Cmp::Lt => value < self.threshold,
            Cmp::Le => value <= self.threshold,
        }
    }
}

fn quoted_fragments(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let close = match chars[i] {
            '«' => Some('»'),
            '"' => Some('"'),
            '\'' => Some('\''),
            _ => None,
        };
        if let Some(end) = close {
            if let Some(rel) = chars[i + 1..].iter().position(|c| *c == end) {
                let frag: String = chars[i + 1..i + 1 + rel].iter().collect();
                if !frag.trim().is_empty() {
                    out.push(frag);
                }
                i += rel + 2;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn comparisons(text: &str) -> Vec<Comparison> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let (op, width) = if chars[i] == '>' && i + 1 < chars.len() && chars[i + 1] == '=' {
            (Cmp::Ge, 2)
        } else if chars[i] == '<' && i + 1 < chars.len() && chars[i + 1] == '=' {
            (Cmp::Le, 2)
        } else if chars[i] == '>' {
            (Cmp::Gt, 1)
        } else if chars[i] == '<' {
            (Cmp::Lt, 1)
        } else {
            i += 1;
            continue;
        };
        i += width;
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        let start = i;
        if i < chars.len() && chars[i].is_ascii_digit() {
            i += 1;
            while i < chars.len()
                && (chars[i].is_ascii_digit() || chars[i] == '.' || chars[i] == ',')
            {
                i += 1;
            }
            let raw: String = chars[start..i].iter().collect();
            if let Ok(threshold) = raw.replace(',', ".").parse::<f64>() {
                out.push(Comparison { op, threshold });
            }
        }
    }
    out
}

fn evidence_numbers(evidence: &Evidence) -> Vec<f64> {
    let mut blob = evidence.window_title.clone();
    blob.push(' ');
    blob.push_str(&evidence.elements.join(" "));
    blob.push(' ');
    blob.push_str(&evidence.texts.join(" "));
    let chars: Vec<char> = blob.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_ascii_digit() {
            let start = i;
            i += 1;
            while i < chars.len()
                && (chars[i].is_ascii_digit() || chars[i] == '.' || chars[i] == ',')
            {
                i += 1;
            }
            let raw: String = chars[start..i].iter().collect();
            if let Ok(n) = raw.replace(',', ".").parse::<f64>() {
                out.push(n);
            }
        } else {
            i += 1;
        }
    }
    out
}

fn significant_tokens(text: &str) -> Vec<String> {
    const STOP: &[&str] = &[
        "видно",
        "что",
        "когда",
        "есть",
        "это",
        "для",
        "или",
        "как",
        "уже",
        "ещё",
        "еще",
        "надо",
        "нужно",
        "должен",
        "должна",
        "должно",
        "после",
        "перед",
        "только",
        "если",
        "при",
        "над",
        "под",
        "без",
        "его",
        "её",
        "ее",
        "они",
        "был",
        "была",
        "были",
        "будет",
        "the",
        "and",
        "with",
        "from",
        "this",
        "that",
    ];
    let mut token = String::new();
    let mut out = Vec::new();
    let flush = |token: &mut String, out: &mut Vec<String>| {
        if token.chars().count() >= 4 && !STOP.contains(&token.as_str()) {
            out.push(std::mem::take(token));
        } else {
            token.clear();
        }
    };
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            token.push(ch.to_lowercase().next().unwrap_or(ch));
        } else {
            flush(&mut token, &mut out);
        }
    }
    flush(&mut token, &mut out);
    out
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

    fn screen(text: &str) -> Evidence {
        Evidence {
            window_title: String::new(),
            elements: vec![],
            texts: vec![text.to_lowercase()],
        }
    }

    #[test]
    fn numeric_criterion_accepts_a_larger_value_on_screen() {
        let ok = screen("свободно 512 ГБ");
        assert!(criterion_observed("видно свободно > 50 ГБ", &ok));
        let small = screen("свободно 10 ГБ");
        assert!(!criterion_observed("видно свободно > 50 ГБ", &small));
    }

    #[test]
    fn quoted_criterion_must_appear_verbatim() {
        let done = screen("установщик: Готово");
        assert!(criterion_observed("установщик показал «Готово»", &done));
        let busy = screen("установщик ещё работает");
        assert!(!criterion_observed("установщик показал «Готово»", &busy));
    }

    #[test]
    fn vague_or_empty_criterion_cannot_pass() {
        let ok = screen("всё хорошо и окно открыто");
        assert!(!criterion_observed("", &ok));
        assert!(!criterion_observed("ок", &ok));
        assert!(!criterion_observed("файл скачан", &Evidence::default()));
        assert!(criterion_observed(
            "файл скачан",
            &screen("файл скачан в загрузки")
        ));
    }
}
