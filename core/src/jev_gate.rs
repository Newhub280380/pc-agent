use crate::agent::graph::{NodeState, TaskGraph};
use crate::{back_gate, corpus, endoscope, ledger, logic, proof, provenance, witness};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Write};

pub struct Gate {
    routes: HashMap<String, RouteState>,
    next_route_id: u64,
    collector: Option<String>,
    studies: HashMap<String, StudyState>,
    next_study_id: u64,
    pub extractor: Option<Box<dyn corpus::Extract + Send>>,
    /// Реестр закрытых маршрутов и исследований: ключ — `route-N` или
    /// `study-N`, значение — печать задних ворот. Запись сюда делает
    /// только `close`/`close_study` после `back_gate::seal_*`; повторное
    /// закрытие того же id отклоняется, а `jev_status` показывает печать.
    closed: HashMap<String, back_gate::Seal>,
    /// Политика доказательств (профиль strict/custom). Меняет пороги, но
    /// не само требование: без свидетеля шлюза шаг не закрывается.
    policy: witness::Policy,
    /// Архив закрытых маршрутов (журнал и свидетели) для `jev_report`.
    archive: HashMap<String, RouteState>,
    /// Эндоскоп — отдельный самостоятельный продукт. Агент обязан работать
    /// и без него: тогда печать ставят свидетели шлюза + Z3, а в печати
    /// честно стоит `endoscope_not_configured`. Обязательным эндоскоп
    /// становится, только если его явно подключили (`ENDOSCOPE_URL`,
    /// `ENDOSCOPE_RUN_ID` или `ENDOSCOPE_REQUIRED`).
    endoscope: back_gate::EndoscopePolicy,
    /// Кто сейчас вызывает шлюз. По stdio — `PCAGENT_CALLER` или `local`;
    /// по HTTP — имя агента, взятое шлюзом из его токена. Аргументы
    /// инструментов эту личность не меняют.
    caller: String,
    /// Реестр авторства артефактов и сводка по агентам.
    provenance: provenance::Registry,
    /// Ограничения для удалённых (облачных) агентов. `None` — локальный
    /// stdio-режим, где агент и так сидит на этой машине.
    remote: Option<RemotePolicy>,
    /// Подмена приговора эндоскопа только в тестах: в бою приговор всегда
    /// берётся из `endoscope::judge_report`, обходного пути нет.
    #[cfg(test)]
    judge: Option<Judge>,
}

/// Подменный судья для тестов: отчёт → приговор эндоскопа.
#[cfg(test)]
type Judge = Box<dyn Fn(&str) -> endoscope::Verdict + Send>;

struct RouteState {
    task: String,
    graph: TaskGraph,
    /// Подтверждение шлюза по узлу: `true` ставится только когда критерий
    /// узла сверен с содержимым, захваченным самим шлюзом (свидетели) или
    /// собранным его сборщиком. Слова агента сюда не попадают.
    observed: HashMap<String, bool>,
    sources: Vec<ledger::Source>,
    /// Предпосылки, заявленные вызывающей стороной в `jev_route`. Шлюз их
    /// не проверяет и честно показывает в отчёте как допущения.
    have: Vec<String>,
    /// Свидетели и журнал действий маршрута.
    ledger: witness::Ledger,
    /// Агент, построивший маршрут. Чужой агент маршрут не трогает.
    owner: String,
}

/// Что разрешено облачному агенту, подключённому по HTTP. Он не сидит на
/// этой машине, поэтому шлюз не даёт ему читать произвольные файлы,
/// запускать команды и снимать экран хоста.
#[derive(Debug, Clone, Default)]
pub struct RemotePolicy {
    /// Единственный каталог, файлы из которого можно наблюдать/регистрировать.
    pub file_root: Option<std::path::PathBuf>,
}

fn remote_admits(remote: Option<&RemotePolicy>, probe: &witness::Probe) -> Result<(), String> {
    let Some(remote) = remote else {
        return Ok(());
    };
    match probe {
        witness::Probe::Url { .. } => Ok(()),
        witness::Probe::File { path } => {
            let Some(root) = &remote.file_root else {
                return Err("удалённому агенту файлы хоста недоступны: шлюз запущен без --file-root".into());
            };
            let root = root
                .canonicalize()
                .map_err(|e| format!("каталог артефактов недоступен: {e}"))?;
            let full = root.join(path);
            let full = full
                .canonicalize()
                .map_err(|e| format!("файл {path} недоступен: {e}"))?;
            if full.starts_with(&root) {
                Ok(())
            } else {
                Err(format!("файл {path} вне каталога артефактов"))
            }
        }
        _ => Err(format!(
            "удалённому агенту наблюдение {} на хосте запрещено: допустимы url и file внутри каталога артефактов",
            probe.kind()
        )),
    }
}

/// Для удалённого агента путь файла задаётся относительно каталога
/// артефактов; шлюз подставляет полный путь сам.
fn resolve_probe(remote: Option<&RemotePolicy>, probe: witness::Probe) -> witness::Probe {
    match (remote.and_then(|r| r.file_root.as_ref()), probe) {
        (Some(root), witness::Probe::File { path }) => witness::Probe::File {
            path: root.join(path).to_string_lossy().into_owned(),
        },
        (_, probe) => probe,
    }
}

struct StudyState {
    sources: Vec<ledger::Source>,
    walk: corpus::Walk,
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
            studies: HashMap::new(),
            next_study_id: 0,
            extractor: None,
            closed: HashMap::new(),
            policy: witness::Policy::from_env(),
            archive: HashMap::new(),
            endoscope: endoscope_policy_from_env(),
            caller: std::env::var("PCAGENT_CALLER")
                .ok()
                .map(|v| provenance::normalize(&v))
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "local".to_string()),
            provenance: provenance::Registry::from_env(),
            remote: None,
            #[cfg(test)]
            judge: None,
        }
    }
}

/// Эндоскоп обязателен только когда его явно подключили в окружении.
fn endoscope_policy_from_env() -> back_gate::EndoscopePolicy {
    let set = |k: &str| std::env::var(k).is_ok_and(|v| !v.trim().is_empty());
    if set("ENDOSCOPE_URL") || set("ENDOSCOPE_RUN_ID") || set("ENDOSCOPE_REQUIRED") {
        back_gate::EndoscopePolicy::Required
    } else {
        back_gate::EndoscopePolicy::Optional
    }
}

impl Gate {
    /// Шлюз для облачных агентов (HTTP): ограниченные наблюдения.
    pub fn remote(policy: RemotePolicy) -> Self {
        Self {
            remote: Some(policy),
            ..Self::default()
        }
    }

    /// Обработать запрос от имени агента, личность которого установил
    /// транспорт (по токену). Это единственный способ задать `caller`.
    pub fn handle_as(&mut self, caller: &str, req: Value) -> Option<Value> {
        self.caller = provenance::normalize(caller);
        self.handle(req)
    }

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
                    // Облачные клиенты (Streamable HTTP) просят новые версии
                    // протокола; набор инструментов от этого не меняется.
                    "protocolVersion": req
                        .pointer("/params/protocolVersion")
                        .and_then(Value::as_str)
                        .filter(|v| matches!(*v, "2025-03-26" | "2025-06-18"))
                        .unwrap_or("2024-11-05"),
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

    /// Вход всех инструментов: проверка владельца маршрута и учёт вызова
    /// в сводке агента.
    fn call_tool(&mut self, params: &Value) -> Value {
        let args = params.get("arguments").unwrap_or(&Value::Null);
        let result = match self.foreign_route(args) {
            Some(reason) => refusal(reason),
            None => self.dispatch_tool(params),
        };
        let refused = result.get("isError").and_then(Value::as_bool) == Some(true);
        let caller = self.caller.clone();
        self.provenance.note_call(&caller, refused);
        result
    }

    /// Маршрут принадлежит тому агенту, который его построил. Пять облачных
    /// агентов на одном шлюзе не могут закрывать шаги друг за друга.
    fn foreign_route(&self, args: &Value) -> Option<String> {
        let id = args
            .get("route_id")
            .or_else(|| args.get("id"))
            .and_then(Value::as_str)?;
        let route = self.routes.get(id).or_else(|| self.archive.get(id))?;
        (route.owner != self.caller).then(|| {
            format!(
                "маршрут {id} принадлежит агенту {}, а вызывает {}",
                route.owner, self.caller
            )
        })
    }

    /// Наблюдение артефакта для реестра авторства (вне маршрута).
    fn observe_artifact(&self, artifact: &Value) -> Result<witness::Observation, String> {
        let probe = witness::Probe::from_json(artifact)?;
        if !matches!(
            probe,
            witness::Probe::File { .. } | witness::Probe::Url { .. }
        ) {
            return Err("artifact.kind должен быть file или url".into());
        }
        remote_admits(self.remote.as_ref(), &probe)?;
        witness::observe(resolve_probe(self.remote.as_ref(), probe))
    }

