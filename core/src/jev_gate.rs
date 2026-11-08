use crate::agent::graph::{NodeState, TaskGraph};
use crate::{endoscope, ledger, logic, proof};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Read, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub struct Gate {
    routes: HashMap<String, RouteState>,
    next_route_id: u64,
    collector: Option<String>,
}

struct RouteState {
    task: String,
    graph: TaskGraph,
    observed: HashMap<String, bool>,
    sources: Vec<ledger::Source>,
}

impl RouteState {
    fn is_research(&self) -> bool {
        self.graph
            .nodes
            .iter()
            .any(|node| node.id == "research_summary")
    }

    fn reel_ids(&self) -> Vec<String> {
        self.graph
            .nodes
            .iter()
            .filter(|node| node.id.starts_with("reel_"))
            .map(|node| node.id.clone())
            .collect()
    }
}

impl Default for Gate {
    fn default() -> Self {
        Self {
            routes: HashMap::new(),
            next_route_id: 0,
            collector: std::env::var("PCAGENT_COLLECTOR")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
        }
    }
}

impl Gate {
    pub fn handle(&mut self, req: Value) -> Option<Value> {
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let Some(method) = req.get("method").and_then(Value::as_str) else {
            return Some(rpc_error(id, -32600, "invalid request"));
        };
        if method == "notifications/initialized" {
            return None;
        }
        req.get("id")?;

        let response = match method {
            "initialize" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": {
                        "name": "pcagent-jev",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }
            }),
            "tools/list" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "tools": tool_definitions() }
            }),
            "tools/call" => {
                let params = req.get("params").unwrap_or(&Value::Null);
                let result = self.call_tool(params);
                json!({ "jsonrpc": "2.0", "id": id, "result": result })
            }
            _ => rpc_error(id, -32601, "Method not found"),
        };
        Some(response)
    }

    fn call_tool(&mut self, params: &Value) -> Value {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return tool_result(
                json!({"status":"refused","reason":"не указано имя инструмента"}),
                true,
            );
        };
        let args = params.get("arguments").unwrap_or(&Value::Null);
        match name {
            "jev_route" => {
                let Some(task) = args.get("task").and_then(Value::as_str) else {
                    return tool_result(
                        json!({"status":"refused","reason":"параметр task должен быть строкой"}),
                        true,
                    );
                };
                let have = match args.get("have") {
                    None => Vec::new(),
                    Some(Value::Array(values)) => {
                        let mut have = Vec::with_capacity(values.len());
                        for value in values {
                            let Some(fact) = value.as_str() else {
                                return tool_result(
                                    json!({"status":"refused","reason":"элементы have должны быть строками"}),
                                    true,
                                );
                            };
                            have.push(fact.to_string());
                        }
                        have
                    }
                    Some(_) => {
                        return tool_result(
                            json!({"status":"refused","reason":"параметр have должен быть массивом строк"}),
                            true,
                        )
                    }
                };
                self.route_task(task, &have)
            }
            "jev_step_done" => {
                let (Some(route_id), Some(step_id), Some(evidence)) = (
                    args.get("route_id").and_then(Value::as_str),
                    args.get("step_id").and_then(Value::as_str),
                    args.get("evidence").and_then(Value::as_str),
                ) else {
                    return tool_result(
                        json!({"status":"refused","reason":"route_id, step_id и evidence должны быть строками"}),
                        true,
                    );
                };
                self.step_done(route_id, step_id, evidence)
            }
            "jev_collect" => {
                let Some(route_id) = args.get("route_id").and_then(Value::as_str) else {
                    return tool_result(
                        json!({"status":"refused","reason":"route_id должен быть строкой"}),
                        true,
                    );
                };
                self.collect(route_id)
            }
            "jev_close" => {
                let (Some(route_id), Some(report)) = (
                    args.get("route_id").and_then(Value::as_str),
                    args.get("report").and_then(Value::as_str),
                ) else {
                    return tool_result(
                        json!({"status":"refused","reason":"route_id и report должны быть строками"}),
                        true,
                    );
                };
                self.close(route_id, report)
            }
            _ => tool_result(
                json!({"status":"refused","reason":format!("неизвестный инструмент {name}")}),
                true,
            ),
        }
    }

    fn route_task(&mut self, task: &str, have: &[String]) -> Value {
        match proof::route(task, have) {
            proof::Routing::Unknown => {
                tool_result(route_result(proof::Routing::Unknown, "route-1"), false)
            }
            proof::Routing::Missing { kind, missing } => tool_result(
                route_result(proof::Routing::Missing { kind, missing }, "route-1"),
                false,
            ),
            proof::Routing::Route { kind, graph } => {
                self.next_route_id = self
                    .next_route_id
                    .checked_add(1)
                    .expect("route ID counter exhausted");
                let route_id = format!("route-{}", self.next_route_id);
                let observed = graph
                    .nodes
                    .iter()
                    .map(|node| (node.id.clone(), false))
                    .collect();
                let result = route_graph_result(kind.as_str(), &route_id, &graph);
                self.routes.insert(
                    route_id,
                    RouteState {
                        task: task.to_string(),
                        graph,
                        observed,
                        sources: Vec::new(),
                    },
                );
                tool_result(result, false)
            }
        }
    }

    fn step_done(&mut self, route_id: &str, step_id: &str, evidence: &str) -> Value {
        let Some(route) = self.routes.get(route_id) else {
            return refusal(format!("неизвестный маршрут {route_id}"));
        };
        let Some(node) = route.graph.nodes.iter().find(|node| node.id == step_id) else {
            return refusal(format!("неизвестный шаг {step_id}"));
        };
        if route.is_research() && collector_owned_step(step_id) {
            return refusal("этот шаг закрывает только сборщик шлюза".to_string());
        }
        if route.is_research() && step_id == "research_summary" {
            return refusal("сводку закрывает только проверка отчёта шлюзом".to_string());
        }
        let done_when = node.done_when.clone();
        if evidence.trim().is_empty() {
            return refusal("доказательство пустое".to_string());
        }
        let evidence_lower = evidence.to_lowercase();
        let groups = quoted_fragment_groups(&done_when);
        if groups.len() == 1 {
            let missing: Vec<String> = groups[0]
                .iter()
                .filter(|fragment| !evidence_lower.contains(&fragment.to_lowercase()))
                .cloned()
                .collect();
            if !missing.is_empty() {
                return refusal(format!(
                    "в доказательстве отсутствует обязательный фрагмент: {}",
                    missing.join("; ")
                ));
            }
        } else if !groups.is_empty()
            && !groups.iter().any(|group| {
                group
                    .iter()
                    .all(|fragment| evidence_lower.contains(&fragment.to_lowercase()))
            })
        {
            let alternatives: Vec<String> = groups.iter().map(|group| group.join(" + ")).collect();
            return refusal(format!(
                "в доказательстве отсутствует подходящий вариант: {}",
                alternatives.join(" или ")
            ));
        }
        let route = self.routes.get_mut(route_id).unwrap();
        match logic::allow_node_done(&route.graph, step_id, true) {
            logic::Verdict::Ok => {}
            logic::Verdict::Violation(reason) => return refusal(reason),
        }
        if let Some(node) = route.graph.nodes.iter_mut().find(|node| node.id == step_id) {
            node.state = NodeState::Done;
            node.note = evidence.to_string();
        }
        if !route.graph.completed_order.iter().any(|id| id == step_id) {
            route.graph.completed_order.push(step_id.to_string());
        }
        route.observed.insert(step_id.to_string(), true);
        let remaining: Vec<&str> = route
            .graph
            .nodes
            .iter()
            .filter(|node| node.state != NodeState::Done)
            .map(|node| node.id.as_str())
            .collect();
        tool_result(
            json!({"status":"done","step_id":step_id,"remaining":remaining}),
            false,
        )
    }

    fn collect(&mut self, route_id: &str) -> Value {
        let Some(route) = self.routes.get(route_id) else {
            return refusal(format!("неизвестный маршрут {route_id}"));
        };
        if !route.is_research() {
            return refusal("сборщик доступен только для маршрута InstagramResearch".to_string());
        }
        if !route.sources.is_empty() {
            return refusal("сборщик уже запускался для этого маршрута".to_string());
        }
        let task = route.task.clone();
        let reel_ids = route.reel_ids();
        let reel_count = reel_ids.len();
        let Some(command) = self.collector.as_deref() else {
            return tool_result(
                json!({
                    "status":"missing",
                    "reason":"сборщик роликов не настроен: PCAGENT_COLLECTOR — команда, которая выдаёт JSON [Source]"
                }),
                true,
            );
        };
        let sources = match run_collector(command, &task, reel_count) {
            Ok(sources) => sources,
            Err(reason) => return refusal(reason),
        };

        let all_complete = exactly_complete_sources(&sources, &reel_ids);
        let source_count = sources.len();
        let complete_count = sources
            .iter()
            .filter(|source| ledger::complete(source).is_ok())
            .count();
        let route = self.routes.get_mut(route_id).unwrap();
        route.sources = sources;

        let mut completed_steps = Vec::new();
        let mut pending = Vec::new();
        if all_complete {
            for step_id in ["research_open_profile", "research_list_reels"] {
                let note = format!("Сборщик вернул ровно {reel_count} полных источников");
                if let Err(reason) = mark_route_node_done(route, step_id, &note) {
                    return refusal(reason);
                }
                completed_steps.push(step_id.to_string());
            }
        } else {
            let overall_reason = format!(
                "ожидалось {reel_count} уникальных полных источников, получено {source_count} ({complete_count} полных)"
            );
            for step_id in ["research_open_profile", "research_list_reels"] {
                pending.push(json!({"step_id":step_id,"reason":overall_reason.clone()}));
            }
        }
        for reel_id in &reel_ids {
            let matching: Vec<&ledger::Source> = route
                .sources
                .iter()
                .filter(|source| source.id == *reel_id)
                .collect();
            if matching.len() != 1 {
                let reason = if matching.is_empty() {
                    format!("источник {reel_id} не получен")
                } else {
                    format!("получено несколько источников с id {reel_id}")
                };
                pending.push(json!({"step_id":reel_id,"reason":reason}));
                continue;
            }
            if let Err(reason) = ledger::complete(matching[0]) {
                pending.push(json!({"step_id":reel_id,"reason":reason}));
                continue;
            }
            let note = format!("Полный транскрипт источника {reel_id} собран шлюзом");
            match mark_route_node_done(route, reel_id, &note) {
                Ok(()) => completed_steps.push(reel_id.clone()),
                Err(reason) => pending.push(json!({"step_id":reel_id,"reason":reason})),
            }
        }
        tool_result(
            json!({
                "status":"collected",
                "route_id":route_id,
                "source_count":source_count,
                "complete":all_complete,
                "completed_steps":completed_steps,
                "pending":pending
            }),
            false,
        )
    }

    fn close(&mut self, route_id: &str, report: &str) -> Value {
        let Some(route) = self.routes.get(route_id) else {
            return refusal(format!("неизвестный маршрут {route_id}"));
        };
        if route.is_research() {
            let check = ledger::check_report(report, &route.sources);
            let verified_report = check.verified_text();
            if !check.rejected.is_empty() || !check.uncited_sources.is_empty() {
                let rejected: Vec<Value> = check
                    .rejected
                    .iter()
                    .map(|(statement, why)| json!({"statement":statement,"why":why}))
                    .collect();
                return tool_result(
                    json!({
                        "status":"open",
                        "rejected":rejected,
                        "uncited":check.uncited_sources,
                        "verified_report":verified_report
                    }),
                    true,
                );
            }
            let route = self.routes.get_mut(route_id).unwrap();
            let summary_done = route
                .graph
                .nodes
                .iter()
                .any(|node| node.id == "research_summary" && node.state == NodeState::Done);
            if !summary_done {
                if let Err(reason) =
                    mark_route_node_done(route, "research_summary", &verified_report)
                {
                    return refusal(reason);
                }
            }
            let route = self.routes.get(route_id).unwrap();
            match logic::check_solution(&route.graph, &route.observed) {
                logic::Verdict::Ok => {}
                logic::Verdict::Violation(reason) => return refusal(reason),
            }
            let verdict = endoscope::judge_report(&verified_report);
            return self.close_after_proof(route_id, &verdict);
        }

        match logic::check_solution(&route.graph, &route.observed) {
            logic::Verdict::Ok => {}
            logic::Verdict::Violation(reason) => return refusal(reason),
        }
        let verdict = endoscope::judge_report(report);
        self.close_after_proof(route_id, &verdict)
    }

    fn close_after_proof(&mut self, route_id: &str, verdict: &endoscope::Verdict) -> Value {
        match close_endoscope_verdict(verdict) {
            Ok(()) => {
                self.routes.remove(route_id);
                tool_result(json!({"status":"closed","route_id":route_id}), false)
            }
            Err(reason) => refusal(reason),
        }
    }
}

