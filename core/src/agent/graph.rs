//! TaskGraph — DAG подзадач вместо линейного списка.
//!
//! Зачем DAG, а не список:
//!   - «поставь мне Fable 5» это [проверить диск] → [скачать] → [установить] →
//!     [запустить], где «скачать» бессмысленно без места на диске. Линейный
//!     список это выражает случайно (порядком), граф — явно (зависимостями);
//!   - независимые ветки видно сразу: если [скачать инсталлятор] и
//!     [освободить диск] не связаны, провал одной не блокирует другую;
//!   - откат: у каждого узла есть компенсирующее действие, и при провале мы
//!     откатываем ровно выполненные узлы в обратном порядке, а не «всё».
//!
//! Альтернативы, которые отброшены:
//!   1. хранить план в промпте и надеяться, что модель помнит порядок —
//!      именно так агенты зацикливаются и теряют цель;
//!   2. полноценный движок workflow (temporal-подобный) — избыточен для
//!      локального агента: нет распределённости, нет многодневных задач.
//!
//! Инварианты (проверяются в `validate`, ошибки — не паники):
//!   - id уникальны и непустые;
//!   - все зависимости существуют;
//!   - циклов нет;
//!   - размер графа ограничен (модель любит сгенерировать 200 «шагов»).

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Максимум узлов. Больше — почти всегда признак того, что модель
/// расписала задачу до уровня «нажми пробел», и такой план не выполним.
pub const MAX_NODES: usize = 40;
const MAX_DEPS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeState {
    Pending,
    Running,
    Done,
    Failed,
    /// Узел откачен компенсирующим действием.
    RolledBack,
    /// Пропущен: его предпосылка провалилась, выполнять бессмысленно.
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub goal: String,
    /// Наблюдаемый критерий готовности. Без него узел нельзя ни проверить,
    /// ни закрыть — агент будет объявлять успех по своему ощущению.
    #[serde(default)]
    pub done_when: String,
    #[serde(default)]
    pub deps: Vec<String>,
    /// Как откатить этот узел, если дальше всё сломается. Пусто = откат не нужен
    /// (например, «проверить свободное место» ничего не меняет).
    #[serde(default)]
    pub rollback: String,
    #[serde(default = "pending")]
    pub state: NodeState,
    #[serde(default)]
    pub note: String,
    /// Сколько раз узел уже провалился: после лимита он не берётся снова,
    /// иначе агент вечно бьётся в одну и ту же дверь.
    #[serde(default)]
    pub attempts: u32,
}

