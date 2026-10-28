//! Разбор сбоя и перепланирование остатка.
//!
//! После отката к чекпоинту граф снова такой, каким был до плохого шага.
//! Здесь по трассе и тексту ошибки находится причина, проваленный узел
//! возвращается в работу, а потомки, которые из-за него пропускались,
//! тоже. Новый план обязан пройти ту же проверку графа, что и исходный.
//! Текст отчёта модели причиной не считается: берём последнюю конкретную
//! строку сбоя из трассы или лога.

use crate::agent::graph::{NodeState, TaskGraph};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cause {
    pub summary: String,
}

/// Последняя конкретная причина. Смотрим трассу и лог с конца и берём
/// строку, в которой есть отказ, а не общий «что-то пошло не так».
pub fn analyze(trace: &str, log: &str) -> Cause {
    let mut lines: Vec<&str> = Vec::new();
    lines.extend(trace.lines());
    lines.extend(log.lines());
    let marker = [
        "ошибка:",
        "отклонено",
        "не подтверж",
        "panic",
        "таймаут",
        "failed",
        "error:",
    ];
    let found = lines.into_iter().rev().find_map(|line| {
        let t = line.trim();
        if t.is_empty() {
            return None;
        }
        let low = t.to_lowercase();
        if marker.iter().any(|m| low.contains(m)) {
            Some(trim_cause(t))
        } else {
            None
        }
    });
    Cause {
        summary: found.unwrap_or_else(|| "шаг не подтверждён наблюдением".into()),
    }
}

fn trim_cause(line: &str) -> String {
    let t = line.trim();
    let t = t.strip_prefix("Observation:").unwrap_or(t).trim();
    const MAX: usize = 240;
    if t.chars().count() <= MAX {
        t.to_string()
    } else {
        t.chars().take(MAX).collect()
    }
}