fn close_endoscope_verdict(verdict: &endoscope::Verdict) -> Result<(), String> {
    if matches!(verdict, endoscope::Verdict::NotConfigured) {
        return Err("эндоскоп не настроен: задача остаётся открытой".to_string());
    }
    if !verdict.closes() {
        return Err(verdict.why());
    }
    Ok(())
}

fn collector_owned_step(step_id: &str) -> bool {
    step_id == "research_open_profile"
        || step_id == "research_list_reels"
        || step_id.starts_with("reel_")
}

fn mark_route_node_done(
    route: &mut RouteState,
    node_id: &str,
    evidence: &str,
) -> Result<(), String> {
    match logic::allow_node_done(&route.graph, node_id, true) {
        logic::Verdict::Ok => {}
        logic::Verdict::Violation(reason) => return Err(reason),
    }
    let Some(node) = route.graph.nodes.iter_mut().find(|node| node.id == node_id) else {
        return Err(format!("нет узла {node_id}"));
    };
    node.state = NodeState::Done;
    node.note = evidence.to_string();
    if !route.graph.completed_order.iter().any(|id| id == node_id) {
        route.graph.completed_order.push(node_id.to_string());
    }
    route.observed.insert(node_id.to_string(), true);
    Ok(())
}

fn exactly_complete_sources(sources: &[ledger::Source], reel_ids: &[String]) -> bool {
    !reel_ids.is_empty()
        && sources.len() == reel_ids.len()
        && reel_ids.iter().all(|id| {
            let mut matches = sources.iter().filter(|source| source.id == *id);
            matches
                .next()
                .is_some_and(|source| ledger::complete(source).is_ok())
                && matches.next().is_none()
        })
}

