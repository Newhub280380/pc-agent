//! Grounding — «заземление»: любое утверждение агента должно быть выводимо из
//! наблюдения (UI-дерево, OCR, заголовок окна, вывод инструмента).
//!
//! Проблема, которую это решает: LLM охотно пишет «файл лежит в
//! C:\Program Files\Fable\fable.exe», «нажал кнопку Продолжить», «команда
//! выполнена успешно» — при том, что на экране ничего этого нет. Такие
//! выдумки страшнее падений: агент отчитывается об успехе, которого не было.
//!
//! Как ловим: из текста агента извлекаем проверяемые сущности — пути, URL,
//! имена в кавычках — и требуем, чтобы каждая встречалась в наблюдении, в
//! задаче пользователя или (для путей) реально существовала на диске.
//!
//! Альтернативы:
//!   1. просить модель «не выдумывать» в промпте — не работает, это не гарантия;
//!   2. вторая модель-судья — дорого (+вызов на каждый шаг) и сама галлюцинирует;
//!   3. детерминированная проверка сущностей (выбрано) — дешёвая, объяснимая,
//!      ловит именно вредный класс выдумок: несуществующие объекты.
//!
//! Осознанное ограничение: проверяются сущности, а не смысл. «Реклама
//! настроена» без сущностей не поймается здесь — за это отвечает self-verify
//! (см. `verify.rs`), который требует наблюдаемого подтверждения.

use std::collections::HashSet;
use std::path::Path;

/// Всё, что агент реально видел на этом шаге.
#[derive(Debug, Clone, Default)]
pub struct Evidence {
    pub window_title: String,
    /// Имена UI-элементов (в нижнем регистре).
    pub elements: Vec<String>,
    /// Текстовые строки: OCR, подписи, вывод инструментов (в нижнем регистре).
    pub texts: Vec<String>,
}

