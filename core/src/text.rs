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
    // Регистр сравниваем посимвольно: привести всю строку к нижнему регистру
    // нельзя, у части букв (İ) при этом меняется длина, и индексы разъезжаются.
    fn key_at(chars: &[char], i: usize, key: &str) -> bool {
        let mut n = i;
        for kc in key.chars() {
            match chars.get(n) {
                Some(c) if c.to_lowercase().eq(std::iter::once(kc)) => n += 1,
                _ => return false,
            }
        }
        true
    }
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        // Значение после ключа: password": "hunter2" → заглушка.
        let key = KEYS.iter().find(|k| key_at(&chars, i, k));
        if let Some(key) = key {
            let len = key.chars().count();
            out.extend(&chars[i..i + len]);
            i += len;
            let mut j = i;
            let mut quote = None;
            while j < chars.len() && (chars[j].is_whitespace() || "\":='`".contains(chars[j])) {
                if "\"'`".contains(chars[j]) {
                    quote = Some(chars[j]);
                }
                out.push(chars[j]);
                j += 1;
            }
            if j > i && j < chars.len() && Some(chars[j]) != quote {
                // В кавычках секрет может быть из нескольких слов — затираем
                // всё до закрывающей кавычки, а не первое слово.
                match quote {
                    Some(q) if !chars[j].is_whitespace() => {
                        while j < chars.len() && chars[j] != q {
                            j += 1;
                        }
                        out.push_str("***");
                    }
                    None if !chars[j].is_whitespace() => {
                        while j < chars.len()
                            && !chars[j].is_whitespace()
                            && !"\",'}`".contains(chars[j])
                        {
                            j += 1;
                        }
                        out.push_str("***");
                    }
                    _ => {}
                }
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

    /// Кривой ответ модели не должен ронять агента: у İ при приведении
    /// к нижнему регистру меняется длина. И пароль из нескольких слов
    /// затирается целиком, а не до первого пробела.
    #[test]
    fn odd_case_does_not_panic_and_multiword_value_is_masked() {
        assert_eq!(mask_secrets("İcode"), "İcode");
        let m = mask_secrets("{\"password\": \"correct horse battery staple\"}");
        assert!(!m.contains("horse"), "{m}");
        assert!(!m.contains("staple"), "{m}");
    }
}
