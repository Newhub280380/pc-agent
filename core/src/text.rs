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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_kept_and_long_text_is_cut() {
        assert_eq!(truncate("привет", 10), "привет");
        assert_eq!(truncate("привет", 3), "при…");
    }
}