fn pending() -> NodeState {
    NodeState::Pending
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TaskGraph {
    pub goal: String,
    pub nodes: Vec<Node>,
    #[serde(default)]
    pub risks: Vec<String>,
    /// Порядок фактического выполнения — нужен для корректного откуата
    /// (обратный порядок завершения, а не обратный порядок в списке).
    #[serde(default)]
    pub completed_order: Vec<String>,
}

/// Максимум попыток на узел до признания его провалившимся.
pub const MAX_NODE_ATTEMPTS: u32 = 3;

impl TaskGraph {
    /// Линейная цепочка — фолбэк, когда модель не смогла выдать зависимости.
    /// Плохой граф лучше отсутствия графа: порядок хотя бы сохранён.
    pub fn linear(goal: &str, steps: &[(String, String)]) -> Self {
        let mut nodes = Vec::with_capacity(steps.len());
        for (i, (g, dw)) in steps.iter().enumerate() {
            nodes.push(Node {
                id: format!("n{}", i + 1),
                goal: g.clone(),
                done_when: dw.clone(),
                deps: if i == 0 {
                    vec![]
                } else {
                    vec![format!("n{i}")]
                },
                rollback: String::new(),
                state: NodeState::Pending,
                note: String::new(),
                attempts: 0,
            });
        }
        Self {
            goal: goal.to_string(),
            nodes,
            risks: vec![],
            completed_order: vec![],
        }
    }

    /// Валидация входных данных от модели. Возвращает ошибку, а не паникует:
    /// невалидный план — штатная ситуация, на неё есть перепланирование.
    pub fn validate(&mut self) -> Result<(), String> {
        if self.nodes.is_empty() {
            return Err("граф задач пуст".into());
        }
        if self.nodes.len() > MAX_NODES {
            return Err(format!(
                "слишком много узлов: {} (максимум {MAX_NODES})",
                self.nodes.len()
            ));
        }
        let mut seen: HashSet<String> = HashSet::new();
        for n in &mut self.nodes {
            n.id = n.id.trim().to_string();
            if n.id.is_empty() {
                return Err("у узла пустой id".into());
            }
            if n.id.chars().count() > 64 {
                return Err(format!("слишком длинный id узла: {}", n.id));
            }
            if !seen.insert(n.id.clone()) {
                return Err(format!("дублирующийся id узла: {}", n.id));
            }
            if n.goal.trim().is_empty() {
                return Err(format!("узел {} без цели", n.id));
            }
            if n.done_when.trim().is_empty() {
                // Критерий готовности обязателен, но выдумывать его за модель
                // дешевле, чем ронять весь план.
                n.done_when = format!("видно, что «{}» выполнено", n.goal);
            }
            if n.deps.len() > MAX_DEPS {
                return Err(format!("у узла {} слишком много зависимостей", n.id));
            }
            n.deps.retain(|d| !d.trim().is_empty());
            n.deps.dedup();
        }
        let ids: HashSet<&str> = self.nodes.iter().map(|n| n.id.as_str()).collect();
        for n in &self.nodes {
            for d in &n.deps {
                if d == &n.id {
                    return Err(format!("узел {} зависит от себя", n.id));
                }
                if !ids.contains(d.as_str()) {
                    return Err(format!("узел {} зависит от несуществующего {d}", n.id));
                }
            }
        }
        self.topo_order()
            .map(|_| ())
            .ok_or_else(|| "в графе задач есть цикл зависимостей".to_string())
    }

    /// Топологический порядок (Кан). None = цикл.
    pub fn topo_order(&self) -> Option<Vec<String>> {
        let mut indeg: HashMap<&str, usize> = self
            .nodes
            .iter()
            .map(|n| (n.id.as_str(), n.deps.len()))
            .collect();
        let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
        for n in &self.nodes {
            for d in &n.deps {
                children.entry(d.as_str()).or_default().push(n.id.as_str());
            }
        }
        // Стабильный порядок: берём узлы в порядке объявления, чтобы план
        // выполнялся предсказуемо и его можно было сравнивать в тестах.
        let mut queue: Vec<&str> = self
            .nodes
            .iter()
            .filter(|n| indeg.get(n.id.as_str()).copied().unwrap_or(0) == 0)
            .map(|n| n.id.as_str())
            .collect();
        let mut out: Vec<String> = Vec::with_capacity(self.nodes.len());
        let mut i = 0;
        while i < queue.len() {
            let id = queue[i];
            i += 1;
            out.push(id.to_string());
            let kids = children.get(id).cloned().unwrap_or_default();
            for k in kids {
                if let Some(d) = indeg.get_mut(k) {
                    *d = d.saturating_sub(1);
                    if *d == 0 {
                        queue.push(k);
                    }
                }
            }
        }
        if out.len() == self.nodes.len() {
            Some(out)
        } else {
            None
        }
    }

    fn state_of(&self, id: &str) -> Option<NodeState> {
        self.nodes.iter().find(|n| n.id == id).map(|n| n.state)
    }

    /// Узлы, готовые к выполнению: сами ожидают и все зависимости закрыты.
    pub fn ready(&self) -> Vec<&Node> {
        self.nodes
            .iter()
            .filter(|n| n.state == NodeState::Pending && n.attempts < MAX_NODE_ATTEMPTS)
            .filter(|n| {
                n.deps
                    .iter()
                    .all(|d| self.state_of(d) == Some(NodeState::Done))
            })
            .collect()
    }

    /// Следующий узел. Порядок — топологический, поэтому агент не прыгает
    /// между ветками без причины.
    pub fn next(&self) -> Option<&Node> {
        let order = self.topo_order().unwrap_or_default();
        let ready: HashSet<&str> = self.ready().iter().map(|n| n.id.as_str()).collect();
        order
            .iter()
            .find(|id| ready.contains(id.as_str()))
            .and_then(|id| self.nodes.iter().find(|n| &n.id == id))
    }

    pub fn mark_done(&mut self, id: &str, note: &str) {
        if let Some(n) = self.nodes.iter_mut().find(|n| n.id == id) {
            n.state = NodeState::Done;
            n.note = note.to_string();
        }
        if !self.completed_order.iter().any(|x| x == id) {
            self.completed_order.push(id.to_string());
        }
    }

    /// Провал попытки. Возвращает true, если узел исчерпал попытки и
    /// окончательно провалился (тогда пора откатывать).
    pub fn mark_attempt_failed(&mut self, id: &str, err: &str) -> bool {
        let mut terminal = false;
        if let Some(n) = self.nodes.iter_mut().find(|n| n.id == id) {
            n.attempts = n.attempts.saturating_add(1);
            n.note = err.chars().take(300).collect();
            if n.attempts >= MAX_NODE_ATTEMPTS {
                n.state = NodeState::Failed;
                terminal = true;
            }
        }
        if terminal {
            self.skip_descendants(id);
        }
        terminal
    }

    /// Всё, что зависело от провалившегося узла, выполнять нельзя.
    /// Иначе агент «устанавливает» то, что не скачал.
    fn skip_descendants(&mut self, failed: &str) {
        let mut blocked: HashSet<String> = HashSet::new();
        blocked.insert(failed.to_string());
        // Топологический порядок гарантирует, что предок обработан раньше потомка.
        let order = self.topo_order().unwrap_or_default();
        for id in order {
            let deps_blocked = self
                .nodes
                .iter()
                .find(|n| n.id == id)
                .map(|n| n.deps.iter().any(|d| blocked.contains(d)))
                .unwrap_or(false);
            if deps_blocked {
                blocked.insert(id.clone());
                if let Some(n) = self.nodes.iter_mut().find(|n| n.id == id) {
                    if n.state == NodeState::Pending {
                        n.state = NodeState::Skipped;
                        n.note = format!("пропущено: провалилась предпосылка {failed}");
                    }
                }
            }
        }
    }

    /// План отката: выполненные узлы с непустым rollback в обратном порядке
    /// завершения. Возвращаем инструкции, а не действия: конкретный клик
    /// зависит от того, что сейчас на экране, и это решает уже цикл ReAct.
    pub fn rollback_plan(&self) -> Vec<(String, String)> {
        self.completed_order
            .iter()
            .rev()
            .filter_map(|id| self.nodes.iter().find(|n| &n.id == id))
            .filter(|n| n.state == NodeState::Done && !n.rollback.trim().is_empty())
            .map(|n| (n.id.clone(), n.rollback.clone()))
            .collect()
    }

    pub fn mark_rolled_back(&mut self, id: &str) {
        if let Some(n) = self.nodes.iter_mut().find(|n| n.id == id) {
            n.state = NodeState::RolledBack;
        }
        self.completed_order.retain(|x| x != id);
    }

    pub fn failed(&self) -> Vec<&Node> {
        self.nodes
            .iter()
            .filter(|n| n.state == NodeState::Failed)
            .collect()
    }

    pub fn progress(&self) -> (usize, usize) {
        (
            self.nodes
                .iter()
                .filter(|n| n.state == NodeState::Done)
                .count(),
            self.nodes.len(),
        )
    }

    pub fn to_prompt(&self) -> String {
        let mut s = format!("ЦЕЛЬ: {}\nГРАФ ЗАДАЧ (DAG):\n", self.goal);
        for n in &self.nodes {
            let mark = match n.state {
                NodeState::Done => "x",
                NodeState::Failed => "!",
                NodeState::Skipped => "-",
                NodeState::RolledBack => "<",
                NodeState::Running => ">",
                NodeState::Pending => " ",
            };
            s.push_str(&format!(
                "{} [{}] {} (готово когда: {}{})\n",
                n.id,
                mark,
                n.goal,
                n.done_when,
                if n.deps.is_empty() {
                    String::new()
                } else {
                    format!("; после: {}", n.deps.join(","))
                }
            ));
        }
        if !self.risks.is_empty() {
            s.push_str(&format!("РИСКИ: {}\n", self.risks.join("; ")));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g() -> TaskGraph {
        let mut g: TaskGraph = serde_json::from_str(
            r#"{"goal":"поставить игру","nodes":[
              {"id":"disk","goal":"проверить диск","done_when":"видно свободно >50 ГБ"},
              {"id":"dl","goal":"скачать","done_when":"файл скачан","deps":["disk"],"rollback":"удалить скачанный файл"},
              {"id":"inst","goal":"установить","done_when":"есть ярлык","deps":["dl"],"rollback":"удалить игру"},
              {"id":"run","goal":"запустить","done_when":"окно игры открыто","deps":["inst"]}]}"#,
        )
        .expect("фикстура графа");
        g.validate().expect("граф валиден");
        g
    }

    #[test]
    fn executes_in_dependency_order() {
        let mut gr = g();
        assert_eq!(gr.next().map(|n| n.id.clone()), Some("disk".into()));
        gr.mark_done("disk", "свободно 300 ГБ");
        assert_eq!(gr.next().map(|n| n.id.clone()), Some("dl".into()));
        gr.mark_done("dl", "скачано");
        assert_eq!(gr.next().map(|n| n.id.clone()), Some("inst".into()));
    }

    #[test]
    fn cycle_is_rejected_not_panicking() {
        let mut bad: TaskGraph = serde_json::from_str(
            r#"{"goal":"g","nodes":[
              {"id":"a","goal":"a","done_when":"a","deps":["b"]},
              {"id":"b","goal":"b","done_when":"b","deps":["a"]}]}"#,
        )
        .expect("парсинг");
        assert!(bad.validate().is_err());
        assert!(bad.topo_order().is_none());
    }

    #[test]
    fn unknown_dep_and_dup_id_rejected() {
        let mut a: TaskGraph =
            serde_json::from_str(r#"{"goal":"g","nodes":[{"id":"a","goal":"a","deps":["zzz"]}]}"#)
                .expect("парсинг");
        assert!(a.validate().is_err());
        let mut b: TaskGraph = serde_json::from_str(
            r#"{"goal":"g","nodes":[{"id":"a","goal":"a"},{"id":"a","goal":"b"}]}"#,
        )
        .expect("парсинг");
        assert!(b.validate().is_err());
    }

    #[test]
    fn failure_skips_descendants_and_rolls_back_in_reverse() {
        let mut gr = g();
        gr.mark_done("disk", "ок");
        gr.mark_done("dl", "скачано");
        for _ in 0..MAX_NODE_ATTEMPTS - 1 {
            assert!(!gr.mark_attempt_failed("inst", "инсталлятор ругается"));
        }
        assert!(gr.mark_attempt_failed("inst", "инсталлятор ругается"));
        // «запустить» больше не берём: устанавливать нечего.
        assert!(gr.next().is_none());
        assert_eq!(
            gr.nodes
                .iter()
                .find(|n| n.id == "run")
                .map(|n| n.state)
                .expect("узел run"),
            NodeState::Skipped
        );
        // Откат: сначала отменяем скачивание (последнее выполненное с rollback).
        let plan = gr.rollback_plan();
        assert_eq!(
            plan,
            vec![("dl".to_string(), "удалить скачанный файл".to_string())]
        );
    }

    #[test]
    fn too_many_nodes_rejected() {
        let nodes: Vec<String> = (0..MAX_NODES + 1)
            .map(|i| format!("{{\"id\":\"n{i}\",\"goal\":\"g{i}\"}}"))
            .collect();
        let mut big: TaskGraph = serde_json::from_str(&format!(
            "{{\"goal\":\"g\",\"nodes\":[{}]}}",
            nodes.join(",")
        ))
        .expect("парсинг");
        assert!(big.validate().is_err());
    }

    #[test]
    fn linear_fallback_is_valid() {
        let mut lin = TaskGraph::linear(
            "цель",
            &[
                ("шаг 1".into(), "видно 1".into()),
                ("шаг 2".into(), "видно 2".into()),
            ],
        );
        assert!(lin.validate().is_ok());
        assert_eq!(lin.next().map(|n| n.id.clone()), Some("n1".into()));
    }
}
