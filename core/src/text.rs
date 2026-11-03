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
    const QUOTES: [char; 3] = ['"', '\'', '`'];
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        // Значение после ключа: password": "hunter2" → заглушка.
        if let Some(key) = KEYS.iter().find(|k| key_at(&chars, i, k)) {
            let len = key.chars().count();
            let mut j = i + len;
            // Кавычка, закрывающая имя поля, и пробелы до разделителя.
            if chars.get(j).is_some_and(|c| QUOTES.contains(c)) {
                j += 1;
            }
            while chars.get(j).is_some_and(|c| c.is_whitespace()) {
                j += 1;
            }
            let has_value = chars.get(j).is_some_and(|c| *c == ':' || *c == '=');
            if has_value {
                j += 1;
                while chars.get(j).is_some_and(|c| c.is_whitespace()) {
                    j += 1;
                }
                // Всё до начала значения — имя поля и разделитель, его видно.
                out.extend(&chars[i..j]);
                match chars.get(j) {
                    // Значение в кавычках: внутри может быть и пробел,
                    // и экранированная кавычка — затираем до закрывающей.
                    Some(q) if QUOTES.contains(q) => {
                        let q = *q;
                        out.push(q);
                        let mut m = j + 1;
                        while m < chars.len() && chars[m] != q {
                            m += if chars[m] == '\\' { 2 } else { 1 };
                        }
                        if m > j + 1 {
                            out.push_str("***");
                        }
                        i = m.min(chars.len());
                    }
                    // Голое значение: до пробела, запятой или кавычки
                    // следующего поля, иначе затрём разделители в журнале.
                    // Точка с запятой в список не входит: в `abc;def` она
                    // бывает частью самого секрета, и хвост утёк бы в лог.
                    Some(_) => {
                        let mut m = j;
                        while chars.get(m).is_some_and(|c| {
                            !c.is_whitespace() && !",}]".contains(*c) && !QUOTES.contains(c)
                        }) {
                            m += 1;
                        }
                        if m > j {
                            out.push_str("***");
                        }
                        i = m;
                    }
                    None => i = j,
                }
                continue;
            }
            out.extend(&chars[i..i + len]);
            i += len;
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
        // Голое значение не должно съедать границу следующего поля.
        let bare = mask_secrets("{\"password\": hunter2, \"next\": \"click\"}");
        assert!(!bare.contains("hunter2"), "{bare}");
        assert!(bare.contains(", \"next\""), "{bare}");
        // Экранированная кавычка внутри значения не открывает хвост пароля.
        let esc = mask_secrets("{\"password\":\"first\\\"secret suffix\"}");
        assert!(!esc.contains("suffix"), "{esc}");
        // Значение с пробелом в начале тоже затирается.
        let lead = mask_secrets("{\"password\": \"  topsecret\"}");
        assert!(!lead.contains("topsecret"), "{lead}");
        // Точка с запятой может быть внутри секрета — хвост не оставляем.
        assert_eq!(mask_secrets("password=abc;def"), "password=***");
        let m = mask_secrets("{\"password\": \"correct horse battery staple\"}");
        assert!(!m.contains("horse"), "{m}");
        assert!(!m.contains("staple"), "{m}");
    }
}
