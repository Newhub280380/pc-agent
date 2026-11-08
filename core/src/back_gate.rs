//! Задние ворота (back gate): единственное место, где задача получает
//! флаг закрытия `jev_close`.
//!
//! Зачем отдельный модуль. Агент погружается на максимальную глубину
//! проложенного Jev-пути (последний узел графа, последний фрагмент корпуса)
//! и в этой точке должен «вернуться» к пользователю. Раньше возврат
//! складывался из нескольких проверок, разбросанных по `jev_gate` и
//! `agent`, а итоговый признак «задача закрыта» был обычным `bool`, который
//! любой код мог выставить сам. Теперь признак — это значение типа [`Seal`]
//! с приватным полем: сконструировать его можно только внутри этого модуля,
//! то есть только пройдя полную проверку. LLM и агент печать не выставляют —
//! они лишь просят шлюз её поставить.
//!
//! Что проверяется детерминированно (без участия модели):
//!   1. каждый узел проложенного пути пройден (`Done`) — ни пропущенных,
//!      ни откаченных, ни проваленных;
//!   2. у каждого узла есть доказательство (`note`) и подтверждение
//!      шлюза (`observed == true`), а не только пометка;
//!   3. порядок прохождения (`completed_order`) содержит каждый узел ровно
//!      один раз, и все зависимости стоят строго раньше зависимых узлов;
//!   4. то же самое доказывает Z3: [`crate::logic::check_solution`]
//!      (решение подтверждено по всем узлам) и
//!      [`crate::logic::prove_walk_order`] (фактический порядок обхода
//!      совместим с зависимостями). `Unknown` солвера — отказ;
//!   5. независимый приговор эндоскопа закрывает задачу.
//!
//! Любой провал — [`Refusal`] с точным перечнем непройденных и
//! неподтверждённых шагов: это и есть «строгий возврат» на уровне
//! финализации — пользователю возвращается не «готово», а список того,
//! чего не хватает.

use crate::agent::graph::{NodeState, TaskGraph};
use crate::{corpus, endoscope, ledger, logic};
use serde_json::{json, Map, Value};
use std::collections::HashMap;

/// Печать закрытия. Поле `_private` запрещает конструировать значение вне
/// модуля: единственный способ получить `Seal` — пройти [`seal_route`] или
/// [`seal_study`]. Именно наличие этого значения и есть флаг `jev_close`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seal {
    /// Что закрыто: идентификатор маршрута или исследования.
    subject: String,
    /// Фактический порядок прохождения шагов (или список единиц корпуса).
    path: Vec<String>,
    /// Какие проверки легли в основу печати, по порядку.
    proof: Vec<&'static str>,
    /// Момент закрытия (UTC, RFC 3339).
    closed_at: String,
    _private: (),
}

impl Seal {
    fn new(subject: &str, path: Vec<String>, proof: Vec<&'static str>) -> Self {
        Self {
            subject: subject.to_string(),
            path,
            proof,
            closed_at: chrono::Utc::now().to_rfc3339(),
            _private: (),
        }
    }

    pub fn subject(&self) -> &str {
        &self.subject
    }

    pub fn path(&self) -> &[String] {
        &self.path
    }

    /// Представление для MCP-ответа. Флаг `jev_close: true` появляется
    /// только здесь — никакой другой код его не пишет.
    pub fn json(&self) -> Value {
        json!({
            "jev_close": true,
            "subject": self.subject,
            "steps": self.path.len(),
            "path": self.path,
            "proof": self.proof,
            "closed_at": self.closed_at,
        })
    }
}

/// Отказ в закрытии: задача остаётся открытой, и пользователь получает
/// точный перечень того, что не пройдено или не подтверждено. Функции
/// `seal_*` возвращают его в `Box`: отказ — большое значение, а печать —
/// маленькое, и успешный путь не должен таскать лишние байты.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Refusal {
    pub subject: String,
    /// Узлы пути, которые не пройдены (не в состоянии `Done`), и почему.
    pub unvisited: Vec<(String, String)>,
    /// Пройденные узлы без доказательства или без подтверждения шлюза.
    pub unconfirmed: Vec<(String, String)>,
    /// Нарушения порядка: узел закрыт раньше своей зависимости, дважды
    /// или вовсе не попал в журнал прохождения.
    pub out_of_order: Vec<String>,
    /// Отказы доказательной части: Z3 и эндоскоп.
    pub proof_failures: Vec<String>,
    /// Дополнительные поля предметной проверки (например, отклонённые
    /// утверждения отчёта у исследования корпуса).
    pub extra: Map<String, Value>,
}