    fn register_artifact(&mut self, args: &Value) -> Value {
        let caller = self.caller.clone();
        let (sha, bytes, locator, source) = if let Some(artifact) = args.get("artifact") {
            match self.observe_artifact(artifact) {
                Ok(o) => (o.sha256, o.bytes, o.probe.locator(), "observed"),
                Err(reason) => return refusal(reason),
            }
        } else if let Some(sha) = args.get("sha256").and_then(Value::as_str) {
            if !provenance::valid_sha256(sha) {
                return refusal("sha256 должен быть 64 hex-символа".into());
            }
            (
                sha.to_lowercase(),
                args.get("bytes").and_then(Value::as_u64).unwrap_or(0),
                args.get("locator")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                "declared",
            )
        } else {
            return refusal("нужен artifact {kind:file|url} или sha256".into());
        };
        let author = self
            .provenance
            .register(&caller, &sha, bytes, &locator, source);
        if author.author == caller {
            tool_result(
                json!({"status":"registered","author":caller,"record":author.json()}),
                false,
            )
        } else {
            // Хэш уже принадлежит другому: агент принёс чужую работу.
            tool_result(
                json!({
                    "status":"refused",
                    "reason":format!("этот артефакт раньше зарегистрировал {} — автор он, а не {caller}", author.author),
                    "record":author.json()
                }),
                true,
            )
        }
    }

    fn attest(&mut self, args: &Value) -> Value {
        let (Some(artifact), Some(claimed)) = (
            args.get("artifact"),
            args.get("claimed_author").and_then(Value::as_str),
        ) else {
            return refusal("нужны artifact {kind:file|url} и claimed_author".into());
        };
        let observation = match self.observe_artifact(artifact) {
            Ok(o) => o,
            Err(reason) => return refusal(reason),
        };
        let caller = self.caller.clone();
        let locator = observation.probe.locator();
        match self
            .provenance
            .attest(&caller, claimed, &observation.sha256, &locator)
        {
            provenance::Verdict::Confirmed(record) => tool_result(
                json!({
                    "status":"confirmed",
                    "verdict":format!("ПОДТВЕРЖДЕНО ШЛЮЗОМ: автор {}", record.author),
                    "sha256":observation.sha256,
                    "record":record.json()
                }),
                false,
            ),
            provenance::Verdict::Mismatch { claimed, author } => tool_result(
                json!({
                    "status":"mismatch",
                    "verdict":format!("ВЫДУМКА: заявлен автор {claimed}, а по реестру автор {}", author.author),
                    "sha256":observation.sha256,
                    "record":author.json()
                }),
                true,
            ),
            provenance::Verdict::Unproven => tool_result(
                json!({
                    "status":"unproven",
                    "verdict":format!("НЕ ПОДТВЕРЖДЕНО: этот файл никто не регистрировал, авторство {claimed} не доказано"),
                    "sha256":observation.sha256
                }),
                true,
            ),
        }
    }