fn run_collector(
    command: &str,
    task: &str,
    reel_count: usize,
) -> Result<Vec<ledger::Source>, String> {
    let mut child = Command::new(command)
        .arg(task)
        .arg(reel_count.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| format!("не удалось запустить сборщик: {error}"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "не удалось прочитать stdout сборщика".to_string())?;
    let output_reader = thread::spawn(move || {
        let mut output = Vec::new();
        stdout.read_to_end(&mut output).map(|_| output)
    });
    let started = Instant::now();
    let timeout = Duration::from_secs(15 * 60);
    let status = loop {
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            let _ = output_reader.join();
            return Err("сборщик не завершился за 15 минут".to_string());
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(Duration::from_millis(100)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = output_reader.join();
                return Err(format!("ошибка ожидания сборщика: {error}"));
            }
        }
    };
    let output = output_reader
        .join()
        .map_err(|_| "ошибка чтения stdout сборщика".to_string())?
        .map_err(|error| format!("не удалось прочитать stdout сборщика: {error}"))?;
    if !status.success() {
        return Err(format!("сборщик завершился с кодом {status}"));
    }
    serde_json::from_slice(&output)
        .map_err(|error| format!("сборщик вернул некорректный JSON [Source]: {error}"))
}

fn route_result(routing: proof::Routing, route_id: &str) -> Value {
    match routing {
        proof::Routing::Route { kind, graph } => {
            route_graph_result(kind.as_str(), route_id, &graph)
        }
        proof::Routing::Missing { kind, missing } => {
            let available_have: Vec<&str> =
                kind.needs().into_iter().map(|(fact, _)| fact).collect();
            json!({
                "status":"missing",
                "kind":kind.as_str(),
                "missing":missing,
                "available_have":available_have
            })
        }
        proof::Routing::Unknown => {
            json!({"status":"unknown","why":"задача не из каталога операторов — маршрут не выдумывается"})
        }
    }
}

