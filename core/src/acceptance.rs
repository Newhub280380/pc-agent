//! Критерии приёмки до старта задачи.
//!
//! План говорит, как идти. Критерии говорят, когда можно остановиться.
//! Их разбираем из текста задачи до первого шага: цитата, сравнение числа
//! или содержательные слова. Пустую и «ок» не превращаем в критерий.
//! Задача закрывается только когда каждый критерий виден на экране.
//! Иначе статус остаётся незакрытым, даже если модель прислала отчёт.

use crate::agent::grounding::Evidence;
use crate::agent::verify::{self, criterion_observed};

/// Измеримые условия задачи. Порядок стабильный, повторы выкинуты.
pub fn decompose(task: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in verify::measurable_parts(task) {
        push_unique(&mut out, part);
    }
    if out.is_empty() {
        let whole = task.trim();
        if verify::is_measurable(whole) {
            push_unique(&mut out, whole.to_string());
        }
    }
    out
}

/// Критерии, которых на этом экране нет. Пустой список критериев —
/// тоже незакрытие: доказать «готово» нечем.
pub fn unmet(criteria: &[String], evidence: &Evidence) -> Vec<String> {
    if criteria.is_empty() {
        return vec!["нет измеримых критериев приёмки".into()];
    }
    criteria
        .iter()
        .filter(|c| !criterion_observed(c, evidence))
        .cloned()
        .collect()
}

pub fn all_met(criteria: &[String], evidence: &Evidence) -> bool {
    unmet(criteria, evidence).is_empty()
}

fn push_unique(out: &mut Vec<String>, item: String) {
    let item = item.trim().to_string();
    if item.is_empty() || out.iter().any(|e| e == &item) {
        return;
    }
    out.push(item);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_and_comparisons_become_criteria() {
        let c = decompose("поставь игру: на экране «Готово» и свободно > 50 ГБ");
        assert!(c.iter().any(|x| x.contains("Готово")), "{c:?}");
        assert!(c.iter().any(|x| x.contains('>')), "{c:?}");
    }

    #[test]
    fn vague_task_has_no_criteria() {
        assert!(decompose("").is_empty());
        assert!(decompose("ок").is_empty());
    }

    #[test]
    fn finish_requires_every_criterion_on_screen() {
        let c = decompose("в окне видно «Готово» и свободно > 50");
        let seen = Evidence::from_prompt("- статус | Готово\nсвободно 80 ГБ\n");
        assert!(all_met(&c, &seen), "не закрылись: {:?}", unmet(&c, &seen));
        let thin = Evidence::from_prompt("- статус | Готово\nсвободно 10 ГБ\n");
        assert!(!all_met(&c, &thin));
        let _ = seen;
    }
}