    fn dispatch_tool(&mut self, params: &Value) -> Value {
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
                let (Some(route_id), Some(step_id)) = (
                    args.get("route_id").and_then(Value::as_str),
                    args.get("step_id").and_then(Value::as_str),
                ) else {
                    return tool_result(
                        json!({"status":"refused","reason":"route_id и step_id должны быть строками"}),
                        true,
                    );
                };
                // Старый параметр `evidence` (свободный текст агента)
                // больше не доказательство. Он отклоняется явно, чтобы
                // агент не думал, что текст «не дошёл».
                if args.get("evidence").is_some() {
                    return tool_result(
                        json!({
                            "status":"refused",
                            "reason":"параметр evidence упразднён: доказательством служат только свидетели шлюза — сначала jev_observe, затем witness_ids"
                        }),
                        true,
                    );
                }
                let witness_ids = match args.get("witness_ids") {
                    Some(Value::Array(values)) => {
                        let mut ids = Vec::with_capacity(values.len());
                        for value in values {
                            let Some(id) = value.as_str() else {
                                return tool_result(
                                    json!({"status":"refused","reason":"элементы witness_ids должны быть строками"}),
                                    true,
                                );
                            };
                            ids.push(id.to_string());
                        }
                        ids
                    }
                    _ => {
                        return tool_result(
                            json!({"status":"refused","reason":"witness_ids должен быть массивом идентификаторов от jev_observe"}),
                            true,
                        )
                    }
                };
                let note = args.get("note").and_then(Value::as_str).unwrap_or_default();
                self.step_done(route_id, step_id, &witness_ids, note)
            }
            "jev_observe" => {
                let (Some(route_id), Some(step_id), Some(probe)) = (
                    args.get("route_id").and_then(Value::as_str),
                    args.get("step_id").and_then(Value::as_str),
                    args.get("probe"),
                ) else {
                    return tool_result(
                        json!({"status":"refused","reason":"route_id, step_id должны быть строками, probe — объектом"}),
                        true,
                    );
                };
                let probe = match witness::Probe::from_json(probe) {
                    Ok(probe) => probe,
                    Err(reason) => return refusal(reason),
                };
                self.observe(route_id, step_id, probe)
            }
            "jev_report" => {
                let Some(id) = args.get("id").and_then(Value::as_str) else {
                    return tool_result(
                        json!({"status":"refused","reason":"id должен быть строкой"}),
                        true,
                    );
                };
                self.report(id)
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
            "jev_study" => {
                let Some(question) = args.get("question").and_then(Value::as_str) else {
                    return tool_result(
                        json!({"status":"refused","reason":"question должен быть строкой"}),
                        true,
                    );
                };
                let Some(specs) = args.get("sources").and_then(Value::as_array) else {
                    return tool_result(
                        json!({"status":"refused","reason":"sources должен быть массивом строк"}),
                        true,
                    );
                };
                let mut values = Vec::with_capacity(specs.len());
                for spec in specs {
                    let Some(spec) = spec.as_str() else {
                        return tool_result(
                            json!({"status":"refused","reason":"элементы sources должны быть строками"}),
                            true,
                        );
                    };
                    values.push(spec.to_string());
                }
                let (result, is_error) = self.study(question, &values);
                tool_result(result, is_error)
            }
            "jev_close_study" => {
                let (Some(study_id), Some(report)) = (
                    args.get("study_id").and_then(Value::as_str),
                    args.get("report").and_then(Value::as_str),
                ) else {
                    return tool_result(
                        json!({"status":"refused","reason":"study_id и report должны быть строками"}),
                        true,
                    );
                };
                self.close_study(study_id, report)
            }
            "jev_status" => {
                let Some(id) = args.get("id").and_then(Value::as_str) else {
                    return tool_result(
                        json!({"status":"refused","reason":"id должен быть строкой"}),
                        true,
                    );
                };
                self.status(id)
            }
            "jev_register_artifact" => self.register_artifact(args),
            "jev_attest" => self.attest(args),
            "jev_agents" => tool_result(self.provenance.agents_json(), false),
            _ => tool_result(
                json!({"status":"refused","reason":format!("неизвестный инструмент {name}")}),
                true,
            ),
        }
    }

    /// Приговор эндоскопа по проверенному отчёту. В бою — только живой
    /// эндоскоп; подмена возможна лишь в тестах.
    fn judge(&self, report: &str) -> endoscope::Verdict {
        #[cfg(test)]
        if let Some(judge) = self.judge.as_ref() {
            return judge(report);
        }
        endoscope::judge_report(report)
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
                        have: have.to_vec(),
                        ledger: witness::Ledger::default(),
                        owner: self.caller.clone(),
                    },
                );
                tool_result(result, false)
            }
        }
    }

    /// Наблюдение шлюза ради доказательства шага. Разрешено только для
    /// узла, который сейчас на рельсах (готов по зависимостям и ещё не
    /// закрыт): попытка наблюдать «вперёд» или «назад» отклоняется и
    /// записывается в журнал как `off_rail`. Так агент физически не может
    /// собрать доказательства по трём роликам и закрыть тридцать.
    fn observe(&mut self, route_id: &str, step_id: &str, probe: witness::Probe) -> Value {
        let Some(route) = self.routes.get_mut(route_id) else {
            return refusal(format!("неизвестный маршрут {route_id}"));
        };
        let Some(node) = route.graph.nodes.iter().find(|node| node.id == step_id) else {
            route
                .ledger
                .log("jev_observe", step_id, "refused", "неизвестный шаг");
            return refusal(format!("неизвестный шаг {step_id}"));
        };
        if route.is_research() && (collector_owned_step(step_id) || step_id == "research_summary") {
            route.ledger.log(
                "jev_observe",
                step_id,
                "refused",
                "шаг принадлежит сборщику шлюза",
            );
            return refusal(
                "этот шаг доказывает только сборщик шлюза, свидетели не нужны".to_string(),
            );
        }
        if node.state == NodeState::Done {
            route
                .ledger
                .log("jev_observe", step_id, "refused", "шаг уже закрыт");
            return refusal(format!("шаг {step_id} уже закрыт"));
        }
        let on_rail = route.graph.ready().iter().any(|ready| ready.id == step_id);
        if !on_rail {
            let current: Vec<String> = route
                .graph
                .ready()
                .iter()
                .map(|ready| ready.id.clone())
                .collect();
            route.ledger.log(
                "jev_observe",
                step_id,
                "off_rail",
                format!("текущие узлы пути: {}", current.join(", ")),
            );
            return tool_result(
                json!({
                    "status":"refused",
                    "reason":format!("шаг {step_id} сейчас не на рельсах: его зависимости не закрыты"),
                    "current_steps":current
                }),
                true,
            );
        }
        if let Err(reason) = self
            .policy
            .allows(&probe)
            .and_then(|_| remote_admits(self.remote.as_ref(), &probe))
        {
            route
                .ledger
                .log("jev_observe", step_id, "refused", reason.clone());
            return refusal(reason);
        }
        let probe = resolve_probe(self.remote.as_ref(), probe);
        let locator = probe.locator();
        match witness::observe(probe) {
            Ok(observation) => {
                let witness = route.ledger.record(route_id, step_id, observation);
                tool_result(
                    json!({
                        "status":"observed",
                        "witness":witness.json(),
                        "next":"jev_step_done"
                    }),
                    false,
                )
            }
            Err(reason) => {
                route.ledger.log(
                    "jev_observe",
                    step_id,
                    "refused",
                    format!("{locator}: {reason}"),
                );
                refusal(reason)
            }
        }
    }

    /// Закрыть шаг по свидетелям шлюза. Решение принимает код: каждый
    /// свидетель должен существовать, принадлежать этому маршруту и этому
    /// шагу, а захваченное содержимое — удовлетворять критерию `done_when`
    /// и порогу профиля. `note` агента сохраняется в журнал как
    /// непроверенный комментарий и ни на что не влияет.
    fn step_done(
        &mut self,
        route_id: &str,
        step_id: &str,
        witness_ids: &[String],
        note: &str,
    ) -> Value {
        let Some(route) = self.routes.get_mut(route_id) else {
            return refusal(format!("неизвестный маршрут {route_id}"));
        };
        let Some(node) = route.graph.nodes.iter().find(|node| node.id == step_id) else {
            route
                .ledger
                .log("jev_step_done", step_id, "refused", "неизвестный шаг");
            return refusal(format!("неизвестный шаг {step_id}"));
        };
        if route.is_research() && collector_owned_step(step_id) {
            return refusal("этот шаг закрывает только сборщик шлюза".to_string());
        }
        if route.is_research() && step_id == "research_summary" {
            return refusal("сводку закрывает только проверка отчёта шлюзом".to_string());
        }
        let done_when = node.done_when.clone();
        if witness_ids.is_empty() {
            route
                .ledger
                .log("jev_step_done", step_id, "refused", "без свидетелей");
            return refusal(
                "нет свидетелей: сначала jev_observe для этого шага, затем передайте witness_ids"
                    .to_string(),
            );
        }
        let mut witnesses = Vec::with_capacity(witness_ids.len());
        for id in witness_ids {
            match route.ledger.get(id) {
                Some(witness) if witness.step_id == step_id => witnesses.push(witness),
                Some(witness) => {
                    let reason = format!(
                        "свидетель {id} принадлежит шагу {}, а не {step_id}",
                        witness.step_id
                    );
                    route
                        .ledger
                        .log("jev_step_done", step_id, "refused", reason.clone());
                    return refusal(reason);
                }
                None => {
                    let reason = format!("свидетель {id} неизвестен шлюзу для маршрута {route_id}");
                    route
                        .ledger
                        .log("jev_step_done", step_id, "refused", reason.clone());
                    return refusal(reason);
                }
            }
        }
        if let Err(reason) = witness::satisfies(&done_when, &witnesses, &self.policy) {
            route
                .ledger
                .log("jev_step_done", step_id, "refused", reason.clone());
            return refusal(reason);
        }
        let gate_note = witnesses
            .iter()
            .map(|witness| witness.note())
            .collect::<Vec<_>>()
            .join(" | ");
        match logic::allow_node_done(&route.graph, step_id, true) {
            logic::Verdict::Ok => {}
            logic::Verdict::Violation(reason) => {
                route
                    .ledger
                    .log("jev_step_done", step_id, "refused", reason.clone());
                return refusal(reason);
            }
        }
        if let Some(node) = route.graph.nodes.iter_mut().find(|node| node.id == step_id) {
            node.state = NodeState::Done;
            // В узле остаётся доказательство словами шлюза, не агента.
            node.note = gate_note.clone();
        }
        if !route.graph.completed_order.iter().any(|id| id == step_id) {
            route.graph.completed_order.push(step_id.to_string());
        }
        route.observed.insert(step_id.to_string(), true);
        route.ledger.log(
            "jev_step_done",
            step_id,
            "accepted",
            if note.trim().is_empty() {
                gate_note
            } else {
                format!("{gate_note} | note(непроверено): {}", note.trim())
            },
        );
        let remaining: Vec<&str> = route
            .graph
            .nodes
            .iter()
            .filter(|node| node.state != NodeState::Done)
            .map(|node| node.id.as_str())
            .collect();
        // Точка возврата с максимальной глубины: последний шаг пути закрыт,
        // `remaining` пуст. Это ещё не закрытие задачи — флаг `jev_close`
        // выставляют только задние ворота в `close`, поэтому агенту явно
        // говорится, что делать дальше, а статус остаётся «open».
        let all_steps_done = remaining.is_empty();
        tool_result(
            json!({
                "status":"done",
                "step_id":step_id,
                "witnesses":witness_ids,
                "remaining":remaining,
                "all_steps_done":all_steps_done,
                "jev_close":false,
                "next":if all_steps_done {"jev_close"} else {"jev_step_done"}
            }),
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
        let sources = match corpus::run_collector(command, &[task.clone(), reel_count.to_string()])
        {
            Ok(sources) => sources,
            Err(reason) => return refusal(format!("{reason:#}")),
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

    /// Закрытие маршрута. Единственный путь к `status: "closed"` для
    /// маршрута: проверка отчёта по корпусу (для исследования), затем
    /// печать задних ворот (`back_gate::seal_route`) — все шаги пройдены,
    /// подтверждены, порядок доказан Z3, эндоскоп согласен. Любой провал
    /// возвращает перечень непройденного; никакого другого способа
    /// получить печать у шлюза нет.
    fn close(&mut self, route_id: &str, report: &str) -> Value {
        if let Some(seal) = self.closed.get(route_id) {
            return tool_result(
                json!({
                    "status":"refused",
                    "reason":format!("маршрут {route_id} уже закрыт шлюзом; повторное закрытие невозможно"),
                    "seal":seal.json()
                }),
                true,
            );
        }
        let Some(route) = self.routes.get(route_id) else {
            return refusal(format!("неизвестный маршрут {route_id}"));
        };
        // Текст отчёта, который уйдёт эндоскопу: для исследования — только
        // проверенные по корпусу утверждения, иначе — отчёт как есть
        // (эндоскоп сверяет его со своей записью прогона).
        let judged_report = if route.is_research() {
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
                        "jev_close":false,
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
            // Сводку закрывает только шлюз и только когда её зависимости
            // (сборщик, все ролики) уже пройдены. Если Z3 не разрешает —
            // не отказываем здесь общей фразой: ниже задние ворота вернут
            // точный перечень непройденных шагов, включая саму сводку.
            if !summary_done {
                let _ = mark_route_node_done(route, "research_summary", &verified_report);
            }
            verified_report
        } else {
            report.to_string()
        };

        // Эндоскоп спрашиваем только когда путь структурно цел: иначе
        // независимая проверка тратится на заведомо открытую задачу.
        let route = self.routes.get(route_id).unwrap();
        let anchor = route.ledger.anchor();
        let verdict = if back_gate::seal_route(
            route_id,
            &route.graph,
            &route.observed,
            &endoscope::Verdict::Proven,
            back_gate::EndoscopePolicy::Required,
            &anchor,
        )
        .is_ok()
        {
            self.judge(&judged_report)
        } else {
            endoscope::Verdict::NotConfigured
        };
        match back_gate::seal_route(
            route_id,
            &route.graph,
            &route.observed,
            &verdict,
            self.endoscope,
            &anchor,
        ) {
            Ok(seal) => {
                let route = self.routes.get_mut(route_id).unwrap();
                route
                    .ledger
                    .log("jev_close", "", "accepted", "печать поставлена");
                let result = json!({
                    "status":"closed",
                    "route_id":route_id,
                    "seal":seal.json(),
                    "assumptions":route.have,
                    "off_rail_attempts":route.ledger.off_rail_count()
                });
                // Закрытый маршрут уходит в архив целиком: печать и журнал
                // нужны для `jev_report` после закрытия.
                let route = self.routes.remove(route_id).unwrap();
                self.archive.insert(route_id.to_string(), route);
                self.closed.insert(route_id.to_string(), seal);
                tool_result(result, false)
            }
            Err(refused) => {
                let route = self.routes.get_mut(route_id).unwrap();
                route
                    .ledger
                    .log("jev_close", "", "refused", refused.summary());
                tool_result(refused.json(), true)
            }
        }
    }

    /// Итоговый ответ пользователю, собранный шлюзом, а не агентом.
    /// Закрыто — текст начинается с «ПОДТВЕРЖДЕНО ШЛЮЗОМ» и содержит путь,
    /// свидетелей и якорь печати. Не закрыто — «НЕ ПОДТВЕРЖДЕНО» и точный
    /// перечень непройденного. Любой текст агента без этого заголовка —
    /// не результат, а черновик.
    fn report(&self, id: &str) -> Value {
        if let Some(seal) = self.closed.get(id) {
            let archived = self.archive.get(id);
            let witnesses: Vec<Value> = archived
                .map(|route| {
                    route
                        .ledger
                        .witnesses()
                        .iter()
                        .map(witness::Witness::json)
                        .collect()
                })
                .unwrap_or_default();
            let assumptions: Vec<String> =
                archived.map(|route| route.have.clone()).unwrap_or_default();
            let mut text = format!(
                "ПОДТВЕРЖДЕНО ШЛЮЗОМ: {id} закрыт, шагов пройдено {}, якорь {}\n",
                seal.path().len(),
                seal.anchor()
            );
            text.push_str(&format!("Путь: {}\n", seal.path().join(" → ")));
            if !witnesses.is_empty() {
                text.push_str(&format!("Свидетелей шлюза: {}\n", witnesses.len()));
            }
            if !assumptions.is_empty() {
                text.push_str(&format!(
                    "Допущения, заявленные вызывающей стороной (шлюзом не проверялись): {}\n",
                    assumptions.join(", ")
                ));
            }
            return tool_result(
                json!({
                    "status":"closed",
                    "id":id,
                    "verified":true,
                    "text":text,
                    "seal":seal.json(),
                    "witnesses":witnesses,
                    "assumptions":assumptions
                }),
                false,
            );
        }
        let Some(route) = self.routes.get(id) else {
            if self.studies.contains_key(id) {
                return tool_result(
                    json!({
                        "status":"open",
                        "id":id,
                        "verified":false,
                        "text":format!("НЕ ПОДТВЕРЖДЕНО: исследование {id} не закрыто шлюзом; вызовите jev_close_study")
                    }),
                    true,
                );
            }
            return refusal(format!("неизвестный маршрут или исследование {id}"));
        };
        let refused = back_gate::seal_route(
            id,
            &route.graph,
            &route.observed,
            &endoscope::Verdict::NotConfigured,
            self.endoscope,
            &route.ledger.anchor(),
        )
        .err()
        .map(|refusal| *refusal)
        .unwrap_or_default();
        let why = if refused.is_empty() {
            "путь пройден, осталось вызвать jev_close".to_string()
        } else {
            refused.summary()
        };
        tool_result(
            json!({
                "status":"open",
                "id":id,
                "verified":false,
                "text":format!("НЕ ПОДТВЕРЖДЕНО: маршрут {id} не закрыт шлюзом — {why}"),
                "refusal":refused.json()
            }),
            true,
        )
    }

    /// Состояние маршрута или исследования глазами задних ворот: закрыт ли
    /// (есть печать) и, если нет, что именно не пройдено. Инструмент для
    /// пользователя и внешнего оркестратора — подтверждение «готово» можно
    /// показывать только при `jev_close: true`.
    fn status(&self, id: &str) -> Value {
        if let Some(seal) = self.closed.get(id) {
            return tool_result(json!({"status":"closed","id":id,"seal":seal.json()}), false);
        }
        if let Some(route) = self.routes.get(id) {
            // Печать не ставится и эндоскоп не вызывается — только
            // структурная картина пути.
            let refused = back_gate::seal_route(
                id,
                &route.graph,
                &route.observed,
                &endoscope::Verdict::NotConfigured,
                self.endoscope,
                &route.ledger.anchor(),
            )
            .err()
            .map(|refusal| refusal.json())
            .unwrap_or_else(|| json!({"status":"open","jev_close":false}));
            let steps: Vec<Value> = route
                .graph
                .nodes
                .iter()
                .map(|node| {
                    json!({
                        "id":node.id,
                        "state":format!("{:?}", node.state).to_lowercase(),
                        "observed":route.observed.get(&node.id).copied().unwrap_or(false),
                        "witnesses":route.ledger.for_step(&node.id).len()
                    })
                })
                .collect();
            let current: Vec<String> = route
                .graph
                .ready()
                .iter()
                .map(|node| node.id.clone())
                .collect();
            let mut result = refused;
            result["id"] = id.into();
            result["steps"] = Value::Array(steps);
            result["current_steps"] = json!(current);
            result["completed_order"] = json!(route.graph.completed_order);
            result["assumptions"] = json!(route.have);
            result["policy"] = self.policy.json();
            result["off_rail_attempts"] = json!(route.ledger.off_rail_count());
            result["journal"] = Value::Array(
                route
                    .ledger
                    .journal()
                    .iter()
                    .map(witness::JournalEntry::json)
                    .collect(),
            );
            return tool_result(result, false);
        }
        if let Some(study) = self.studies.get(id) {
            let total: usize = study
                .sources
                .iter()
                .map(|source| source.segments.len())
                .sum();
            return tool_result(
                json!({
                    "status":"open",
                    "jev_close":false,
                    "id":id,
                    "chunks":total,
                    "visited":study.walk.visited.len(),
                    "failed":study.walk.failed.len(),
                    "next":"jev_close_study"
                }),
                false,
            );
        }
        refusal(format!("неизвестный маршрут или исследование {id}"))
    }

    fn study(&mut self, question: &str, specs: &[String]) -> (Value, bool) {
        if question.trim().is_empty() {
            return (
                json!({"status":"refused","reason":"question не должен быть пустым"}),
                true,
            );
        }
        if specs.is_empty() {
            return (
                json!({"status":"refused","reason":"нужен хотя бы один source"}),
                true,
            );
        }
        let mut missing = Vec::new();
        let owned_extractor = if self.extractor.is_none() {
            match corpus::LlmExtract::from_config() {
                Ok(extractor) => Some(extractor),
                Err(error) => {
                    missing.push(format!("{error:#}"));
                    None
                }
            }
        } else {
            None
        };
        if specs.iter().any(|spec| spec.starts_with("collector:")) && self.collector.is_none() {
            missing.push("сборщик видео не настроен: задайте PCAGENT_COLLECTOR".to_string());
        }
        if !missing.is_empty() {
            return (json!({"status":"missing","missing":missing}), true);
        }
        let sources = match corpus::load_sources(specs, self.collector.as_deref()) {
            Ok(sources) if !sources.is_empty() => sources,
            Ok(_) => {
                return (
                    json!({"status":"refused","reason":"источники не содержат единиц материала"}),
                    true,
                )
            }
            Err(error) => {
                let reason = format!("{error:#}");
                if reason.contains("PCAGENT_COLLECTOR") {
                    return (json!({"status":"missing","missing":[reason]}), true);
                }
                return (json!({"status":"refused","reason":reason}), true);
            }
        };
        let walk = if let Some(extractor) = self.extractor.as_deref() {
            corpus::walk(question, &sources, extractor)
        } else {
            corpus::walk(
                question,
                &sources,
                owned_extractor.as_ref().expect("LLM extractor was created"),
            )
        };
        let walk = match walk {
            Ok(walk) => walk,
            Err(error) => {
                return (
                    json!({"status":"refused","reason":format!("{error:#}")}),
                    true,
                );
            }
        };

        self.next_study_id = self
            .next_study_id
            .checked_add(1)
            .expect("study ID counter exhausted");
        let study_id = format!("study-{}", self.next_study_id);
        let units: Vec<Value> = sources
            .iter()
            .map(|source| {
                json!({
                    "id":source.id,
                    "locator":source.url,
                    "chunks":source.segments.len()
                })
            })
            .collect();
        let dropped: Vec<Value> = walk
            .dropped
            .iter()
            .map(|(statement, why)| json!({"statement":statement,"why":why}))
            .collect();
        let failed: Vec<Value> = walk
            .failed
            .iter()
            .map(|(id, position, why)| json!({"id":id,"position":position,"why":why}))
            .collect();
        self.studies.insert(
            study_id.clone(),
            StudyState {
                sources,
                walk: walk.clone(),
            },
        );
        (
            json!({
                "status":"studied",
                "study_id":study_id,
                "units":units,
                "notes":walk.notes,
                "dropped":dropped,
                "failed":failed
            }),
            false,
        )
    }

    /// Закрытие исследования корпуса. Как и у маршрута, печать ставят
    /// только задние ворота: все фрагменты изучены шлюзом, источники полны,
    /// каждое утверждение отчёта проверено, эндоскоп согласен.
    fn close_study(&mut self, study_id: &str, report: &str) -> Value {
        if let Some(seal) = self.closed.get(study_id) {
            return tool_result(
                json!({
                    "status":"refused",
                    "reason":format!("исследование {study_id} уже закрыто шлюзом; повторное закрытие невозможно"),
                    "seal":seal.json()
                }),
                true,
            );
        }
        let Some(study) = self.studies.get(study_id) else {
            return tool_result(
                json!({"status":"refused","reason":format!("неизвестное исследование {study_id}")}),
                true,
            );
        };
        // Сначала корпусная проверка без эндоскопа; эндоскоп вызывается
        // только для отчёта, который корпус уже подтвердил.
        let verified_text = match back_gate::seal_study(
            study_id,
            &study.sources,
            &study.walk,
            report,
            &endoscope::Verdict::Proven,
            back_gate::EndoscopePolicy::Required,
        ) {
            Ok((_, verified_text)) => verified_text,
            Err(refused) => return tool_result(refused.json(), true),
        };
        let verdict = self.judge(&verified_text);
        match back_gate::seal_study(
            study_id,
            &study.sources,
            &study.walk,
            report,
            &verdict,
            self.endoscope,
        ) {
            Ok((seal, _)) => {
                let result = json!({"status":"closed","study_id":study_id,"seal":seal.json()});
                self.studies.remove(study_id);
                self.closed.insert(study_id.to_string(), seal);
                tool_result(result, false)
            }
            Err(refused) => tool_result(refused.json(), true),
        }
    }
}

pub fn study_json(question: &str, sources: &[String]) -> Value {
    Gate::default().study(question, sources).0
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
            "name":"jev_observe",
            "description":"Наблюдение шлюза ради доказательства текущего шага: file (path), url (url), command (program, args) или screen. Возвращает witness_id; текст агента доказательством не является.",
            "inputSchema":{
                "type":"object",
                "properties":{
                    "route_id":{"type":"string"},
                    "step_id":{"type":"string"},
                    "probe":{
                        "type":"object",
                        "properties":{
                            "kind":{"type":"string","enum":["file","url","command","screen"]},
                            "path":{"type":"string"},
                            "url":{"type":"string"},
                            "program":{"type":"string"},
                            "args":{"type":"array","items":{"type":"string"}}
                        },
                        "required":["kind"],
                        "additionalProperties":false
                    }
                },
                "required":["route_id","step_id","probe"],
                "additionalProperties":false
            }
        }),
        json!({
            "name":"jev_step_done",
            "description":"Закрыть один шаг по свидетелям шлюза (witness_ids от jev_observe) и зависимостям Z3. Критерий шага сверяется с захваченным содержимым, а не с текстом агента.",
            "inputSchema":{
                "type":"object",
                "properties":{
                    "route_id":{"type":"string"},
                    "step_id":{"type":"string"},
                    "witness_ids":{"type":"array","items":{"type":"string"}},
                    "note":{"type":"string"}
                },
                "required":["route_id","step_id","witness_ids"],
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
            "description":"Закрыть задачу через задние ворота: все шаги пройдены и подтверждены, порядок доказан Z3, эндоскоп согласен. Иначе — перечень непройденного.",
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
        json!({
            "name":"jev_study",
            "description":"Изучить все фрагменты указанных источников с проверенными заметками.",
            "inputSchema":{
                "type":"object",
                "properties":{
                    "question":{"type":"string"},
                    "sources":{"type":"array","items":{"type":"string"}}
                },
                "required":["question","sources"],
                "additionalProperties":false
            }
        }),
        json!({
            "name":"jev_close_study",
            "description":"Закрыть исследование через задние ворота: все фрагменты изучены, утверждения отчёта проверены, эндоскоп согласен.",
            "inputSchema":{
                "type":"object",
                "properties":{
                    "study_id":{"type":"string"},
                    "report":{"type":"string"}
                },
                "required":["study_id","report"],
                "additionalProperties":false
            }
        }),
        json!({
            "name":"jev_report",
            "description":"Итоговый ответ пользователю, собранный шлюзом: «ПОДТВЕРЖДЕНО ШЛЮЗОМ» с печатью и свидетелями или «НЕ ПОДТВЕРЖДЕНО» с перечнем непройденного.",
            "inputSchema":{
                "type":"object",
                "properties":{
                    "id":{"type":"string"}
                },
                "required":["id"],
                "additionalProperties":false
            }
        }),
        json!({
            "name":"jev_status",
            "description":"Состояние маршрута или исследования: есть ли печать jev_close и какие шаги не пройдены.",
            "inputSchema":{
                "type":"object",
                "properties":{
                    "id":{"type":"string"}
                },
                "required":["id"],
                "additionalProperties":false
            }
        }),
        json!({
            "name":"jev_register_artifact",
            "description":"Зарегистрировать СВОЙ артефакт (файл/картинку/дизайн) в реестре авторства. Автором записывается вызывающий агент по его токену — записать работу на другого нельзя. artifact {kind:url,url} или {kind:file,path}; либо sha256 (+bytes, locator) от хука производителя.",
            "inputSchema":{
                "type":"object",
                "properties":{
                    "artifact":{
                        "type":"object",
                        "properties":{
                            "kind":{"type":"string","enum":["file","url"]},
                            "path":{"type":"string"},
                            "url":{"type":"string"}
                        },
                        "required":["kind"],
                        "additionalProperties":false
                    },
                    "sha256":{"type":"string"},
                    "bytes":{"type":"integer"},
                    "locator":{"type":"string"}
                },
                "additionalProperties":false
            }
        }),
        json!({
            "name":"jev_attest",
            "description":"ОБЯЗАТЕЛЬНО перед словами «это сделал X»: шлюз сам скачивает/читает артефакт, считает SHA-256 и сверяет заявленного автора с реестром. Ответ: confirmed, mismatch (выдумка, засчитывается тебе) или unproven.",
            "inputSchema":{
                "type":"object",
                "properties":{
                    "artifact":{
                        "type":"object",
                        "properties":{
                            "kind":{"type":"string","enum":["file","url"]},
                            "path":{"type":"string"},
                            "url":{"type":"string"}
                        },
                        "required":["kind"],
                        "additionalProperties":false
                    },
                    "claimed_author":{"type":"string"}
                },
                "required":["artifact","claimed_author"],
                "additionalProperties":false
            }
        }),
        json!({
            "name":"jev_agents",
            "description":"Сводка по агентам: вызовы, отказы шлюза, зарегистрированные артефакты, подтверждённое авторство и пойманные выдумки; последние события.",
            "inputSchema":{"type":"object","properties":{},"additionalProperties":false}
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
    // Тесты собирают Gate через Default и затем подставляют коллектор,
    // экстрактор и судью — так читается яснее, чем литерал структуры.
    #![allow(clippy::field_reassign_with_default)]

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

    /// Файл, который «видит» шлюз вместо экрана: содержимое — то, что в бою
    /// захватили бы file/url/command/screen. Дополняется строкой-паддингом,
    /// чтобы короткие тестовые фразы проходили порог профиля по байтам
    /// (порог ловит пустые наблюдения, а не короткие).
    fn captured_file(content: &str) -> String {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("pcagent-gate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("capture-{n}.txt"));
        std::fs::write(&path, format!("{content}\n[захвачено шлюзом в тесте]\n")).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn call_observe(gate: &mut Gate, route_id: &str, step_id: &str, probe: Value) -> Value {
        gate.handle(request(
            2,
            "tools/call",
            json!({
                "name":"jev_observe",
                "arguments":{"route_id":route_id,"step_id":step_id,"probe":probe}
            }),
        ))
        .unwrap()
    }

    fn call_step_with(gate: &mut Gate, route_id: &str, step: &Value, witness_ids: Value) -> Value {
        gate.handle(request(
            2,
            "tools/call",
            json!({
                "name":"jev_step_done",
                "arguments":{
                    "route_id":route_id,
                    "step_id":step["id"],
                    "witness_ids":witness_ids
                }
            }),
        ))
        .unwrap()
    }

    /// Честный шаг: шлюз наблюдает файл с `content`, затем шаг закрывается
    /// по полученному свидетелю. Если наблюдение отклонено (шаг не на
    /// рельсах, маршрут закрыт, шаг сборщика) — возвращается этот отказ.
    fn call_step(gate: &mut Gate, route_id: &str, step: &Value, content: &str) -> Value {
        let observed = call_observe(
            gate,
            route_id,
            step["id"].as_str().unwrap(),
            json!({"kind":"file","path":captured_file(content)}),
        );
        if observed["result"]["isError"] == true {
            return observed;
        }
        let witness_id = response_text(&observed)["witness"]["witness_id"].clone();
        call_step_with(gate, route_id, step, json!([witness_id]))
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
            unit: ledger::Unit::Sec,
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
    fn initialize_and_tools_list_expose_twelve_tools() {
        let mut gate = Gate::default();
        let initialized = gate.handle(request(1, "initialize", json!({}))).unwrap();
        assert_eq!(initialized["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(initialized["result"]["serverInfo"]["name"], "pcagent-jev");
        assert!(gate
            .handle(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .is_none());
        let listed = gate.handle(request(2, "tools/list", json!({}))).unwrap();
        assert_eq!(listed["result"]["tools"].as_array().unwrap().len(), 12);
    }

    struct StudyExtractor;

    impl corpus::Extract for StudyExtractor {
        fn extract(
            &self,
            _question: &str,
            source_id: &str,
            unit: ledger::Unit,
            segment: &ledger::Segment,
        ) -> anyhow::Result<Vec<String>> {
            Ok(vec![format!(
                "{} {}",
                segment.text.trim().trim_end_matches('.'),
                ledger::citation_for_segment(source_id, unit, segment)
            )])
        }
    }

    #[test]
    fn study_tool_walks_file_sources_with_injected_extractor() {
        use std::time::{SystemTime, UNIX_EPOCH};

        let path = std::env::temp_dir().join(format!(
            "pc-agent-jev-study-{}-{}.txt",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, "Документ подтверждает важный вывод для команды.").unwrap();

        let mut gate = Gate::default();
        gate.extractor = Some(Box::new(StudyExtractor));
        let response = gate
            .handle(request(
                3,
                "tools/call",
                json!({
                    "name":"jev_study",
                    "arguments":{
                        "question":"Что подтверждает документ?",
                        "sources":[format!("file:{}", path.display())]
                    }
                }),
            ))
            .unwrap();
        let _ = std::fs::remove_file(path);

        let result = response_text(&response);
        assert_eq!(result["status"], "studied");
        assert_eq!(result["units"][0]["chunks"], 1);
        assert_eq!(result["notes"].as_array().unwrap().len(), 1);
        let study_id = result["study_id"].as_str().unwrap();
        let study = gate.studies.get(study_id).unwrap();
        let report = result["notes"][0].as_str().unwrap();
        assert!(corpus::back_gate(&study.sources, &study.walk, report).is_ok());
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
            "этот шаг доказывает только сборщик шлюза, свидетели не нужны"
        );
        let profile_step = steps
            .iter()
            .find(|step| step["id"] == "research_open_profile")
            .unwrap();
        let refused = call_step(&mut gate, &route_id, profile_step, "профиль открыт");
        assert_eq!(refused["result"]["isError"], true);
        assert_eq!(
            response_text(&refused)["reason"],
            "этот шаг доказывает только сборщик шлюза, свидетели не нужны"
        );
        // И напрямую, минуя наблюдение, шаг сборщика тоже не закрыть.
        let direct = call_step_with(&mut gate, &route_id, reel_step, json!(["w-0000"]));
        assert_eq!(
            response_text(&direct)["reason"],
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
        gate.endoscope = back_gate::EndoscopePolicy::Required;
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
        assert_eq!(result["status"], "open");
        assert_eq!(result["jev_close"], false);
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

    #[cfg(unix)]
    #[test]
    fn research_route_is_sealed_only_after_every_reel_and_claim_check() {
        let sources = vec![
            research_source("reel_1", 60, 60),
            research_source("reel_2", 60, 60),
        ];
        let (_collector, executable) = fake_collector(&sources);
        let mut gate = Gate::default();
        gate.collector = Some(executable);
        gate.judge = Some(Box::new(|_| endoscope::Verdict::Proven));
        let (route_id, _) = route(&mut gate, "исследуй последние 2 reels instagram");
        // До сборщика: ни один шаг не пройден — отказ с полным перечнем.
        let early = response_text(&call_close(&mut gate, &route_id, "готово"));
        assert_eq!(early["jev_close"], false);
        assert!(early.get("seal").is_none());

        assert_eq!(
            response_text(&call_collect(&mut gate, &route_id))["complete"],
            true
        );
        let report = "Водитель поворачивает направо возле перекрёстка [reel_1@00:20]. В конце ролика автомобиль останавливается у дома [reel_2@00:40].";
        let closed = response_text(&call_close(&mut gate, &route_id, report));
        assert_eq!(closed["status"], "closed");
        assert_eq!(closed["seal"]["jev_close"], true);
        let path = closed["seal"]["path"].as_array().unwrap();
        assert_eq!(path.last().unwrap(), "research_summary");
        assert!(path.iter().any(|id| id == "reel_2"));
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
        let result = response_text(&response);
        assert_eq!(result["status"], "open");
        assert_eq!(result["jev_close"], false);
        assert!(!result["unvisited"].as_array().unwrap().is_empty());
    }

    fn call_close(gate: &mut Gate, route_id: &str, report: &str) -> Value {
        gate.handle(request(
            4,
            "tools/call",
            json!({"name":"jev_close","arguments":{"route_id":route_id,"report":report}}),
        ))
        .unwrap()
    }

    fn call_status(gate: &mut Gate, id: &str) -> Value {
        gate.handle(request(
            6,
            "tools/call",
            json!({"name":"jev_status","arguments":{"id":id}}),
        ))
        .unwrap()
    }

    /// Проходит все шаги маршрута в порядке выдачи (он топологический).
    fn walk_all_steps(gate: &mut Gate, route_id: &str, steps: &[Value]) {
        for step in steps {
            let evidence = step["done_when"].as_str().unwrap();
            let response = call_step(gate, route_id, step, evidence);
            assert_eq!(response["result"]["isError"], false, "{response}");
        }
    }

    #[test]
    fn all_steps_done_without_endoscope_stays_open() {
        let mut gate = Gate::default();
        // Эндоскоп явно подключён как обязательный, но не отвечает.
        gate.endoscope = back_gate::EndoscopePolicy::Required;
        let (route_id, steps) = route(&mut gate, "отрендери ролик с заменой лица");
        walk_all_steps(&mut gate, &route_id, &steps);
        let route = gate.routes.get(&route_id).unwrap();
        assert_eq!(
            logic::check_solution(&route.graph, &route.observed),
            logic::Verdict::Ok
        );
        // Эндоскоп не настроен (в тестовом окружении переменных нет):
        // путь пройден целиком, но печати нет и маршрут остаётся открытым.
        let response = call_close(&mut gate, &route_id, "готово");
        assert_eq!(response["result"]["isError"], true);
        let result = response_text(&response);
        assert_eq!(result["status"], "open");
        assert_eq!(result["jev_close"], false);
        assert!(result["proof_failures"][0]
            .as_str()
            .unwrap()
            .contains("эндоскоп не настроен"));
        assert!(gate.routes.contains_key(&route_id));
        assert!(!gate.closed.contains_key(&route_id));
    }

    /// Пять облачных агентов на одном шлюзе: чужой маршрут не трогается.
    #[test]
    fn another_agent_cannot_walk_or_close_a_foreign_route() {
        let mut gate = Gate::default();
        gate.endoscope = back_gate::EndoscopePolicy::Optional;
        gate.caller = "grok-1".into();
        let (route_id, _steps) = route(&mut gate, "отрендери ролик с заменой лица");
        let foreign = response_text(
            &gate
                .handle_as(
                    "grok-2",
                    request(
                        2,
                        "tools/call",
                        json!({"name":"jev_close","arguments":{"route_id":route_id,"report":"готово"}}),
                    ),
                )
                .unwrap(),
        );
        assert_eq!(foreign["status"], "refused");
        assert!(foreign["reason"].as_str().unwrap().contains("grok-1"));
        let own = response_text(
            &gate
                .handle_as(
                    "grok-1",
                    request(
                        3,
                        "tools/call",
                        json!({"name":"jev_status","arguments":{"id":route_id}}),
                    ),
                )
                .unwrap(),
        );
        assert_ne!(own["status"], "refused");
        let agents = response_text(
            &gate
                .handle(request(
                    4,
                    "tools/call",
                    json!({"name":"jev_agents","arguments":{}}),
                ))
                .unwrap(),
        );
        let grok2 = agents["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["agent"] == "grok-2")
            .unwrap()
            .clone();
        assert_eq!(grok2["refusals"], 1);
    }

    /// Агент самостоятелен: без подключённого эндоскопа полный проход
    /// закрывают свидетели шлюза + Z3, а печать честно говорит, что
    /// независимой проверки не было.
    #[test]
    fn standalone_agent_closes_without_endoscope() {
        let mut gate = Gate::default();
        gate.endoscope = back_gate::EndoscopePolicy::Optional;
        let (route_id, steps) = route(&mut gate, "отрендери ролик с заменой лица");
        walk_all_steps(&mut gate, &route_id, &steps);
        let report = response_text(
            &gate
                .handle(request(
                    7,
                    "tools/call",
                    json!({"name":"jev_report","arguments":{"id":route_id}}),
                ))
                .unwrap(),
        );
        assert!(report["text"]
            .as_str()
            .unwrap()
            .contains("осталось вызвать jev_close"));
        let response = call_close(&mut gate, &route_id, "готово");
        assert_eq!(response["result"]["isError"], false, "{response}");
        let result = response_text(&response);
        assert_eq!(result["status"], "closed");
        assert_eq!(result["seal"]["jev_close"], true);
        let proof = result["seal"]["proof"].to_string();
        assert!(proof.contains("endoscope_not_configured"), "{proof}");
        assert!(proof.contains("witness_anchor"), "{proof}");
    }

    /// Положительный сценарий: полный проход пути → печать jev_close.
    #[test]
    fn full_walk_with_proven_endoscope_gets_the_seal() {
        let mut gate = Gate::default();
        gate.judge = Some(Box::new(|_| endoscope::Verdict::Proven));
        let (route_id, steps) = route(&mut gate, "отрендери ролик с заменой лица");
        // Последний шаг возвращает агента с глубины: remaining пуст, но
        // флага закрытия ещё нет — только указание вызвать jev_close.
        let last = steps.len() - 1;
        walk_all_steps(&mut gate, &route_id, &steps[..last]);
        let deepest = call_step(
            &mut gate,
            &route_id,
            &steps[last],
            steps[last]["done_when"].as_str().unwrap(),
        );
        let deepest = response_text(&deepest);
        assert_eq!(deepest["all_steps_done"], true);
        assert_eq!(deepest["jev_close"], false);
        assert_eq!(deepest["next"], "jev_close");

        let response = call_close(&mut gate, &route_id, "готово");
        assert_eq!(response["result"]["isError"], false, "{response}");
        let result = response_text(&response);
        assert_eq!(result["status"], "closed");
        assert_eq!(result["seal"]["jev_close"], true);
        assert_eq!(result["seal"]["steps"], steps.len());
        let path: Vec<&str> = result["seal"]["path"]
            .as_array()
            .unwrap()
            .iter()
            .map(|id| id.as_str().unwrap())
            .collect();
        let expected: Vec<&str> = steps.iter().map(|s| s["id"].as_str().unwrap()).collect();
        assert_eq!(path, expected);
        assert!(!gate.routes.contains_key(&route_id));
        assert!(gate.closed.contains_key(&route_id));

        let status = response_text(&call_status(&mut gate, &route_id));
        assert_eq!(status["status"], "closed");
        assert_eq!(status["seal"]["jev_close"], true);
    }

    /// Частичное прохождение: отказ с точным перечнем непройденных шагов.
    #[test]
    fn partial_walk_is_refused_with_exact_unvisited_list() {
        let mut gate = Gate::default();
        gate.judge = Some(Box::new(|_| endoscope::Verdict::Proven));
        let (route_id, steps) = route(&mut gate, "отрендери ролик с заменой лица");
        assert!(steps.len() >= 2);
        let half = steps.len() / 2;
        walk_all_steps(&mut gate, &route_id, &steps[..half]);

        let response = call_close(&mut gate, &route_id, "готово");
        assert_eq!(response["result"]["isError"], true);
        let result = response_text(&response);
        assert_eq!(result["status"], "open");
        assert_eq!(result["jev_close"], false);
        let unvisited: Vec<&str> = result["unvisited"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["step_id"].as_str().unwrap())
            .collect();
        let expected: Vec<&str> = steps[half..]
            .iter()
            .map(|s| s["id"].as_str().unwrap())
            .collect();
        assert_eq!(unvisited, expected);
        assert!(result["reason"]
            .as_str()
            .unwrap()
            .contains("не пройдены шаги"));
        assert!(gate.routes.contains_key(&route_id));

        let status = response_text(&call_status(&mut gate, &route_id));
        assert_eq!(status["status"], "open");
        assert_eq!(status["jev_close"], false);
        assert_eq!(
            status["unvisited"].as_array().unwrap().len(),
            steps.len() - half
        );
    }

    /// Преждевременное закрытие: ни одного шага не пройдено.
    #[test]
    fn premature_close_lists_every_step_and_keeps_route_open() {
        let mut gate = Gate::default();
        gate.judge = Some(Box::new(|_| endoscope::Verdict::Proven));
        let (route_id, steps) = route(&mut gate, "отрендери ролик с заменой лица");
        let response = call_close(&mut gate, &route_id, "всё сделано, честно");
        assert_eq!(response["result"]["isError"], true);
        let result = response_text(&response);
        assert_eq!(result["status"], "open");
        assert_eq!(result["jev_close"], false);
        assert_eq!(result["unvisited"].as_array().unwrap().len(), steps.len());
        assert!(result.get("seal").is_none());
        // После отказа путь можно продолжить: шаги принимаются как обычно.
        walk_all_steps(&mut gate, &route_id, &steps);
        let closed = response_text(&call_close(&mut gate, &route_id, "готово"));
        assert_eq!(closed["status"], "closed");
    }

    /// Повторное закрытие: печать одна, второй вызов отклоняется.
    #[test]
    fn second_close_is_refused_and_seal_is_kept() {
        let mut gate = Gate::default();
        gate.judge = Some(Box::new(|_| endoscope::Verdict::Proven));
        let (route_id, steps) = route(&mut gate, "отрендери ролик с заменой лица");
        walk_all_steps(&mut gate, &route_id, &steps);
        let first = response_text(&call_close(&mut gate, &route_id, "готово"));
        assert_eq!(first["status"], "closed");

        let again = call_close(&mut gate, &route_id, "готово ещё раз");
        assert_eq!(again["result"]["isError"], true);
        let again = response_text(&again);
        assert_eq!(again["status"], "refused");
        assert!(again["reason"].as_str().unwrap().contains("уже закрыт"));
        assert_eq!(again["seal"]["closed_at"], first["seal"]["closed_at"]);
        assert_eq!(gate.closed.len(), 1);

        // Шаги закрытого маршрута тоже не принимаются.
        let stale = call_step(
            &mut gate,
            &route_id,
            &steps[0],
            steps[0]["done_when"].as_str().unwrap(),
        );
        assert_eq!(stale["result"]["isError"], true);
    }

    /// Эндоскоп опроверг отчёт — печати нет, даже если путь пройден.
    #[test]
    fn disproven_endoscope_blocks_the_seal() {
        let mut gate = Gate::default();
        gate.judge = Some(Box::new(|_| {
            endoscope::Verdict::Disproven("файл не создавался".into())
        }));
        let (route_id, steps) = route(&mut gate, "отрендери ролик с заменой лица");
        walk_all_steps(&mut gate, &route_id, &steps);
        let result = response_text(&call_close(&mut gate, &route_id, "готово"));
        assert_eq!(result["status"], "open");
        assert_eq!(result["jev_close"], false);
        assert!(result["proof_failures"][0]
            .as_str()
            .unwrap()
            .contains("не доказано"));
        assert!(gate.routes.contains_key(&route_id));
    }

    /// Эндоскоп не вызывается, пока путь не пройден целиком: у открытого
    /// маршрута приговор запрашивать нечего.
    #[test]
    fn endoscope_is_not_consulted_for_an_unfinished_route() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let mut gate = Gate::default();
        gate.judge = Some(Box::new(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            endoscope::Verdict::Proven
        }));
        let (route_id, steps) = route(&mut gate, "отрендери ролик с заменой лица");
        call_close(&mut gate, &route_id, "готово");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        walk_all_steps(&mut gate, &route_id, &steps);
        call_close(&mut gate, &route_id, "готово");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn status_of_unknown_id_is_refused() {
        let mut gate = Gate::default();
        let response = call_status(&mut gate, "route-404");
        assert_eq!(response["result"]["isError"], true);
    }

    #[test]
    fn route_ids_are_not_reused_after_successful_close() {
        let mut gate = Gate::default();
        gate.judge = Some(Box::new(|_| endoscope::Verdict::Proven));
        let (route_a, steps_a) = route(&mut gate, "отрендери ролик с заменой лица");
        walk_all_steps(&mut gate, &route_a, &steps_a);
        let closed = call_close(&mut gate, &route_a, "готово");
        assert_eq!(closed["result"]["isError"], false);

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

    /// Главный вектор обхода: агент присылает текст критерия как
    /// «доказательство». Параметр упразднён — отказ ещё до разбора.
    #[test]
    fn agent_text_is_not_evidence_anymore() {
        let mut gate = Gate::default();
        let (route_id, steps) = route(&mut gate, "отрендери ролик с заменой лица");
        let response = gate
            .handle(request(
                2,
                "tools/call",
                json!({"name":"jev_step_done","arguments":{
                    "route_id":route_id,
                    "step_id":steps[0]["id"],
                    "evidence":steps[0]["done_when"]
                }}),
            ))
            .unwrap();
        assert_eq!(response["result"]["isError"], true);
        assert!(response_text(&response)["reason"]
            .as_str()
            .unwrap()
            .contains("evidence упразднён"));
        // Без свидетелей — тоже отказ, и узел остаётся не пройденным.
        let empty = call_step_with(&mut gate, &route_id, &steps[0], json!([]));
        assert_eq!(empty["result"]["isError"], true);
        let node_state = gate.routes[&route_id].graph.nodes[0].state;
        assert_eq!(node_state, NodeState::Pending);
    }

    /// Свидетель одного шага не закрывает другой; выдуманный id неизвестен.
    #[test]
    fn witness_is_bound_to_its_step_and_cannot_be_forged() {
        let mut gate = Gate::default();
        let (route_id, steps) = route(&mut gate, "отрендери ролик с заменой лица");
        let first = &steps[0];
        let observed = call_observe(
            &mut gate,
            &route_id,
            first["id"].as_str().unwrap(),
            json!({"kind":"file","path":captured_file(first["done_when"].as_str().unwrap())}),
        );
        let witness_id = response_text(&observed)["witness"]["witness_id"]
            .as_str()
            .unwrap()
            .to_string();
        let accepted = call_step_with(&mut gate, &route_id, first, json!([witness_id]));
        assert_eq!(accepted["result"]["isError"], false, "{accepted}");

        let second = &steps[1];
        let reused = call_step_with(&mut gate, &route_id, second, json!([witness_id]));
        assert!(response_text(&reused)["reason"]
            .as_str()
            .unwrap()
            .contains("принадлежит шагу"));
        let forged = call_step_with(&mut gate, &route_id, second, json!(["w-deadbeefdeadbeef"]));
        assert!(response_text(&forged)["reason"]
            .as_str()
            .unwrap()
            .contains("неизвестен шлюзу"));
    }

    /// Наблюдать «вперёд» по пути нельзя: попытка отклоняется и остаётся в
    /// журнале как off_rail, а статус её показывает.
    #[test]
    fn observing_ahead_of_the_rail_is_refused_and_journaled() {
        let mut gate = Gate::default();
        let (route_id, steps) = route(&mut gate, "прочитай директ instagram");
        let dependent = steps
            .iter()
            .find(|step| !step["deps"].as_array().unwrap().is_empty())
            .unwrap();
        let refused = call_observe(
            &mut gate,
            &route_id,
            dependent["id"].as_str().unwrap(),
            json!({"kind":"file","path":captured_file("что угодно")}),
        );
        assert_eq!(refused["result"]["isError"], true);
        let refused = response_text(&refused);
        assert!(refused["reason"]
            .as_str()
            .unwrap()
            .contains("не на рельсах"));
        assert!(!refused["current_steps"].as_array().unwrap().is_empty());
        let status = response_text(&call_status(&mut gate, &route_id));
        assert_eq!(status["off_rail_attempts"], 1);
        assert_eq!(status["journal"][0]["outcome"], "off_rail");
        assert_eq!(status["policy"]["profile"], "strict");
    }

    /// Критерий сверяется с захваченным содержимым: файл без нужного
    /// фрагмента — отказ, даже если агент уверяет обратное в note.
    #[test]
    fn captured_content_decides_not_the_note() {
        let mut gate = Gate::default();
        let (route_id, steps) = route(&mut gate, "сделай пост в instagram");
        let first = steps
            .iter()
            .find(|step| step["deps"].as_array().unwrap().is_empty())
            .unwrap();
        let observed = call_observe(
            &mut gate,
            &route_id,
            first["id"].as_str().unwrap(),
            json!({"kind":"file","path":captured_file("лог без признаков критерия")}),
        );
        let witness_id = response_text(&observed)["witness"]["witness_id"].clone();
        let response = gate
            .handle(request(
                2,
                "tools/call",
                json!({"name":"jev_step_done","arguments":{
                    "route_id":route_id,
                    "step_id":first["id"],
                    "witness_ids":[witness_id],
                    "note":first["done_when"]
                }}),
            ))
            .unwrap();
        assert_eq!(response["result"]["isError"], true);
        assert!(response_text(&response)["reason"]
            .as_str()
            .unwrap()
            .contains("обязательный фрагмент"));
    }

    /// Пустое наблюдение (нулевой файл) не проходит порог профиля.
    #[test]
    fn empty_capture_fails_profile_threshold() {
        let mut gate = Gate::default();
        let (route_id, steps) = route(&mut gate, "отрендери ролик с заменой лица");
        let dir = std::env::temp_dir().join(format!("pcagent-gate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("empty.txt");
        std::fs::write(&path, "").unwrap();
        let observed = call_observe(
            &mut gate,
            &route_id,
            steps[0]["id"].as_str().unwrap(),
            json!({"kind":"file","path":path.to_string_lossy()}),
        );
        let witness_id = response_text(&observed)["witness"]["witness_id"].clone();
        let response = call_step_with(&mut gate, &route_id, &steps[0], json!([witness_id]));
        assert!(response_text(&response)["reason"]
            .as_str()
            .unwrap()
            .contains("порог профиля"));
    }

    /// Итоговый ответ собирает шлюз: до печати — «НЕ ПОДТВЕРЖДЕНО» с
    /// перечнем, после — «ПОДТВЕРЖДЕНО ШЛЮЗОМ» с якорем свидетелей.
    #[test]
    fn report_is_built_by_the_gate_from_the_seal() {
        let mut gate = Gate::default();
        gate.judge = Some(Box::new(|_| endoscope::Verdict::Proven));
        let (route_id, steps) = route(&mut gate, "отрендери ролик с заменой лица");
        let call_report = |gate: &mut Gate| {
            gate.handle(request(
                5,
                "tools/call",
                json!({"name":"jev_report","arguments":{"id":route_id}}),
            ))
            .unwrap()
        };
        let before = call_report(&mut gate);
        assert_eq!(before["result"]["isError"], true);
        let before = response_text(&before);
        assert_eq!(before["verified"], false);
        assert!(before["text"]
            .as_str()
            .unwrap()
            .starts_with("НЕ ПОДТВЕРЖДЕНО"));
        assert!(!before["refusal"]["unvisited"]
            .as_array()
            .unwrap()
            .is_empty());

        walk_all_steps(&mut gate, &route_id, &steps);
        let closed = response_text(&call_close(&mut gate, &route_id, "готово"));
        assert_eq!(closed["status"], "closed");
        let anchor = closed["seal"]["anchor"].as_str().unwrap().to_string();
        assert_eq!(anchor.len(), 64);
        assert!(closed["seal"]["proof"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == "witness_anchor"));

        let after = call_report(&mut gate);
        assert_eq!(after["result"]["isError"], false);
        let after = response_text(&after);
        assert_eq!(after["verified"], true);
        assert!(after["text"]
            .as_str()
            .unwrap()
            .starts_with("ПОДТВЕРЖДЕНО ШЛЮЗОМ"));
        assert_eq!(after["seal"]["anchor"], anchor);
        assert_eq!(after["witnesses"].as_array().unwrap().len(), steps.len());
        // Допущения из have показаны явно как непроверенные.
        assert!(!after["assumptions"].as_array().unwrap().is_empty());
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
                    "route_id":"missing","step_id":"step","witness_ids":["w-1"]
                }}),
            ))
            .unwrap();
        assert_eq!(response["result"]["isError"], true);
    }
}
