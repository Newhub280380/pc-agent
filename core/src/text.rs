//! Обрезка строк для промптов и журналов.
//!
//! Раньше одна и та же функция была скопирована в восприятие, рефлексию,
//! исполнитель, self-verify и цикл агента. Расхождение было мелким
//! (многоточие то ставили, то нет) и ломало ожидание «везде один и тот же вид».

pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n).collect();
        out.push('…');
        out
    }
}

/// Затирает похожее на секрет перед записью в журнал.
///
/// В логи уходит сырой ответ модели, а модель читает экран: там бывает
/// код из SMS и пароль, который человек только что ввёл. Журнал лежит на
/// диске неделями, поэтому цифровые коды и значения полей вроде password
/// заменяются заглушкой.
pub fn mask_secrets(s: &str) -> String {
    const KEYS: [&str; 8] = [
        "password",
        "passwd",
        "pass",
        "secret",
        "token",
        "otp",
        "code",
        "пароль",
    ];
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let lower: String = s.to_lowercase();
    let lower: Vec<char> = lower.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        // Значение после ключа: password": "hunter2" → заглушка.
        let key = KEYS.iter().find(|k| {
            let k: Vec<char> = k.chars().collect();
            lower[i..].starts_with(&k[..])
        });
        if let Some(key) = key {
            let len = key.chars().count();
            out.extend(&chars[i..i + len]);
            i += len;
            let mut j = i;
            while j < chars.len() && (chars[j].is_whitespace() || "\":='`".contains(chars[j])) {
                out.push(chars[j]);
                j += 1;
            }
            if j > i && j < chars.len() && !chars[j].is_whitespace() {
                while j < chars.len() && !chars[j].is_whitespace() && !"\",'}`".contains(chars[j]) {
                    j += 1;
                }
                out.push_str("***");
            }
            i = j;
            continue;
        }
        // Длинная цифровая последовательность — это код или номер.
        if chars[i].is_ascii_digit() {
            let mut j = i;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            if j - i >= 4 {
                out.push_str("***");
            } else {
                out.extend(&chars[i..j]);
            }
            i = j;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_kept_and_long_text_is_cut() {
        assert_eq!(truncate("привет", 10), "привет");
        assert_eq!(truncate("привет", 3), "при…");
    }

    #[test]
    fn codes_and_passwords_are_masked() {
        assert_eq!(
            mask_secrets("введи код 483921 в поле"),
            "введи код *** в поле"
        );
        let m = mask_secrets("{\"password\": \"hunter2\", \"next\": \"click\"}");
        assert!(!m.contains("hunter2"), "{m}");
        assert!(m.contains("click"), "{m}");
        assert_eq!(mask_secrets("шаг 2 из 5"), "шаг 2 из 5");
    }
}
