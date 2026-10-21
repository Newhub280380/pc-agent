//! Z3-слой: доказательная проверка планов и решений.
//!
//! Солвер вшит в бинарник и вызывается напрямую. Отдельного режима
//! «без Z3» нет: раньше он был выключен фичей `z3logic`, а вместо него
//! работал второй обход графа, который повторял уже существующий
//! топологический порядок и не проверял решение задачи.
//!
//! Что доказывает Z3:
//!   - план: у узлов есть различные позиции и каждая зависимость
//!     стоит строго раньше узла. Цикл, петля и висячая связь — UNSAT;
//!   - закрытие узла: он становится выполненным только если критерий
//!     уже наблюдён на экране и все зависимости выполнены;
//!   - закрытие задачи: нет открытых и проваленных узлов, каждый
//!     выполненный узел подтверждён наблюдением, есть хотя бы один
//!     такой узел.
//!
//! Текст отчёта модели в формулы не входит. `Unknown` от солвера —
//! отказ, а не разрешение.

use crate::agent::graph::{NodeState, TaskGraph};
use std::collections::HashMap;
use z3::ast::{Ast, Bool, Int};
use z3::{Config, Context, SatResult, Solver};

/// Политики, которые LLM решать не имеет права.
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

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Ok,
    Violation(String),
}

/// План можно исполнять, только если он короче лимита, не содержит
/// запрещённых целей и Z3 находит расписание.
pub fn check_plan(graph: &TaskGraph, pol: &Policies) -> Verdict {
    if graph.nodes.is_empty() {
        return Verdict::Violation("план пуст".into());
    }
    if graph.nodes.len() > pol.max_plan_steps {
        return Verdict::Violation(format!("план длиннее лимита: {} узлов", graph.nodes.len()));
    }
    for n in &graph.nodes {
        let low = n.goal.to_lowercase();
        if let Some(f) = pol
            .forbidden_actions
            .iter()
            .find(|f| low.contains(&f.to_lowercase()))
        {
            return Verdict::Violation(format!("в плане запрещённое политикой действие: {f}"));
        }
    }
    prove_schedule(graph)
}

/// Доказательство, что зависимости образуют порядок, а не цикл.
pub fn prove_schedule(graph: &TaskGraph) -> Verdict {
    if graph.nodes.is_empty() {
        return Verdict::Violation("граф задач пуст".into());
    }
    with_solver(|ctx, solver| {
        let n = graph.nodes.len() as i64;
        let pos: Vec<Int> = (0..graph.nodes.len())
            .map(|i| Int::new_const(ctx, format!("pos_{i}")))
            .collect();
        let zero = Int::from_i64(ctx, 0);
        let limit = Int::from_i64(ctx, n);
        for (i, p) in pos.iter().enumerate() {
            solver.assert(&p.ge(&zero));
            solver.assert(&p.lt(&limit));
            for q in pos.iter().take(i) {
                solver.assert(&p._eq(q).not());
            }
        }
        let index = index_of(graph);
        let mut missing_n = 0usize;
        for (i, node) in graph.nodes.iter().enumerate() {
            for d in &node.deps {
                match index.get(d.as_str()) {
                    Some(&j) => {
                        // Позиция узла строго больше позиции зависимости.
                        solver.assert(&pos[i].gt(&pos[j]));
                    }
                    None => {
                        missing_n += 1;
                        let tag = Bool::new_const(ctx, format!("missing_{i}_{missing_n}"));
                        solver.assert_and_track(&Bool::from_bool(ctx, false), &tag);
                    }
                }
            }
        }
        verdict(solver, "граф зависимостей противоречив (цикл)")
    })
}

/// Можно ли пометить узел выполненным.
/// `observed_now` приходит из детерминированной сверки экрана, не из отчёта.
pub fn allow_node_done(graph: &TaskGraph, id: &str, observed_now: bool) -> Verdict {
    let Some(idx) = graph.nodes.iter().position(|n| n.id == id) else {
        return Verdict::Violation(format!("нет узла {id}"));
    };
    match graph.nodes[idx].state {
        NodeState::Pending | NodeState::Running => {}
        _ => {
            return Verdict::Violation(format!("узел {id} уже не в работе, закрывать его нельзя"));
        }
    }
    with_solver(|ctx, solver| {
        let _done = encode_facts(ctx, solver, graph, |i, node| {
            let closing = i == idx;
            let done = node.state == NodeState::Done || closing;
            let obs = if closing {
                observed_now
            } else {
                node.state == NodeState::Done
            };
            (done, obs)
        });
        verdict(
            solver,
            "узел нельзя закрыть: критерий не виден на экране или зависимость не выполнена",
        )
    })
}

