//! Z3-слой мышления: проверка планов и действий формальной логикой.
//!
//! LLM может придумать план с циклом зависимостей, превышением лимита
//! или запрещённым политикой шагом. Z3 доказывает это за миллисекунды
//! офлайн: без токенов, без сети, без галлюцинаций. Слой вшит в ядро -
//! никаких шлюзов и портов, прямой вызов функции внутри бинарника.

use crate::agent::graph::TaskGraph;
use z3::ast::{Ast, Int};
use z3::{Config, Context, SatResult, Solver};

/// Политики гибридного мышления: то, что LLM решать не имеет права.
#[derive(Debug, Clone)]
pub struct Policies {
    pub max_plan_steps: usize,
    pub forbidden_actions: Vec<String>,
}

impl Policies {
    pub fn new() -> Self {
        Self {
            max_plan_steps: 120,
            forbidden_actions: vec![],
        }
    }
}

impl Default for Policies {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, PartialEq)]
pub enum Verdict {
    Ok,
    Violation(String),
}

/// Проверка графа задач. SAT-ядро: каждому узлу - целочисленная позиция,
/// каждая зависимость требует строгого порядка. Цикл в зависимостях делает
/// модель противоречивой - Z3 это доказывает, а не чувствует.
pub fn check_plan(graph: &TaskGraph, pol: &Policies) -> Verdict {
    if graph.nodes.len() > pol.max_plan_steps {
        return Verdict::Violation(format!(
            "план длиннее лимита: {} узлов",
            graph.nodes.len()
        ));
    }
    for n in &graph.nodes {
        if n.deps.iter().any(|d| d == &n.id) {
            return Verdict::Violation(format!("узел зависит от себя: {}", n.id));
        }
        let low = n.goal.to_lowercase();
        if let Some(f) = pol
            .forbidden_actions
            .iter()
            .find(|f| low.contains(&f.to_lowercase()))
        {
            return Verdict::Violation(format!(
                "в плане запрещённое политикой действие: {}",
                f
            ));
        }
    }
    let cfg = Config::default();
    let ctx = Context::new(&cfg);
    let solver = Solver::new(&ctx);
    let pos: Vec<Int> = graph
        .nodes
        .iter()
        .map(|n| Int::new_const(&ctx, format!("pos_{}", n.id)))
        .collect();
    for (i, n) in graph.nodes.iter().enumerate() {
        for d in &n.deps {
            if let Some(j) = graph.nodes.iter().position(|m| &m.id == d) {
                solver.assert(&pos[i].gt(&pos[j]));
            }
        }
    }
    match solver.check() {
        SatResult::Sat | SatResult::Unknown => Verdict::Ok,
        SatResult::Unsat => {
            Verdict::Violation("граф зависимостей противоречив (цикл)".into())
        }
    }
}

/// Предстартовый фильтр ОДНОГО действия: быстрая проверка политики
/// без обращения к LLM. Вызывается перед каждым шагом агента.
pub fn gate_action(action: &str, pol: &Policies) -> Verdict {
    let low = action.to_lowercase();
    if let Some(f) = pol
        .forbidden_actions
        .iter()
        .find(|f| low.contains(&f.to_lowercase()))
    {
        return Verdict::Violation(format!("запрещено политикой: {}", f));
    }
    Verdict::Ok
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linear(n: usize) -> TaskGraph {
        let steps: Vec<_> = (0..n)
            .map(|i| (format!("узел {}", i), "готово".to_string()))
            .collect();
        TaskGraph::linear("цель", &steps)
    }

    #[test]
    fn consistent_plan_passes() {
        let mut g = linear(3);
        g.validate().unwrap();
        assert_eq!(check_plan(&g, &Policies::new()), Verdict::Ok);
    }

    #[test]
    fn forbidden_action_is_rejected() {
        let mut pol = Policies::new();
        pol.forbidden_actions.push("удалить всё".into());
        assert!(matches!(
            gate_action("Удалить всё на диске", &pol),
            Verdict::Violation(_)
        ));
    }

    #[test]
    fn oversized_plan_is_rejected() {
        let mut g = linear(10);
        g.validate().unwrap();
        let mut pol = Policies::new();
        pol.max_plan_steps = 5;
        assert!(matches!(check_plan(&g, &pol), Verdict::Violation(_)));
    }
}