fn route_graph_result(kind: &str, route_id: &str, graph: &TaskGraph) -> Value {
    let steps: Vec<Value> = graph
        .nodes
        .iter()
        .map(|node| {
            json!({
                "id":node.id,
                "goal":node.goal,
                "done_when":node.done_when,
                "deps":node.deps
            })
        })
        .collect();
    json!({"status":"route","route_id":route_id,"kind":kind,"steps":steps})
}

pub fn route_json(task: &str, have: &[String]) -> Value {
    route_result(proof::route(task, have), "route-1")
}

fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name":"jev_route",
            "description":"Построить детерминированный маршрут из каталога операторов.",
            "inputSchema":{
                "type":"object",
                "properties":{
                    "task":{"type":"string"},
                    "have":{"type":"array","items":{"type":"string"}}
                },
                "required":["task"],
                "additionalProperties":false
            }
        }),
        json!({
            "name":"jev_step_done",
            "description":"Закрыть один шаг по зависимостям Z3 и текстовому доказательству.",
            "inputSchema":{
                "type":"object",
                "properties":{
                    "route_id":{"type":"string"},
                    "step_id":{"type":"string"},
                    "evidence":{"type":"string"}
                },
                "required":["route_id","step_id","evidence"],
                "additionalProperties":false
            }
        }),
        json!({
            "name":"jev_collect",
            "description":"Собрать транскрипты Instagram-роликов шлюзом из настроенного коллектора.",
            "inputSchema":{
                "type":"object",
                "properties":{
                    "route_id":{"type":"string"}
                },
                "required":["route_id"],
                "additionalProperties":false
            }
        }),
        json!({
            "name":"jev_close",
            "description":"Закрыть задачу только после проверки графа Z3 и эндоскопа.",
            "inputSchema":{
                "type":"object",
                "properties":{
                    "route_id":{"type":"string"},
                    "report":{"type":"string"}
                },
                "required":["route_id","report"],
                "additionalProperties":false
            }
        }),
    ]
}