/// Задача закрыта только доказательством. Отчёт модели сюда не передаётся.
///
/// Успех: нет ожидающих и проваленных узлов, каждый выполненный узел
/// подтверждён наблюдением, и хотя бы один узел выполнен. Откат и
/// пропуск — история, они успех не изображают и сами по себе задачу
/// не закрывают.
pub fn check_solution(graph: &TaskGraph, observed: &HashMap<String, bool>) -> Verdict {
    if graph.nodes.is_empty() {
        return Verdict::Violation("нечего закрывать: граф пуст".into());
    }
    with_solver(|ctx, solver| {
        let done = encode_facts(ctx, solver, graph, |_i, node| {
            let is_done = node.state == NodeState::Done;
            let is_obs = observed.get(&node.id).copied().unwrap_or(false);
            (is_done, is_obs)
        });
        for (i, node) in graph.nodes.iter().enumerate() {
            let blocking = matches!(
                node.state,
                NodeState::Pending | NodeState::Running | NodeState::Failed
            );
            let tag = Bool::new_const(ctx, format!("open_{i}"));
            solver.assert_and_track(&Bool::from_bool(ctx, !blocking), &tag);
        }
        let refs: Vec<&Bool> = done.iter().collect();
        let tag = Bool::new_const(ctx, "some_done");
        solver.assert_and_track(&Bool::or(ctx, &refs), &tag);
        verdict(
            solver,
            "задача не закрыта: результат не подтверждён по всем узлам",
        )
    })
}

/// Фильтр одного действия до исполнения. Это политика, не отчёт модели.
pub fn gate_action(action: &str, pol: &Policies) -> Verdict {
    let low = action.to_lowercase();
    if let Some(f) = pol
        .forbidden_actions
        .iter()
        .find(|f| low.contains(&f.to_lowercase()))
    {
        return Verdict::Violation(format!("запрещено политикой: {f}"));
    }
    Verdict::Ok
}

fn index_of(graph: &TaskGraph) -> HashMap<&str, usize> {
    graph
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id.as_str(), i))
        .collect()
}

/// Записывает факты «узел выполнен» и «критерий наблюдён» и следствия:
/// выполненность требует наблюдения и выполненных зависимостей.
fn encode_facts<'ctx, F>(
    ctx: &'ctx Context,
    solver: &'ctx Solver,
    graph: &TaskGraph,
    mut claim: F,
) -> Vec<Bool<'ctx>>
where
    F: FnMut(usize, &crate::agent::graph::Node) -> (bool, bool),
{
    let done: Vec<Bool> = (0..graph.nodes.len())
        .map(|i| Bool::new_const(ctx, format!("done_{i}")))
        .collect();
    let obs: Vec<Bool> = (0..graph.nodes.len())
        .map(|i| Bool::new_const(ctx, format!("obs_{i}")))
        .collect();
    let index = index_of(graph);
    let mut missing_n = 0usize;
    for (i, node) in graph.nodes.iter().enumerate() {
        let (is_done, is_obs) = claim(i, node);
        let fact_done = Bool::new_const(ctx, format!("fact_done_{i}"));
        solver.assert_and_track(&done[i]._eq(&Bool::from_bool(ctx, is_done)), &fact_done);
        let fact_obs = Bool::new_const(ctx, format!("fact_obs_{i}"));
        solver.assert_and_track(&obs[i]._eq(&Bool::from_bool(ctx, is_obs)), &fact_obs);

        let need_obs = Bool::new_const(ctx, format!("need_obs_{i}"));
        solver.assert_and_track(&done[i].implies(&obs[i]), &need_obs);
        for d in &node.deps {
            match index.get(d.as_str()) {
                Some(&j) => {
                    let need_dep = Bool::new_const(ctx, format!("need_dep_{i}_{j}"));
                    solver.assert_and_track(&done[i].implies(&done[j]), &need_dep);
                }
                None => {
                    missing_n += 1;
                    let missing = Bool::new_const(ctx, format!("missing_dep_{i}_{missing_n}"));
                    solver.assert_and_track(&Bool::from_bool(ctx, false), &missing);
                }
            }
        }
    }
    done
}

fn with_solver<T>(f: impl FnOnce(&Context, &Solver) -> T) -> T {
    // У Z3 свой глобальный аллокатор. Параллельные контексты из `cargo test`
    // на части сборок падают, поэтому доказательства идут по одному.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cfg = Config::new();
    let ctx = Context::new(&cfg);
    let solver = Solver::new(&ctx);
    f(&ctx, &solver)
}