impl Evidence {
    /// Разбор того же текста, который ушёл в модель. Важно именно так:
    /// проверяем модель по тем данным, которые ей дали, а не по внутренним
    /// структурам — иначе проверка «знает больше» модели и врёт в её пользу.
    pub fn from_prompt(prompt: &str) -> Self {
        let mut ev = Evidence::default();
        for raw in prompt.lines() {
            let line = raw.trim();
            if let Some(rest) = line.strip_prefix("- ") {
                match rest.split_once('|') {
                    Some((name, tail)) => {
                        ev.elements.push(name.trim().to_lowercase());
                        // Хвост тоже сохраняем: там тип элемента и координаты,
                        // на них модель иногда ссылается.
                        ev.texts.push(tail.trim().to_lowercase());
                    }
                    None => ev.texts.push(rest.trim().to_lowercase()),
                }
                continue;
            }
            if let Some(idx) = line.find("Активное окно: \"") {
                let after = &line[idx + "Активное окно: \"".len()..];
                if let Some(end) = after.rfind('"') {
                    ev.window_title = after[..end].to_lowercase();
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("ЭКРАН ТЕЛЕФОНА. Активное: ")
            {
                ev.window_title = rest.trim().to_lowercase();
                continue;
            }
            if !line.is_empty() {
                ev.texts.push(line.to_lowercase());
            }
        }
        ev
    }

    /// Добавить вывод инструмента: он тоже законный источник фактов
    /// («adb shell: 12 файлов» — это наблюдение, а не выдумка).
    pub fn add_tool_output(&mut self, out: &str) {
        for l in out.lines() {
            let l = l.trim().to_lowercase();
            if !l.is_empty() {
                self.texts.push(l);
            }
        }
    }

    pub fn mentions(&self, needle: &str) -> bool {
        let n = needle.trim().to_lowercase();
        if n.is_empty() {
            return true;
        }
        if self.window_title.contains(&n) {
            return true;
        }
        self.elements
            .iter()
            .any(|e| e.contains(&n) || n.contains(e))
            || self.texts.iter().any(|t| t.contains(&n))
    }

    pub fn is_empty(&self) -> bool {
        self.window_title.is_empty() && self.elements.is_empty() && self.texts.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimKind {
    /// Путь к файлу/каталогу.
    Path,
    Url,
    /// Имя элемента интерфейса в кавычках.
    Element,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub kind: ClaimKind,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub claim: Claim,
    pub why: String,
}

impl Violation {
    pub fn human(&self) -> String {
        format!("«{}»: {}", self.claim.value, self.why)
    }
}

/// Извлечение проверяемых сущностей. Регулярок нет намеренно: одна зависимость
/// меньше, а формат тут простой и предсказуемый.
pub fn extract_claims(text: &str) -> Vec<Claim> {
    let mut out: Vec<Claim> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for tok in text.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '(' | ')')) {
        let t = tok.trim_matches(|c: char| matches!(c, '.' | '!' | '?' | ':' | '»' | '«' | '"'));
        if t.is_empty() {
            continue;
        }
        let kind = if t.starts_with("http://") || t.starts_with("https://") {
            ClaimKind::Url
        } else if is_pathish(t) {
            ClaimKind::Path
        } else {
            continue;
        };
        if seen.insert(t.to_lowercase()) {
            out.push(Claim {
                kind,
                value: t.to_string(),
            });
        }
    }

    for value in quoted(text) {
        // Однобуквенные и слишком длинные «имена» — шум, а не ссылка на элемент.
        let n = value.chars().count();
        if !(2..=80).contains(&n) {
            continue;
        }
        if is_pathish(&value) || value.starts_with("http") {
            continue;
        }
        if seen.insert(value.to_lowercase()) {
            out.push(Claim {
                kind: ClaimKind::Element,
                value,
            });
        }
    }
    out
}

/// Windows-путь (`C:\...`), UNC (`\\host\share`) или unix-путь. Плюс имена
/// исполняемых файлов: «запусти fable.exe» — тоже проверяемое утверждение.
fn is_pathish(t: &str) -> bool {
    let bytes = t.as_bytes();
    let drive = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/');
    let unc = t.starts_with("\\\\");
    let unix = t.starts_with('/') && t.len() > 1 && t[1..].contains('/');
    let exe = {
        let low = t.to_lowercase();
        [".exe", ".msi", ".bat", ".cmd", ".ps1", ".dll"]
            .iter()
            .any(|s| low.ends_with(s))
    };
    drive || unc || unix || exe
}

/// Содержимое «…», "…" и '…'. Кавычки — самый частый способ, которым модель
/// называет кнопки и поля.
fn quoted(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let pairs = [('«', '»'), ('"', '"'), ('\'', '\''), ('“', '”')];
    for (open, close) in pairs {
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == open {
                let mut j = i + 1;
                let mut buf = String::new();
                while j < chars.len() && chars[j] != close {
                    buf.push(chars[j]);
                    j += 1;
                }
                if j < chars.len() {
                    let v = buf.trim().to_string();
                    if !v.is_empty() {
                        out.push(v);
                    }
                    i = j + 1;
                    continue;
                }
                // Незакрытая кавычка — не утверждение, а обрывок текста.
                break;
            }
            i += 1;
        }
    }
    out
}

/// Проверка утверждений агента.
///
/// `task` — формулировка пользователя: то, что он сам назвал, агент не выдумал.
/// `check_fs` — разрешено ли трогать диск (в тестах и на чужой машине не нужно).
pub fn check_claims(text: &str, ev: &Evidence, task: &str, check_fs: bool) -> Vec<Violation> {
    let task_low = task.to_lowercase();
    let mut out = Vec::new();
    for claim in extract_claims(text) {
        let low = claim.value.to_lowercase();
        if task_low.contains(&low) {
            continue;
        }
        if ev.mentions(&claim.value) {
            continue;
        }
        match claim.kind {
            ClaimKind::Path => {
                if check_fs && Path::new(&claim.value).exists() {
                    continue;
                }
                out.push(Violation {
                    why: if check_fs {
                        "путь не виден на экране и не существует на диске".into()
                    } else {
                        "путь не подтверждён наблюдением".into()
                    },
                    claim,
                });
            }
            ClaimKind::Url => out.push(Violation {
                why: "ссылка не встречается ни в задаче, ни на экране".into(),
                claim,
            }),
            ClaimKind::Element => out.push(Violation {
                why: "такого элемента нет в UI-дереве и в тексте экрана".into(),
                claim,
            }),
        }
    }
    out
}

/// Итоговый отчёт нельзя объявлять успехом без подтверждённых шагов:
/// «Do → Verify → Report», где Report опирается на Verify.
pub fn report_is_grounded(report: &str, verified_steps: usize) -> Result<(), String> {
    if verified_steps > 0 {
        return Ok(());
    }
    let low = report.to_lowercase();
    let success = [
        "готово",
        "выполнено",
        "успешно",
        "сделал",
        "настроил",
        "установил",
        "отправил",
        "done",
        "success",
    ]
    .iter()
    .any(|k| low.contains(k));
    if success {
        Err("отчёт заявляет успех, но ни один шаг не был подтверждён проверкой".into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Наблюдение как его видит агент: заголовок окна + элементы + OCR.
    fn screen() -> Evidence {
        Evidence::from_prompt(
            "ЭКРАН 1920x1080. Активное окно: \"Steam\"\n\
             ЭЛЕМЕНТЫ UI (name | type | центр x,y | размер):\n\
             - Библиотека | Button | 100,50 | 80x20\n\
             - Установить | Button | 400,600 | 120x40\n\
             ТЕКСТ НА ЭКРАНЕ (OCR):\n\
             - Свободно 512 ГБ\n",
        )
    }

    // ---- 5 тестов «ловля галлюцинаций» ----

    #[test]
    fn hallucination_1_invented_file_path_is_caught() {
        let ev = screen();
        let claim = r"Игра установлена в C:\Games\Fable5\fable.exe, запускаю";
        let v = check_claims(claim, &ev, "поставь мне Fable 5", false);
        assert_eq!(v.len(), 1, "должен поймать выдуманный путь: {v:?}");
        assert_eq!(v[0].claim.kind, ClaimKind::Path);
    }

    #[test]
    fn hallucination_2_invented_button_is_caught() {
        let ev = screen();
        // Кнопки «Продолжить» на экране нет — есть «Установить».
        let v = check_claims(
            "Нажимаю «Продолжить», чтобы двинуться дальше",
            &ev,
            "",
            false,
        );
        assert_eq!(v.len(), 1, "{v:?}");
        assert_eq!(v[0].claim.kind, ClaimKind::Element);
        // А реально существующая кнопка претензий не вызывает.
        assert!(check_claims("Нажимаю «Установить»", &ev, "", false).is_empty());
    }

    #[test]
    fn hallucination_3_invented_url_is_caught() {
        let ev = screen();
        let v = check_claims(
            "Открываю https://fable-download.example.com/setup",
            &ev,
            "поставь Fable 5",
            false,
        );
        assert_eq!(v.len(), 1, "{v:?}");
        assert_eq!(v[0].claim.kind, ClaimKind::Url);
        // URL, который назвал сам пользователь, — законный факт.
        assert!(check_claims(
            "Открываю https://store.steampowered.com",
            &ev,
            "купи игру на https://store.steampowered.com",
            false
        )
        .is_empty());
    }

    #[test]
    fn hallucination_4_success_report_without_verification_is_rejected() {
        assert!(report_is_grounded("Готово, реклама настроена и запущена", 0).is_err());
        assert!(report_is_grounded("Готово, реклама настроена", 3).is_ok());
        // Честный отчёт о неудаче пропускаем: это не галлюцинация.
        assert!(report_is_grounded("Не смог: нет доступа к Ads Manager", 0).is_ok());
    }

    #[test]
    fn hallucination_5_invented_tool_output_is_caught() {
        let mut ev = screen();
        // Агент «процитировал» вывод команды, которого не было.
        let fake = r"Команда показала, что файл лежит в /opt/fable/data/save.bin";
        assert_eq!(check_claims(fake, &ev, "", false).len(), 1);
        // Тот же путь после реального вывода инструмента — уже факт.
        ev.add_tool_output("/opt/fable/data/save.bin");
        assert!(check_claims(fake, &ev, "", false).is_empty());
    }

    // ---- проверки на ложные срабатывания ----

    #[test]
    fn plain_russian_text_is_not_a_claim() {
        let ev = screen();
        let ok = "Вижу список игр, свободно 512 ГБ. Думаю нажать кнопку установки.";
        assert!(check_claims(ok, &ev, "", false).is_empty());
        assert!(extract_claims("просто текст без путей и ссылок").is_empty());
    }

    #[test]
    fn evidence_parses_phone_prompt() {
        let ev = Evidence::from_prompt(
            "ЭКРАН ТЕЛЕФОНА. Активное: com.google.android.youtube/.HomeActivity\n\
             ЭЛЕМЕНТЫ (текст | класс | центр):\n- Подписки | TextView | 500,1800 | кликабельно\n",
        );
        assert!(ev.mentions("подписки"));
        assert!(ev.mentions("youtube"));
        assert!(!ev.mentions("сохранить в галерею"));
    }

    #[test]
    fn unclosed_quote_does_not_panic_or_claim() {
        let ev = screen();
        let _ = check_claims("нажимаю «неполная кавычка", &ev, "", false);
        let _ = extract_claims("\"\"\"'''«»«");
        let _ = extract_claims(&"«".repeat(5000));
    }
}