fn tool_result(value: Value, is_error: bool) -> Value {
    json!({
        "content":[{"type":"text","text":value.to_string()}],
        "isError":is_error
    })
}

fn refusal(reason: String) -> Value {
    tool_result(json!({"status":"refused","reason":reason}), true)
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc":"2.0",
        "id":id,
        "error":{"code":code,"message":message}
    })
}

fn quoted_fragment_groups(text: &str) -> Vec<Vec<String>> {
    let mut open = None;
    let mut fragment = String::new();
    let mut previous_fragment_end = None;
    let mut group = Vec::new();
    let mut groups = Vec::new();
    for (index, ch) in text.char_indices() {
        match open {
            None if ch == '«' => {
                open = Some('»');
                fragment.clear();
            }
            None if ch == '"' => {
                open = Some('"');
                fragment.clear();
            }
            Some(close) if ch == close => {
                if !fragment.trim().is_empty() {
                    if previous_fragment_end.is_some_and(|end| text[end..index].contains(" или "))
                    {
                        groups.push(std::mem::take(&mut group));
                    }
                    group.push(fragment.clone());
                    previous_fragment_end = Some(index + ch.len_utf8());
                }
                open = None;
            }
            Some(_) => fragment.push(ch),
            None => {}
        }
    }
    if !group.is_empty() {
        groups.push(group);
    }
    groups
}

