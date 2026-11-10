//! JEV-слой: вероятностный да/нет-фильтр действий в ядре.
//!
//! Байесовская оценка: если действие такого класса в прошлом вело к успеху
//! в 9 из 10 случаев — можно делать. Если статистика плохая или её мало,
//! честно говорим «не знаю» и передаём решение выше (человеку/LLM).
//! Чистая математика: миллисекунды, офлайн, ноль галлюцинаций.

use std::collections::HashMap;

/// Решение JEV.
#[derive(Debug, PartialEq)]
pub enum JevVerdict {
    Yes,
    No,
    /// Статистики недостаточно — эскалация выше по цепочке мышления.
    Unknown,
}

/// Статистика классов действий: успехи/провалы копятся из журнала шагов.
#[derive(Default)]
pub struct Jev {
    stats: HashMap<String, (u32, u32)>, // класс -> (success, fail)
    /// Минимум наблюдений, ниже которого вердикт Unknown.
    min_observations: u32,
    /// Порог вероятности успеха для Yes.
    threshold: f64,
}

impl Jev {
    pub fn new() -> Self {
        Self {
            stats: HashMap::new(),
            min_observations: 3,
            threshold: 0.6,
        }
    }

    /// Класс действия: глагол действия («клик», «ввести», «открыть»).
    fn class_of(action: &str) -> String {
        action
            .split_whitespace()
            .next()
            .unwrap_or("other")
            .to_lowercase()
    }

    /// Записываем исход шага: успех подкрепляет класс, провал ослабляет.
    pub fn observe(&mut self, action: &str, ok: bool) {
        let c = Self::class_of(action);
        let e = self.stats.entry(c).or_insert((0, 0));
        if ok {
            e.0 += 1;
        } else {
            e.1 += 1;
        }
    }

    /// Быстрый да/нет по накопленной статистике.
    pub fn ask(&self, action: &str) -> JevVerdict {
        let c = Self::class_of(action);
        if let Some(&(s, f)) = self.stats.get(&c) {
            let n = s + f;
            if n >= self.min_observations {
                let p = s as f64 / n as f64;
                return if p >= self.threshold {
                    JevVerdict::Yes
                } else {
                    JevVerdict::No
                };
            }
        }
        JevVerdict::Unknown
    }

    /// Уверенность класса действия (для отчётов и табло).
    pub fn confidence(&self, action: &str) -> Option<f64> {
        let c = Self::class_of(action);
        self.stats.get(&c).map(|&(s, f)| s as f64 / (s + f) as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_with_no_history() {
        let j = Jev::new();
        assert_eq!(j.ask("клик по кнопке"), JevVerdict::Unknown);
    }

    #[test]
    fn yes_after_good_history() {
        let mut j = Jev::new();
        for _ in 0..4 {
            j.observe("клик по кнопке", true);
        }
        j.observe("клик по кнопке", false);
        assert_eq!(j.ask("клик в меню"), JevVerdict::Yes);
    }

    #[test]
    fn no_after_bad_history() {
        let mut j = Jev::new();
        for _ in 0..3 {
            j.observe("ввести текст в поле", false);
        }
        assert_eq!(j.ask("ввести пароль"), JevVerdict::No);
    }
}
