//! ReAct: Thought → Action → Observation, и только потом следующий шаг.
//!
//! Что здесь принципиально:
//!   - Observation ЗАПИСЫВАЕТСЯ ПОСЛЕ действия и берётся из реального
//!     наблюдения, а не из слов модели. Модель не может «объявить» результат;
//!   - трасса хранит последние 10 шагов и именно она уходит в промпт как
//!     рабочая память: больше — дорого и модель начинает путаться в порядке;
//!   - уверенность: если confidence < порога, агент НЕ действует, а задаёт
//!     уточняющий вопрос. Дешёвый вопрос лучше дорогого неверного клика.
//!
//! Альтернатива хранению трассы в БД: складывать в память (memory.db) каждый
//! шаг — уже делается для журнала, но для промпта нужен именно короткий
//! хвост в оперативке, без запросов к SQLite на каждом шаге.

use std::collections::VecDeque;

/// Сколько шагов агент держит «в голове». Требование: последние 10.
pub const MEMORY_STEPS: usize = 10;

/// Порог уверенности: ниже — уточняющий вопрос вместо действия.
pub const MIN_CONFIDENCE: f64 = 0.8;

#[derive(Debug, Clone)]
pub struct Step {
    pub n: u32,
    pub thought: String,
    pub action: String,
    /// Реальное наблюдение после действия (не текст модели).
    pub observation: String,
    pub confidence: f64,
    /// Результат self-verify: None = проверка не применима.
    pub verified: Option<bool>,
}

#[derive(Debug, Default)]
pub struct Trace {
    steps: VecDeque<Step>,
}

impl Trace {
    pub fn push(&mut self, step: Step) {
        if self.steps.len() == MEMORY_STEPS {
            self.steps.pop_front();
        }
        self.steps.push_back(step);
    }

    pub fn len(&self) -> usize {
        self.steps.len()
    }

    pub fn verified_count(&self) -> usize {
        self.steps
            .iter()
            .filter(|s| s.verified == Some(true))
            .count()
    }

    /// Сколько раз подряд повторяется одно и то же действие. Зацикливание —
    /// главный способ, которым агент сжигает бюджет впустую.
    pub fn repeats_of(&self, action: &str) -> u32 {
        self.steps
            .iter()
            .rev()
            .take_while(|s| s.action == action)
            .count() as u32
    }

    /// Хвост трассы для промпта в формате ReAct.
    pub fn render(&self) -> String {
        if self.steps.is_empty() {
            return String::new();
        }
        let mut s = String::from("ПОСЛЕДНИЕ ШАГИ (ReAct, максимум 10):\n");
        for st in &self.steps {
            s.push_str(&format!(
                "#{n} Thought: {t} (уверенность {c:.2})\n   Action: {a}\n   Observation: {o}{v}\n",
                n = st.n,
                c = st.confidence,
                t = trim(&st.thought, 200),
                a = trim(&st.action, 200),
                o = trim(&st.observation, 300),
                v = match st.verified {
                    Some(true) => " [проверено]",
                    Some(false) => " [проверка НЕ подтвердила]",
                    None => "",
                }
            ));
        }
        s
    }
}

/// Решение о том, можно ли действовать при такой уверенности.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    Act,
    /// Спросить человека: текст вопроса.
    Clarify(String),
}

/// Проверка уверенности. Уточняющий вопрос обязателен при confidence < 0.8,
/// но не для действий, которые сами являются вопросом или завершением:
/// иначе агент зациклится на «уточни уточнение».
pub fn confidence_gate(confidence: f64, asks_human: bool, suggested: &str, action: &str) -> Gate {
    if asks_human || confidence >= MIN_CONFIDENCE {
        return Gate::Act;
    }
    // NaN от модели трактуем как низкую уверенность: молча действовать нельзя.
    let c = if confidence.is_finite() {
        confidence
    } else {
        0.0
    };
    let q = if suggested.trim().is_empty() {
        format!(
            "Не уверен (оценка {:.0}%), что делать дальше. Собирался: {}\nПодскажи: продолжать так или как иначе?",
            c * 100.0,
            trim(action, 200)
        )
    } else {
        suggested.trim().to_string()
    };
    Gate::Clarify(q)
}

fn trim(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(n: u32, action: &str) -> Step {
        Step {
            n,
            thought: "думаю".into(),
            action: action.into(),
            observation: "экран".into(),
            confidence: 0.9,
            verified: Some(true),
        }
    }

    #[test]
    fn trace_keeps_only_last_ten() {
        let mut t = Trace::default();
        for i in 0..25 {
            t.push(step(i, &format!("a{i}")));
        }
        assert_eq!(t.len(), MEMORY_STEPS);
        let r = t.render();
        assert!(r.contains("#24"));
        assert!(!r.contains("#14"), "старые шаги должны вытесняться");
        assert_eq!(t.verified_count(), MEMORY_STEPS);
    }

    #[test]
    fn repeats_are_counted() {
        let mut t = Trace::default();
        t.push(step(1, "click A"));
        t.push(step(2, "click B"));
        t.push(step(3, "click B"));
        assert_eq!(t.repeats_of("click B"), 2);
        assert_eq!(t.repeats_of("click A"), 0);
    }

    #[test]
    fn low_confidence_asks_instead_of_acting() {
        assert_eq!(confidence_gate(0.95, false, "", "click"), Gate::Act);
        match confidence_gate(0.4, false, "", "click «Оплатить»") {
            Gate::Clarify(q) => assert!(q.contains("Оплатить")),
            Gate::Act => panic!("при 0.4 действовать нельзя"),
        }
        // Уже спрашиваем человека — второй вопрос не нужен.
        assert_eq!(confidence_gate(0.1, true, "", "ask_user"), Gate::Act);
        // NaN — тоже повод спросить, а не «считать за 1.0».
        assert!(matches!(
            confidence_gate(f64::NAN, false, "", "click"),
            Gate::Clarify(_)
        ));
    }
}
