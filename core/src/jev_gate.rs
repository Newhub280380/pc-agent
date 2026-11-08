use crate::agent::graph::{NodeState, TaskGraph};
use crate::{endoscope, logic, proof};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Write};

#[derive(Default)]
pub struct Gate {
    routes: HashMap<String, (TaskGraph, HashMap<String, bool>)>,
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
                let mut serial = self.routes.len() + 1;
                let route_id = loop {
                    let candidate = format!("route-{serial}");
                    if !self.routes.contains_key(&candidate) {
                        break candidate;
                    }
                    serial += 1;
                };
                let observed = graph
                    .nodes
                    .iter()
                    .map(|node| (node.id.clone(), false))
                    .collect();
                let result = route_graph_result(kind.as_str(), &route_id, &graph);
                self.routes.insert(route_id, (graph, observed));
                tool_result(result, false)
            }
        }
    }

    fn step_done(&mut self, route_id: &str, step_id: &str, evidence: &str) -> Value {
        let Some((graph, observed)) = self.routes.get_mut(route_id) else {
            return refusal(format!("неизвестный маршрут {route_id}"));
        };
        let Some(node) = graph.nodes.iter().find(|node| node.id == step_id) else {
            return refusal(format!("неизвестный шаг {step_id}"));
        };
        if evidence.trim().is_empty() {
            return refusal("доказательство пустое".to_string());
        }
        let evidence_lower = evidence.to_lowercase();
        let groups = quoted_fragment_groups(&node.done_when);
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
        match logic::allow_node_done(graph, step_id, true) {
            logic::Verdict::Ok => {}
            logic::Verdict::Violation(reason) => return refusal(reason),
        }
        if let Some(node) = graph.nodes.iter_mut().find(|node| node.id == step_id) {
            node.state = NodeState::Done;
            node.note = evidence.to_string();
        }
        if !graph.completed_order.iter().any(|id| id == step_id) {
            graph.completed_order.push(step_id.to_string());
        }
        observed.insert(step_id.to_string(), true);
        let remaining: Vec<&str> = graph
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

    fn close(&mut self, route_id: &str, report: &str) -> Value {
        let Some((graph, observed)) = self.routes.get(route_id) else {
            return refusal(format!("неизвестный маршрут {route_id}"));
        };
        match logic::check_solution(graph, observed) {
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

    #[test]
    fn initialize_and_tools_list_expose_three_tools() {
        let mut gate = Gate::default();
        let initialized = gate.handle(request(1, "initialize", json!({}))).unwrap();
        assert_eq!(initialized["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(initialized["result"]["serverInfo"]["name"], "pcagent-jev");
        assert!(gate
            .handle(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .is_none());
        let listed = gate.handle(request(2, "tools/list", json!({}))).unwrap();
        assert_eq!(listed["result"]["tools"].as_array().unwrap().len(), 3);
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
            .0
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
            .0
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
        let (graph, observed) = gate.routes.get(&route_id).unwrap();
        assert_eq!(logic::check_solution(graph, observed), logic::Verdict::Ok);
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