/// Остаток задачи после сбоя. Выполненное не переигрываем. Проваленный узел
/// и всё, что из-за него было пропущено, снова ожидает выполнения, а в
/// заметке лежит причина — её увидит следующий шаг. Зависимости те же,
/// поэтому порядок по-прежнему можно доказать.
pub fn replan_remaining(graph: &TaskGraph, cause: &Cause) -> Result<TaskGraph, String> {
    let mut g = graph.clone();
    // Возвращаем в работу только те, кого ещё не возвращали слишком часто:
    // иначе три отказа подряд обнуляли попытки, узел брался с тем же
    // критерием и тем же способом, и так до лимита шагов.
    let had_failed = g
        .nodes
        .iter()
        .any(|n| n.state == NodeState::Failed && n.rounds < crate::agent::graph::MAX_NODE_ROUNDS);
    if had_failed {
        for n in &mut g.nodes {
            if n.state == NodeState::Failed && n.rounds < crate::agent::graph::MAX_NODE_ROUNDS {
                n.state = NodeState::Pending;
                n.attempts = 0;
                n.rounds += 1;
                n.note = format!(
                    "повтор {} после сбоя: {} — прежний способ не сработал, попробуй другой",
                    n.rounds, cause.summary
                );
            }
        }
        for n in &mut g.nodes {
            if n.state == NodeState::Skipped {
                n.state = NodeState::Pending;
                n.attempts = 0;
                n.note = format!("снова в плане: {}", cause.summary);
            }
        }
    } else if let Some(n) = g
        .nodes
        .iter_mut()
        .filter(|n| n.state == NodeState::Pending)
        .max_by_key(|n| n.attempts)
    {
        n.note = cause.summary.clone();
    }
    g.validate()?;
    Ok(g)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::graph::TaskGraph;
    use crate::logic::{self, Policies, Verdict};
    use std::collections::HashMap;

    #[test]
    fn cause_comes_from_the_trace_not_from_a_success_claim() {
        let trace = "\
#1 Observation: клик прошёл
#2 Observation: ошибка: установщик не запустился
";
        let log = "Готово, всё установлено\n";
        let cause = analyze(trace, log);
        assert!(
            cause.summary.to_lowercase().contains("установщик"),
            "{}",
            cause.summary
        );
        assert!(!cause.summary.to_lowercase().contains("готово"));
    }

    #[test]
    fn failed_node_is_replanned_and_can_still_be_proved() {
        let mut g = TaskGraph::linear(
            "установить",
            &[
                ("скачать".into(), "файл в загрузках".into()),
                ("установить".into(), "ярлык на столе".into()),
                ("запустить".into(), "окно игры".into()),
            ],
        );
        g.validate().expect("исходный план");
        g.mark_done("n1", "файл виден");
        let mut terminal = false;
        for _ in 0..crate::agent::graph::MAX_NODE_ATTEMPTS {
            terminal = g.mark_attempt_failed("n2", "ошибка: установщик не запустился");
        }
        assert!(terminal);
        assert!(g
            .nodes
            .iter()
            .any(|n| n.id == "n3" && n.state == NodeState::Skipped));

        let cause = analyze(&g.nodes[1].note, "лог: ошибка: установщик не запустился");
        let next = replan_remaining(&g, &cause).expect("остаток плана");
        assert!(
            matches!(logic::check_plan(&next, &Policies::new()), Verdict::Ok),
            "Z3 должен принять перепланированный остаток"
        );
        assert!(next.nodes.iter().all(|n| n.state != NodeState::Failed));
        assert!(next.nodes.iter().all(|n| n.state != NodeState::Skipped));
        assert_eq!(
            next.nodes.iter().find(|n| n.id == "n1").unwrap().state,
            NodeState::Done,
            "уже сделанное не переигрываем"
        );

        let mut done = next;
        done.mark_done("n2", "ярлык виден");
        done.mark_done("n3", "окно видно");
        let observed = HashMap::from([
            ("n1".into(), true),
            ("n2".into(), true),
            ("n3".into(), true),
        ]);
        assert!(matches!(
            logic::check_solution(&done, &observed),
            Verdict::Ok
        ));
    }
    #[test]
    fn node_is_not_reopened_forever() {
        let mut g = TaskGraph::linear(
            "установить",
            &[
                ("скачать".into(), "файл в загрузках".into()),
                ("установить".into(), "ярлык на столе".into()),
            ],
        );
        g.validate().expect("исходный план");
        let cause = Cause {
            summary: "не подтверждено наблюдением".into(),
        };
        // Каждый круг: попытки кончились — переплан вернул узел в работу.
        for round in 1..=crate::agent::graph::MAX_NODE_ROUNDS {
            for _ in 0..crate::agent::graph::MAX_NODE_ATTEMPTS {
                g.mark_attempt_failed("n1", "не подтверждено наблюдением");
            }
            g = replan_remaining(&g, &cause).expect("остаток плана");
            assert_eq!(
                g.nodes.iter().find(|n| n.id == "n1").unwrap().rounds,
                round,
                "круг должен считаться"
            );
        }
        // Круги исчерпаны: узел остаётся провалившимся, а не идёт по новой.
        for _ in 0..crate::agent::graph::MAX_NODE_ATTEMPTS {
            g.mark_attempt_failed("n1", "не подтверждено наблюдением");
        }
        let after = replan_remaining(&g, &cause).expect("остаток плана");
        assert_eq!(
            after.nodes.iter().find(|n| n.id == "n1").unwrap().state,
            NodeState::Failed,
            "бесконечный повтор того же способа запрещён"
        );
        assert!(after
            .nodes
            .iter()
            .any(|n| n.id == "n2"
                && (n.state == NodeState::Skipped || n.state == NodeState::Pending)));
    }

    #[test]
    fn replan_note_tells_model_to_change_approach() {
        let mut g = TaskGraph::linear("дело", &[("шаг".into(), "видно окно".into())]);
        for _ in 0..crate::agent::graph::MAX_NODE_ATTEMPTS {
            g.mark_attempt_failed("n1", "не подтверждено наблюдением");
        }
        let next = replan_remaining(
            &g,
            &Cause {
                summary: "не подтверждено наблюдением".into(),
            },
        )
        .expect("остаток");
        let note = &next.nodes.iter().find(|n| n.id == "n1").unwrap().note;
        assert!(note.contains("другой"), "{note}");
    }
}