fn verdict(solver: &Solver, fallback: &str) -> Verdict {
    match solver.check() {
        SatResult::Sat => Verdict::Ok,
        SatResult::Unknown => {
            Verdict::Violation(format!("Z3 не смог доказать условие: {fallback}"))
        }
        SatResult::Unsat => {
            let core = solver.get_unsat_core();
            if core.is_empty() {
                Verdict::Violation(fallback.into())
            } else {
                let names = core
                    .iter()
                    .map(|b| b.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                Verdict::Violation(format!(
                    "{fallback}: {}",
                    crate::text::truncate(&names, 180)
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::graph::MAX_NODE_ATTEMPTS;

    fn linear(n: usize) -> TaskGraph {
        let steps: Vec<_> = (0..n)
            .map(|i| (format!("узел {i}"), format!("видно {i}")))
            .collect();
        let mut g = TaskGraph::linear("цель", &steps);
        g.validate().unwrap();
        g
    }

    fn cycle() -> TaskGraph {
        serde_json::from_str(
            r#"{"goal":"g","nodes":[
              {"id":"a","goal":"a","done_when":"a","deps":["b"]},
              {"id":"b","goal":"b","done_when":"b","deps":["a"]}]}"#,
        )
        .unwrap()
    }

    #[test]
    fn consistent_plan_passes() {
        assert_eq!(check_plan(&linear(3), &Policies::new()), Verdict::Ok);
    }

    #[test]
    fn cycle_is_contradiction_not_ok() {
        let g = cycle();
        assert!(matches!(
            check_plan(&g, &Policies::new()),
            Verdict::Violation(_)
        ));
        assert!(matches!(prove_schedule(&g), Verdict::Violation(_)));
    }

    #[test]
    fn forbidden_action_is_rejected() {
        let mut pol = Policies::new();
        pol.forbidden_actions.push("удалить всё".into());
        assert!(matches!(
            gate_action("Удалить всё на диске", &pol),
            Verdict::Violation(_)
        ));
        let mut g = linear(1);
        g.nodes[0].goal = "удалить всё на диске".into();
        assert!(matches!(check_plan(&g, &pol), Verdict::Violation(_)));
    }

    #[test]
    fn oversized_plan_is_rejected() {
        let g = linear(10);
        let mut pol = Policies::new();
        pol.max_plan_steps = 5;
        assert!(matches!(check_plan(&g, &pol), Verdict::Violation(_)));
    }

    #[test]
    fn node_closes_only_after_observation_and_dependencies() {
        let mut g = linear(2);
        assert!(matches!(
            allow_node_done(&g, "n1", false),
            Verdict::Violation(_)
        ));
        assert!(matches!(
            allow_node_done(&g, "n2", true),
            Verdict::Violation(_)
        ));
        assert_eq!(allow_node_done(&g, "n1", true), Verdict::Ok);
        g.mark_done("n1", "на экране видно 0");
        assert_eq!(allow_node_done(&g, "n2", true), Verdict::Ok);
    }

    #[test]
    fn success_report_does_not_close_an_open_graph() {
        let mut g = linear(2);
        g.mark_done("n1", "модель написала, что всё готово");
        let observed = HashMap::from([("n1".to_string(), true)]);
        assert!(matches!(
            check_solution(&g, &observed),
            Verdict::Violation(_)
        ));
    }

    #[test]
    fn done_mark_without_observation_is_not_a_solution() {
        let mut g = linear(1);
        g.mark_done("n1", "готово");
        assert!(matches!(
            check_solution(&g, &HashMap::new()),
            Verdict::Violation(_)
        ));
    }

    #[test]
    fn fully_observed_graph_is_a_solution() {
        let mut g = linear(2);
        g.mark_done("n1", "видно 0");
        g.mark_done("n2", "видно 1");
        let observed = HashMap::from([("n1".to_string(), true), ("n2".to_string(), true)]);
        assert_eq!(check_solution(&g, &observed), Verdict::Ok);
    }

    #[test]
    fn failed_node_blocks_closure() {
        let mut g = linear(2);
        g.mark_done("n1", "видно 0");
        for _ in 0..MAX_NODE_ATTEMPTS {
            g.mark_attempt_failed("n2", "не вышло");
        }
        let observed = HashMap::from([("n1".to_string(), true)]);
        assert!(matches!(
            check_solution(&g, &observed),
            Verdict::Violation(_)
        ));
    }

    #[test]
    fn rolled_back_graph_without_a_done_node_is_not_success() {
        let mut g = linear(1);
        g.mark_done("n1", "видно 0");
        g.mark_rolled_back("n1");
        let observed = HashMap::from([("n1".to_string(), true)]);
        assert!(matches!(
            check_solution(&g, &observed),
            Verdict::Violation(_)
        ));
    }
}