pub fn serve_stdio() -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    let mut gate = Gate::default();
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(request) => gate.handle(request),
            Err(error) => Some(rpc_error(
                Value::Null,
                -32700,
                &format!("Parse error: {error}"),
            )),
        };
        if let Some(response) = response {
            serde_json::to_writer(&mut output, &response)?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(id: u64, method: &str, params: Value) -> Value {
        json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
    }

    fn response_text(response: &Value) -> Value {
        let text = response["result"]["content"][0]["text"]
            .as_str()
            .expect("MCP text content");
        serde_json::from_str(text).expect("JSON tool result")
    }

    fn route(gate: &mut Gate, task: &str) -> (String, Vec<Value>) {
        let have: Vec<String> = proof::classify(task)
            .unwrap()
            .needs()
            .into_iter()
            .map(|(fact, _)| fact.to_string())
            .collect();
        let response = gate
            .handle(request(
                1,
                "tools/call",
                json!({"name":"jev_route","arguments":{"task":task,"have":have}}),
            ))
            .unwrap();
        let result = response_text(&response);
        (
            result["route_id"].as_str().unwrap().to_string(),
            result["steps"].as_array().unwrap().clone(),
        )
    }

    fn call_step(gate: &mut Gate, route_id: &str, step: &Value, evidence: &str) -> Value {
        gate.handle(request(
            2,
            "tools/call",
            json!({
                "name":"jev_step_done",
                "arguments":{
                    "route_id":route_id,
                    "step_id":step["id"],
                    "evidence":evidence
                }
            }),
        ))
        .unwrap()
    }

    fn call_collect(gate: &mut Gate, route_id: &str) -> Value {
        gate.handle(request(
            3,
            "tools/call",
            json!({"name":"jev_collect","arguments":{"route_id":route_id}}),
        ))
        .unwrap()
    }

    #[cfg(unix)]
    struct TestCollector {
        directory: std::path::PathBuf,
    }

    #[cfg(unix)]
    impl Drop for TestCollector {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    #[cfg(unix)]
    fn fake_collector(sources: &[ledger::Source]) -> (TestCollector, String) {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{SystemTime, UNIX_EPOCH};

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "pc-agent-jev-collector-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let script = directory.join("collector.sh");
        let output = serde_json::to_string(sources)
            .unwrap()
            .replace('\'', "'\\''");
        std::fs::write(&script, format!("#!/bin/sh\nprintf '%s' '{output}'\n")).unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).unwrap();
        (TestCollector { directory }, script.display().to_string())
    }

    fn research_source(id: &str, duration: u32, end: u32) -> ledger::Source {
        ledger::Source {
            id: id.to_string(),
            url: format!("https://example.test/{id}"),
            duration,
            segments: vec![
                ledger::Segment {
                    start: 0,
                    end: 30,
                    text: "На видео водитель поворачивает направо возле перекрёстка.".to_string(),
                },
                ledger::Segment {
                    start: 30,
                    end,
                    text: "В конце ролика автомобиль останавливается у дома.".to_string(),
                },
            ],
        }
    }

    #[test]
    fn initialize_and_tools_list_expose_four_tools() {
        let mut gate = Gate::default();
        let initialized = gate.handle(request(1, "initialize", json!({}))).unwrap();
        assert_eq!(initialized["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(initialized["result"]["serverInfo"]["name"], "pcagent-jev");
        assert!(gate
            .handle(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .is_none());
        let listed = gate.handle(request(2, "tools/list", json!({}))).unwrap();
        assert_eq!(listed["result"]["tools"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn missing_collector_returns_configuration_instruction() {
        let mut gate = Gate::default();
        gate.collector = None;
        let (route_id, _) = route(&mut gate, "исследуй последний 1 reels instagram");
        let response = call_collect(&mut gate, &route_id);
        let result = response_text(&response);
        assert_eq!(result["status"], "missing");
        assert_eq!(
            result["reason"],
            "сборщик роликов не настроен: PCAGENT_COLLECTOR — команда, которая выдаёт JSON [Source]"
        );
    }

    #[cfg(unix)]
    #[test]
    fn collector_closes_profile_list_and_complete_reel_nodes() {
        let sources = vec![
            research_source("reel_1", 60, 60),
            research_source("reel_2", 60, 60),
        ];
        let (_collector, executable) = fake_collector(&sources);
        let mut gate = Gate::default();
        gate.collector = Some(executable);
        let (route_id, steps) = route(&mut gate, "исследуй последние 2 reels instagram");

        let response = call_collect(&mut gate, &route_id);
        assert_eq!(response["result"]["isError"], false);
        let result = response_text(&response);
        assert_eq!(result["status"], "collected");
        assert_eq!(result["complete"], true);
        let route_state = gate.routes.get(&route_id).unwrap();
        for step_id in [
            "research_open_profile",
            "research_list_reels",
            "reel_1",
            "reel_2",
        ] {
            let node = route_state
                .graph
                .nodes
                .iter()
                .find(|node| node.id == step_id)
                .unwrap();
            assert_eq!(node.state, NodeState::Done);
            assert_eq!(route_state.observed.get(step_id), Some(&true));
        }

        let reel_step = steps.iter().find(|step| step["id"] == "reel_1").unwrap();
        let refused = call_step(&mut gate, &route_id, reel_step, "полный транскрипт");
        assert_eq!(refused["result"]["isError"], true);
        assert_eq!(
            response_text(&refused)["reason"],
            "этот шаг закрывает только сборщик шлюза"
        );
        let profile_step = steps
            .iter()
            .find(|step| step["id"] == "research_open_profile")
            .unwrap();
        let refused = call_step(&mut gate, &route_id, profile_step, "профиль открыт");
        assert_eq!(refused["result"]["isError"], true);
        assert_eq!(
            response_text(&refused)["reason"],
            "этот шаг закрывает только сборщик шлюза"
        );
    }

    #[cfg(unix)]
    #[test]
    fn truncated_source_stays_open_while_another_complete_reel_closes() {
        let (_collector, executable) = fake_collector(&[
            research_source("reel_1", 60, 60),
            research_source("reel_2", 100, 72),
        ]);
        let mut gate = Gate::default();
        gate.collector = Some(executable);
        let (route_id, _) = route(&mut gate, "исследуй последние 2 reels instagram");

        let response = call_collect(&mut gate, &route_id);
        let result = response_text(&response);
        assert_eq!(result["complete"], false);
        assert!(result["pending"]
            .as_array()
            .unwrap()
            .iter()
            .any(|step| step["reason"].as_str().unwrap().contains("01:12")));
        let route_state = gate.routes.get(&route_id).unwrap();
        for step_id in ["research_open_profile", "research_list_reels", "reel_2"] {
            assert_eq!(
                route_state
                    .graph
                    .nodes
                    .iter()
                    .find(|node| node.id == step_id)
                    .unwrap()
                    .state,
                NodeState::Pending
            );
        }
        assert_eq!(
            route_state
                .graph
                .nodes
                .iter()
                .find(|node| node.id == "reel_1")
                .unwrap()
                .state,
            NodeState::Done
        );
    }

    #[cfg(unix)]
    #[test]
    fn research_close_lists_fabricated_claims_as_rejected() {
        let sources = vec![
            research_source("reel_1", 60, 60),
            research_source("reel_2", 60, 60),
        ];
        let (_collector, executable) = fake_collector(&sources);
        let mut gate = Gate::default();
        gate.collector = Some(executable);
        let (route_id, _) = route(&mut gate, "исследуй последние 2 reels instagram");
        let collected = call_collect(&mut gate, &route_id);
        assert_eq!(response_text(&collected)["complete"], true);

        let claim = "Водитель поворачивает направо возле перекрёстка на 9-й улице [reel_1@00:20].";
        let response = gate
            .handle(request(
                4,
                "tools/call",
                json!({"name":"jev_close","arguments":{"route_id":route_id,"report":claim}}),
            ))
            .unwrap();
        let result = response_text(&response);
        assert_eq!(result["status"], "open");
        assert_eq!(result["rejected"][0]["statement"], claim);
        assert!(result["rejected"][0]["why"]
            .as_str()
            .unwrap()
            .contains("число 9"));
        assert_eq!(result["uncited"], json!(["reel_1", "reel_2"]));
    }

    #[cfg(unix)]
    #[test]
    fn verified_research_report_reaches_endoscope_after_summary_closes() {
        let sources = vec![
            research_source("reel_1", 60, 60),
            research_source("reel_2", 60, 60),
        ];
        let (_collector, executable) = fake_collector(&sources);
        let mut gate = Gate::default();
        gate.collector = Some(executable);
        let (route_id, _) = route(&mut gate, "исследуй последние 2 reels instagram");
        let collected = call_collect(&mut gate, &route_id);
        assert_eq!(response_text(&collected)["complete"], true);

        let report = "Водитель поворачивает направо возле перекрёстка [reel_1@00:20]. В конце ролика автомобиль останавливается у дома [reel_2@00:40].";
        let response = gate
            .handle(request(
                5,
                "tools/call",
                json!({"name":"jev_close","arguments":{"route_id":route_id,"report":report}}),
            ))
            .unwrap();
        let result = response_text(&response);
        assert_eq!(result["status"], "refused");
        assert!(result["reason"]
            .as_str()
            .unwrap()
            .contains("эндоскоп не настроен"));
        let route_state = gate.routes.get(&route_id).unwrap();
        assert_eq!(
            route_state
                .graph
                .nodes
                .iter()
                .find(|node| node.id == "research_summary")
                .unwrap()
                .state,
            NodeState::Done
        );
    }

    #[test]
    fn route_reports_missing_facts_and_unknown_tasks_without_inventing_steps() {
        let mut gate = Gate::default();
        let missing = gate
            .handle(request(
                1,
                "tools/call",
                json!({"name":"jev_route","arguments":{"task":"сделай пост в instagram"}}),
            ))
            .unwrap();
        let missing = response_text(&missing);
        assert_eq!(missing["status"], "missing");
        assert!(missing["available_have"]
            .as_array()
            .unwrap()
            .iter()
            .any(|fact| fact == "env:media_file"));

        let unknown = gate
            .handle(request(
                2,
                "tools/call",
                json!({"name":"jev_route","arguments":{"task":"придумай слоган"}}),
            ))
            .unwrap();
        assert_eq!(response_text(&unknown)["status"], "unknown");
    }

    #[test]
    fn step_done_refuses_to_close_out_of_order() {
        let mut gate = Gate::default();
        let (route_id, steps) = route(&mut gate, "прочитай директ instagram");
        let dependent = steps
            .iter()
            .find(|step| !step["deps"].as_array().unwrap().is_empty())
            .unwrap();
        let response = call_step(&mut gate, &route_id, dependent, "доказательство шага");
        assert_eq!(response["result"]["isError"], true);
        assert_eq!(response_text(&response)["status"], "refused");
    }

    #[test]
    fn step_done_requires_each_quoted_fragment_in_evidence() {
        let mut gate = Gate::default();
        let (route_id, steps) = route(&mut gate, "сделай пост в instagram");
        let first = steps
            .iter()
            .find(|step| step["deps"].as_array().unwrap().is_empty())
            .unwrap();
        assert!(first["done_when"].as_str().unwrap().contains('«'));
        let response = call_step(&mut gate, &route_id, first, "кнопка видна");
        assert_eq!(response["result"]["isError"], true);
        assert!(response_text(&response)["reason"]
            .as_str()
            .unwrap()
            .contains("обязательный фрагмент"));
    }

    #[test]
    fn step_done_accepts_any_quoted_alternative() {
        let task = "сделай пост в instagram";
        let mut gate = Gate::default();
        let (route_id, steps) = route(&mut gate, task);
        let first = steps
            .iter()
            .find(|step| step["deps"].as_array().unwrap().is_empty())
            .unwrap();
        gate.routes
            .get_mut(&route_id)
            .unwrap()
            .graph
            .nodes
            .iter_mut()
            .find(|node| node.id == first["id"].as_str().unwrap())
            .unwrap()
            .done_when = "на экране видно «вариант A» или «вариант B»".to_string();
        let accepted = call_step(&mut gate, &route_id, first, "видно вариант A");
        assert_eq!(accepted["result"]["isError"], false);

        let (second_route_id, second_steps) = route(&mut gate, task);
        let second_first = second_steps
            .iter()
            .find(|step| step["deps"].as_array().unwrap().is_empty())
            .unwrap();
        gate.routes
            .get_mut(&second_route_id)
            .unwrap()
            .graph
            .nodes
            .iter_mut()
            .find(|node| node.id == second_first["id"].as_str().unwrap())
            .unwrap()
            .done_when = "на экране видно «вариант A» или «вариант B»".to_string();
        let refused = call_step(
            &mut gate,
            &second_route_id,
            second_first,
            "виден другой вариант",
        );
        assert_eq!(refused["result"]["isError"], true);
        assert_eq!(response_text(&refused)["status"], "refused");
    }

    #[test]
    fn close_refuses_an_unfinished_route() {
        let mut gate = Gate::default();
        let (route_id, _) = route(&mut gate, "отрендери ролик с заменой лица");
        let response = gate
            .handle(request(
                3,
                "tools/call",
                json!({"name":"jev_close","arguments":{"route_id":route_id,"report":"готово"}}),
            ))
            .unwrap();
        assert_eq!(response["result"]["isError"], true);
        assert_eq!(response_text(&response)["status"], "refused");
    }

    #[test]
    fn all_steps_done_without_endoscope_stays_open() {
        let mut gate = Gate::default();
        let (route_id, steps) = route(&mut gate, "отрендери ролик с заменой лица");
        for step in &steps {
            let evidence = step["done_when"].as_str().unwrap();
            let response = call_step(&mut gate, &route_id, step, evidence);
            assert_eq!(response["result"]["isError"], false, "{response}");
        }
        let route = gate.routes.get(&route_id).unwrap();
        assert_eq!(
            logic::check_solution(&route.graph, &route.observed),
            logic::Verdict::Ok
        );
        let response = gate.close_after_proof(&route_id, &endoscope::Verdict::NotConfigured);
        assert_eq!(response["isError"], true);
        assert!(response["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("эндоскоп не настроен"));

        let response = gate.close_after_proof(&route_id, &endoscope::Verdict::Proven);
        assert_eq!(response["isError"], false);
        let closed: Value =
            serde_json::from_str(response["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(closed["status"], "closed");
        assert!(!gate.routes.contains_key(&route_id));

        let later = gate
            .handle(request(
                4,
                "tools/call",
                json!({"name":"jev_close","arguments":{"route_id":route_id,"report":"готово"}}),
            ))
            .unwrap();
        assert_eq!(later["result"]["isError"], true);
        assert!(response_text(&later)["reason"]
            .as_str()
            .unwrap()
            .contains("неизвестный маршрут"));
    }

    #[test]
    fn route_ids_are_not_reused_after_successful_close() {
        let mut gate = Gate::default();
        let (route_a, steps_a) = route(&mut gate, "отрендери ролик с заменой лица");
        let closed = gate.close_after_proof(&route_a, &endoscope::Verdict::Proven);
        assert_eq!(closed["isError"], false);

        let (route_b, _) = route(&mut gate, "отрендери ролик с заменой лица");
        assert_ne!(route_a, route_b);

        let stale_step = call_step(
            &mut gate,
            &route_a,
            &steps_a[0],
            steps_a[0]["done_when"].as_str().unwrap(),
        );
        assert_eq!(stale_step["result"]["isError"], true);
        assert!(response_text(&stale_step)["reason"]
            .as_str()
            .unwrap()
            .contains("неизвестный маршрут"));
    }

    #[test]
    fn unknown_method_returns_json_rpc_method_not_found() {
        let mut gate = Gate::default();
        let response = gate
            .handle(request(9, "no/such/method", json!({})))
            .unwrap();
        assert_eq!(response["error"]["code"], -32601);
    }

    #[test]
    fn unknown_route_and_step_are_refused() {
        let mut gate = Gate::default();
        let response = gate
            .handle(request(
                1,
                "tools/call",
                json!({"name":"jev_step_done","arguments":{
                    "route_id":"missing","step_id":"step","evidence":"text"
                }}),
            ))
            .unwrap();
        assert_eq!(response["result"]["isError"], true);
    }
}
