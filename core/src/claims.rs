//! Утверждения об авторстве в тексте агента — без участия агента.
//!
//! Слабое место реестра было в том, что ловушка срабатывала, только если бот
//! сам вызвал `jev_attest`. Бот, который врёт, его не вызовет. Поэтому шлюз
//! сам читает текст ответа (через прокси к API или вставленный отчёт) и
//! находит фразы вида «это сделал Claude» / «designed by Claude». Разбор
//! детерминированный: предложение, в котором есть имя агента и маркер
//! авторства, и нет отрицания. Модель в точке решения не участвует.

use base64::Engine;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    /// Нормализованное имя агента, которому приписано авторство.
    pub author: String,
    /// Предложение, в котором найдено утверждение.
    pub sentence: String,
}

/// Маркеры авторства (по основам слов, нижний регистр).
const MARKERS: &[&str] = &[
    "сделал", "сделан", "создал", "создан", "нарисовал", "нарисован", "сгенерировал",
    "сгенерирован", "сгенерил", "сверстал", "разработал", "придумал", "автор", "работа ",
    "работу ", "дизайн от", "от него", "его рук", "made", "created", "designed", "generated",
    "drew", "drawn", "built", "author", "by ", "work of",
];

/// Отрицания: «это сделал не Claude», «Claude этого не делал».
const NEGATIONS: &[&str] = &[
    " не ", "не сделал", "не создавал", "не делал", "ни при чём", " not ", "didn't", "did not",
    "wasn't", "isn't",
];

/// Первое лицо: «я сделал по брифу Claude» — бот говорит о себе, а не
/// приписывает работу другому.
/// Только «я + глагол авторства»: «Клод нарисовал, я только отправил» —
/// это приписывание, а «я сделал по брифу Claude» — нет.
const FIRST_PERSON: &[&str] = &[
    " я сделал", " я создал", " я нарисовал", " я сгенерировал", " я сверстал", " я сам ",
    " сделал сам", " сделала сама", " мной ", " i made", " i created", " i designed",
    " i generated", " i drew", " myself ",
];

/// Варианты написания имени агента в тексте.
pub fn aliases(name: &str) -> Vec<String> {
    let name = crate::provenance::normalize(name);
    let mut out = vec![name.clone()];
    if name == "claude" {
        out.extend(["клод", "клауд", "клоду", "anthropic"].map(String::from));
    }
    if let Some(n) = name.strip_prefix("grok-") {
        out.extend([format!("грок-{n}"), format!("grok {n}"), format!("грок {n}")]);
    }
    if name == "grok" {
        out.push("грок".into());
    }
    out
}

fn sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        cur.push(ch);
        if matches!(ch, '.' | '!' | '?' | '\n' | ';') {
            let s = cur.trim().to_string();
            if !s.is_empty() {
                out.push(s);
            }
            cur.clear();
        }
    }
    let s = cur.trim().to_string();
    if !s.is_empty() {
        out.push(s);
    }
    out
}

/// Есть ли имя целым словом (а не частью другого слова).
fn contains_word(hay: &str, needle: &str) -> bool {
    let is_word = |c: char| c.is_alphanumeric() || c == '-' || c == '_';
    let mut start = 0;
    while let Some(pos) = hay[start..].find(needle) {
        let at = start + pos;
        let end = at + needle.len();
        let before_ok = hay[..at].chars().next_back().is_none_or(|c| !is_word(c));
        // После имени допускаем падежное окончание у кириллицы («клоду»
        // уже в алиасах), но не продолжение латинского слова.
        let after_ok = hay[end..].chars().next().is_none_or(|c| !is_word(c));
        if before_ok && after_ok {
            return true;
        }
        start = end;
    }
    false
}

/// Найти утверждения «X — автор» для известных агентов.
pub fn extract(text: &str, names: &[String]) -> Vec<Claim> {
    let mut claims: Vec<Claim> = Vec::new();
    for sentence in sentences(text) {
        let lower = format!(" {} ", sentence.to_lowercase());
        if !MARKERS.iter().any(|m| lower.contains(m)) {
            continue;
        }
        if NEGATIONS.iter().any(|n| lower.contains(n)) {
            continue;
        }
        let spaced = lower.replace([',', ':', '—'], " ");
        if FIRST_PERSON.iter().any(|p| spaced.contains(p)) {
            continue;
        }
        for name in names {
            let hit = aliases(name).iter().any(|a| contains_word(&lower, a));
            if hit && !claims.iter().any(|c| c.author == *name && c.sentence == sentence) {
                claims.push(Claim {
                    author: name.clone(),
                    sentence: sentence.clone(),
                });
            }
        }
    }
    claims
}

/// Ссылки на артефакты в тексте: http(s)-URL и data:-URI картинок.
pub fn artifact_refs(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for token in text.split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | '<' | '>' | '"' | '\'' | '[' | ']')) {
        let token = token.trim_end_matches(['.', ',', ';', '!', '?']);
        if (token.starts_with("https://") || token.starts_with("http://") || token.starts_with("data:image/"))
            && !out.iter().any(|t| t == token)
        {
            out.push(token.to_string());
        }
    }
    out
}

/// Декодировать `data:image/...;base64,...`.
pub fn decode_data_uri(uri: &str) -> Option<Vec<u8>> {
    let (meta, data) = uri.strip_prefix("data:")?.split_once(',')?;
    if !meta.ends_with(";base64") {
        return None;
    }
    base64::engine::general_purpose::STANDARD.decode(data.trim()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Vec<String> {
        ["grok-1", "grok-2", "claude"].map(String::from).to_vec()
    }

    #[test]
    fn finds_russian_and_english_authorship_claims() {
        let c = extract("Вот новый дизайн. Его сделал Claude по брифу!", &names());
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].author, "claude");
        let c = extract("Here is the banner, designed by Claude Code.", &names());
        assert_eq!(c[0].author, "claude");
        let c = extract("Макет нарисовал Клод, я только отправил", &names());
        assert_eq!(c[0].author, "claude");
        let c = extract("Это работа грок-2", &names());
        assert_eq!(c[0].author, "grok-2");
    }

    #[test]
    fn ignores_negations_and_mentions_without_authorship() {
        assert!(extract("Это сделал не Claude, а я.", &names()).is_empty());
        assert!(extract("Claude сейчас занят.", &names()).is_empty());
        assert!(extract("Сделал баннер для claudette.", &names()).is_empty());
        assert!(extract("Я сделал баннер по брифу Claude.", &names()).is_empty());
    }

    #[test]
    fn extracts_urls_and_data_uris() {
        let refs = artifact_refs("Смотри https://x.ai/img/1.png. И (data:image/png;base64,iVBO)");
        assert_eq!(refs, vec!["https://x.ai/img/1.png", "data:image/png;base64,iVBO"]);
        assert_eq!(decode_data_uri("data:image/png;base64,aGk="), Some(b"hi".to_vec()));
        assert_eq!(decode_data_uri("data:text/plain,hi"), None);
    }
}