impl Refusal {
    fn new(subject: &str) -> Self {
        Self {
            subject: subject.to_string(),
            ..Self::default()
        }
    }

    fn is_empty(&self) -> bool {
        self.unvisited.is_empty()
            && self.unconfirmed.is_empty()
            && self.out_of_order.is_empty()
            && self.proof_failures.is_empty()
            && self.extra.is_empty()
    }

    /// Одной строкой — для логов и текстового отчёта пользователю.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.unvisited.is_empty() {
            parts.push(format!(
                "не пройдены шаги: {}",
                self.unvisited
                    .iter()
                    .map(|(id, why)| format!("{id} ({why})"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !self.unconfirmed.is_empty() {
            parts.push(format!(
                "не подтверждены шаги: {}",
                self.unconfirmed
                    .iter()
                    .map(|(id, why)| format!("{id} ({why})"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !self.out_of_order.is_empty() {
            parts.push(format!("нарушен порядок: {}", self.out_of_order.join(", ")));
        }
        if !self.proof_failures.is_empty() {
            parts.push(self.proof_failures.join("; "));
        }
        for (key, value) in &self.extra {
            parts.push(format!("{key}: {value}"));
        }
        if parts.is_empty() {
            "закрытие отклонено".to_string()
        } else {
            parts.join("; ")
        }
    }

    /// Представление для MCP-ответа. `jev_close` здесь всегда `false`.
    pub fn json(&self) -> Value {
        let mut map = Map::new();
        map.insert("status".into(), "open".into());
        map.insert("jev_close".into(), Value::Bool(false));
        map.insert("subject".into(), self.subject.clone().into());
        if !self.unvisited.is_empty() {
            map.insert(
                "unvisited".into(),
                self.unvisited
                    .iter()
                    .map(|(id, why)| json!({"step_id": id, "why": why}))
                    .collect(),
            );
        }
        if !self.unconfirmed.is_empty() {
            map.insert(
                "unconfirmed".into(),
                self.unconfirmed
                    .iter()
                    .map(|(id, why)| json!({"step_id": id, "why": why}))
                    .collect(),
            );
        }
        if !self.out_of_order.is_empty() {
            map.insert(
                "out_of_order".into(),
                self.out_of_order.iter().cloned().map(Value::from).collect(),
            );
        }
        if !self.proof_failures.is_empty() {
            map.insert(
                "proof_failures".into(),
                self.proof_failures
                    .iter()
                    .cloned()
                    .map(Value::from)
                    .collect(),
            );
        }
        for (key, value) in &self.extra {
            map.entry(key.clone()).or_insert_with(|| value.clone());
        }
        map.insert("reason".into(), self.summary().into());
        Value::Object(map)
    }
}

/// Как относиться к ненастроенному эндоскопу.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndoscopePolicy {
    /// MCP-шлюз: без независимой проверки закрытия нет.
    Required,
    /// Встроенный агент: без эндоскопа закрывают проверки ядра
    /// (поведение GUI/headless-режима не меняется).
    Optional,
}

fn endoscope_failure(verdict: &endoscope::Verdict, policy: EndoscopePolicy) -> Option<String> {
    match verdict {
        endoscope::Verdict::NotConfigured if policy == EndoscopePolicy::Required => {
            Some("эндоскоп не настроен: задача остаётся открытой".to_string())
        }
        verdict if !verdict.closes() => Some(verdict.why()),
        _ => None,
    }
}

/// Состояние узла словами для перечня непройденных шагов.
fn state_word(state: NodeState) -> &'static str {
    match state {
        NodeState::Pending => "не начат",
        NodeState::Running => "в работе",
        NodeState::Done => "выполнен",
        NodeState::Failed => "провален",
        NodeState::RolledBack => "откачен",
        NodeState::Skipped => "пропущен",
    }
}

/// Структурная часть проверки маршрута: без солвера, но исчерпывающе.
/// Возвращает отказ со всеми найденными нарушениями сразу, чтобы агент
/// не узнавал о них по одному.
fn inspect_route(subject: &str, graph: &TaskGraph, observed: &HashMap<String, bool>) -> Refusal {
    let mut refusal = Refusal::new(subject);
    if graph.nodes.is_empty() {
        refusal
            .proof_failures
            .push("нечего закрывать: граф пуст".to_string());
        return refusal;
    }
    for node in &graph.nodes {
        if node.state != NodeState::Done {
            refusal
                .unvisited
                .push((node.id.clone(), state_word(node.state).to_string()));
            continue;
        }
        if node.note.trim().is_empty() {
            refusal
                .unconfirmed
                .push((node.id.clone(), "нет доказательства шага".to_string()));
        }
        if !observed.get(&node.id).copied().unwrap_or(false) {
            refusal.unconfirmed.push((
                node.id.clone(),
                "критерий шага не подтверждён шлюзом".to_string(),
            ));
        }
    }
    // Журнал прохождения: каждый узел ровно один раз, лишних нет,
    // зависимости строго раньше.
    let position: HashMap<&str, usize> = graph
        .completed_order
        .iter()
        .enumerate()
        .map(|(index, id)| (id.as_str(), index))
        .collect();
    for (index, id) in graph.completed_order.iter().enumerate() {
        if graph.completed_order[..index].contains(id) {
            refusal
                .out_of_order
                .push(format!("{id}: встречается в журнале прохождения дважды"));
        }
        if !graph.nodes.iter().any(|node| node.id == *id) {
            refusal
                .out_of_order
                .push(format!("{id}: в журнале есть шаг, которого нет в пути"));
        }
    }
    for node in &graph.nodes {
        if node.state != NodeState::Done {
            continue;
        }
        let Some(&own) = position.get(node.id.as_str()) else {
            refusal.out_of_order.push(format!(
                "{}: выполнен, но не записан в журнал прохождения",
                node.id
            ));
            continue;
        };
        for dep in &node.deps {
            match position.get(dep.as_str()) {
                Some(&before) if before < own => {}
                Some(_) => refusal.out_of_order.push(format!(
                    "{}: закрыт раньше своей зависимости {dep}",
                    node.id
                )),
                None => refusal.out_of_order.push(format!(
                    "{}: зависимость {dep} не записана в журнал прохождения",
                    node.id
                )),
            }
        }
    }
    refusal
}

/// Закрытие маршрута через задние ворота.
///
/// `observed` — подтверждения шлюза по узлам (не слова агента). `verdict` —
/// приговор эндоскопа по уже проверенному отчёту. Печать выдаётся только
/// если пройдены все пять проверок из описания модуля; иначе — отказ с
/// перечнем.
pub fn seal_route(
    subject: &str,
    graph: &TaskGraph,
    observed: &HashMap<String, bool>,
    verdict: &endoscope::Verdict,
    policy: EndoscopePolicy,
) -> Result<Seal, Box<Refusal>> {
    let mut refusal = inspect_route(subject, graph, observed);
    if !refusal.is_empty() {
        return Err(Box::new(refusal));
    }
    // Солвер подключается только к структурно целому пути: его задача —
    // независимо доказать то же самое и поймать то, что ручная проверка
    // могла упустить (например, висячую зависимость).
    if let logic::Verdict::Violation(why) = logic::check_solution(graph, observed) {
        refusal.proof_failures.push(format!("Z3 (решение): {why}"));
    }
    if let logic::Verdict::Violation(why) = logic::prove_walk_order(graph, &graph.completed_order) {
        refusal.proof_failures.push(format!("Z3 (порядок): {why}"));
    }
    if let Some(why) = endoscope_failure(verdict, policy) {
        refusal.proof_failures.push(why);
    }
    if !refusal.is_empty() {
        return Err(Box::new(refusal));
    }
    let mut proof = vec![
        "steps_done",
        "evidence",
        "observed",
        "order",
        "z3_solution",
        "z3_order",
    ];
    proof.push(match verdict {
        endoscope::Verdict::Proven => "endoscope",
        _ => "endoscope_not_configured",
    });
    Ok(Seal::new(subject, graph.completed_order.clone(), proof))
}

/// Закрытие исследования корпуса через задние ворота.
///
/// Прохождение здесь — обход всех фрагментов всех источников, который
/// шлюз выполнил сам ([`corpus::walk`]); отчёт агента сверяется с
/// корпусом ([`corpus::back_gate`]). Возвращает печать и проверенный
/// текст отчёта — именно он, а не исходный текст модели, уходит
/// пользователю.
pub fn seal_study(
    subject: &str,
    sources: &[ledger::Source],
    walk: &corpus::Walk,
    report: &str,
    verdict: &endoscope::Verdict,
    policy: EndoscopePolicy,
) -> Result<(Seal, String), Box<Refusal>> {
    let mut refusal = Refusal::new(subject);
    let verified = match corpus::back_gate(sources, walk, report) {
        Ok(verified) => verified,
        Err(Value::Object(reasons)) => {
            for (key, value) in reasons {
                match key.as_str() {
                    "status" => {}
                    "unvisited" => {
                        for chunk in value.as_array().into_iter().flatten() {
                            let chunk = chunk.as_str().unwrap_or_default().to_string();
                            refusal
                                .unvisited
                                .push((chunk, "фрагмент не изучен".to_string()));
                        }
                    }
                    "failed" => {
                        for item in value.as_array().into_iter().flatten() {
                            let chunk = item["chunk"].as_str().unwrap_or_default().to_string();
                            let why = item["why"].as_str().unwrap_or_default().to_string();
                            refusal.unvisited.push((chunk, why));
                        }
                    }
                    "incomplete" => {
                        for item in value.as_array().into_iter().flatten() {
                            let id = item["id"].as_str().unwrap_or_default().to_string();
                            let why = item["why"].as_str().unwrap_or_default().to_string();
                            refusal.unconfirmed.push((id, why));
                        }
                    }
                    _ => {
                        refusal.extra.insert(key, value);
                    }
                }
            }
            return Err(Box::new(refusal));
        }
        Err(other) => {
            refusal.extra.insert("back_gate".into(), other);
            return Err(Box::new(refusal));
        }
    };
    if let Some(why) = endoscope_failure(verdict, policy) {
        refusal.proof_failures.push(why);
        return Err(Box::new(refusal));
    }
    let path: Vec<String> = sources
        .iter()
        .map(|source| format!("{}:{}", source.id, source.segments.len()))
        .collect();
    let mut proof = vec!["sources_complete", "chunks_visited", "claims_verified"];
    proof.push(match verdict {
        endoscope::Verdict::Proven => "endoscope",
        _ => "endoscope_not_configured",
    });
    Ok((Seal::new(subject, path, proof), verified))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linear(n: usize) -> TaskGraph {
        let steps: Vec<_> = (0..n)
            .map(|i| (format!("узел {i}"), format!("видно {i}")))
            .collect();
        let mut graph = TaskGraph::linear("цель", &steps);
        graph.validate().unwrap();
        graph
    }

    fn observed_all(graph: &TaskGraph) -> HashMap<String, bool> {
        graph
            .nodes
            .iter()
            .map(|node| (node.id.clone(), true))
            .collect()
    }

    #[test]
    fn full_walk_in_order_gets_the_seal() {
        let mut graph = linear(3);
        for i in 0..3 {
            graph.mark_done(&format!("n{}", i + 1), &format!("видно {i}"));
        }
        let seal = seal_route(
            "route-1",
            &graph,
            &observed_all(&graph),
            &endoscope::Verdict::Proven,
            EndoscopePolicy::Required,
        )
        .unwrap();
        assert_eq!(seal.path(), ["n1", "n2", "n3"]);
        assert_eq!(seal.json()["jev_close"], true);
        assert!(seal.json()["proof"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == "z3_order"));
    }

    #[test]
    fn partial_walk_lists_exactly_the_unvisited_steps() {
        let mut graph = linear(4);
        graph.mark_done("n1", "видно 0");
        graph.mark_done("n2", "видно 1");
        let refusal = seal_route(
            "route-1",
            &graph,
            &observed_all(&graph),
            &endoscope::Verdict::Proven,
            EndoscopePolicy::Required,
        )
        .unwrap_err();
        let ids: Vec<&str> = refusal
            .unvisited
            .iter()
            .map(|(id, _)| id.as_str())
            .collect();
        assert_eq!(ids, ["n3", "n4"]);
        assert!(refusal.unconfirmed.is_empty());
        assert_eq!(refusal.json()["jev_close"], false);
        assert_eq!(refusal.json()["status"], "open");
    }

    #[test]
    fn done_mark_without_gate_confirmation_is_unconfirmed() {
        let mut graph = linear(2);
        graph.mark_done("n1", "видно 0");
        graph.mark_done("n2", "");
        let observed = HashMap::from([("n1".to_string(), true)]);
        let refusal = seal_route(
            "route-1",
            &graph,
            &observed,
            &endoscope::Verdict::Proven,
            EndoscopePolicy::Required,
        )
        .unwrap_err();
        assert!(refusal.unvisited.is_empty());
        assert_eq!(refusal.unconfirmed.len(), 2);
        assert!(refusal.unconfirmed.iter().all(|(id, _)| id == "n2"));
    }

    #[test]
    fn dependency_closed_after_dependent_is_out_of_order() {
        let mut graph = linear(2);
        // Журнал подделан: сначала n2, потом n1 — состояния при этом Done.
        graph.mark_done("n2", "видно 1");
        graph.mark_done("n1", "видно 0");
        let refusal = seal_route(
            "route-1",
            &graph,
            &observed_all(&graph),
            &endoscope::Verdict::Proven,
            EndoscopePolicy::Required,
        )
        .unwrap_err();
        assert!(refusal
            .out_of_order
            .iter()
            .any(|why| why.contains("n2") && why.contains("n1")));
    }

    #[test]
    fn rolled_back_and_skipped_nodes_block_the_seal() {
        let mut graph = linear(2);
        graph.mark_done("n1", "видно 0");
        graph.mark_done("n2", "видно 1");
        graph.mark_rolled_back("n2");
        let refusal = seal_route(
            "route-1",
            &graph,
            &observed_all(&graph),
            &endoscope::Verdict::Proven,
            EndoscopePolicy::Required,
        )
        .unwrap_err();
        assert_eq!(
            refusal.unvisited,
            vec![("n2".to_string(), "откачен".to_string())]
        );
    }

    #[test]
    fn endoscope_policy_decides_without_endoscope() {
        let mut graph = linear(1);
        graph.mark_done("n1", "видно 0");
        let observed = observed_all(&graph);
        let refused = seal_route(
            "route-1",
            &graph,
            &observed,
            &endoscope::Verdict::NotConfigured,
            EndoscopePolicy::Required,
        )
        .unwrap_err();
        assert!(refused.proof_failures[0].contains("эндоскоп не настроен"));
        let sealed = seal_route(
            "route-1",
            &graph,
            &observed,
            &endoscope::Verdict::NotConfigured,
            EndoscopePolicy::Optional,
        )
        .unwrap();
        assert!(sealed.json()["proof"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == "endoscope_not_configured"));
        let disproven = seal_route(
            "route-1",
            &graph,
            &observed,
            &endoscope::Verdict::Disproven("нет записи".into()),
            EndoscopePolicy::Optional,
        )
        .unwrap_err();
        assert!(disproven.proof_failures[0].contains("не доказано"));
    }

    #[test]
    fn empty_graph_cannot_be_sealed() {
        let graph = TaskGraph::linear("цель", &[]);
        let refusal = seal_route(
            "route-1",
            &graph,
            &HashMap::new(),
            &endoscope::Verdict::Proven,
            EndoscopePolicy::Required,
        )
        .unwrap_err();
        assert!(refusal.summary().contains("граф пуст"));
    }
}
