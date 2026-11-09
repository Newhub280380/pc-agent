//! Шлюз Jev по HTTP (MCP Streamable HTTP) — для облачных агентов.
//!
//! Облачный Grok/бот не может запустить `pcagent --mcp` у себя: stdio ему
//! недоступен, нужен публичный URL. Этот модуль отдаёт тот же `Gate` по
//! `POST /mcp`. Отличия от stdio ровно два, оба в пользу строгости:
//!
//!   * **личность из токена.** У каждого агента свой токен (файл агентов:
//!     строки `имя токен`). Шлюз сам определяет, кто вызывает, и передаёт это
//!     в `Gate::handle_as`. Назваться другим агентом нельзя;
//!   * **ограниченные наблюдения.** Удалённому агенту доступны только `url` и
//!     файлы внутри `--file-root` (см. `jev_gate::RemotePolicy`).
//!
//! Токен принимается в `Authorization: Bearer …` или в пути `/mcp/<токен>`
//! (для клиентов, где нельзя задать заголовок — например, кастомный
//! коннектор Grok Bot). Падение обработчика не роняет сервер: соединение
//! получает 500, следующий запрос обслуживается.

use crate::jev_gate::Gate;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const HEADER_CAP: usize = 16 * 1024;
const BODY_CAP: usize = 1024 * 1024;
const MIN_TOKEN: usize = 16;

#[derive(Debug, Clone)]
pub struct Agents(Vec<(String, String)>);

impl Agents {
    /// Разбор файла агентов: `имя токен` на строку, `#` — комментарий.
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let mut list: Vec<(String, String)> = Vec::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.split_whitespace();
            let (Some(name), Some(token), None) = (parts.next(), parts.next(), parts.next()) else {
                anyhow::bail!("строка {}: ожидается `имя токен`", n + 1);
            };
            let name = crate::provenance::normalize(name);
            if token.len() < MIN_TOKEN {
                anyhow::bail!("строка {}: токен короче {MIN_TOKEN} символов", n + 1);
            }
            if list.iter().any(|(a, t)| *a == name || t == token) {
                anyhow::bail!("строка {}: имя или токен повторяется", n + 1);
            }
            list.push((name, token.to_string()));
        }
        if list.is_empty() {
            anyhow::bail!("в файле агентов нет ни одного агента");
        }
        Ok(Self(list))
    }

    /// Имя агента по токену. Сравнение без раннего выхода по символам.
    pub fn who(&self, token: &str) -> Option<&str> {
        let mut found = None;
        for (name, known) in &self.0 {
            if ct_eq(known.as_bytes(), token.as_bytes()) {
                found = Some(name.as_str());
            }
        }
        found
    }

    pub fn names(&self) -> Vec<&str> {
        self.0.iter().map(|(n, _)| n.as_str()).collect()
    }
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub authorization: Option<String>,
    pub body: Vec<u8>,
}

#[derive(Debug, PartialEq)]
pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: String,
}

impl Response {
    fn text(status: u16, body: &str) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: body.to_string(),
        }
    }
    fn json(status: u16, body: Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: body.to_string(),
        }
    }
}

/// Маршрутизация и авторизация без сокетов — тестируется напрямую.
pub fn respond(gate: &Mutex<Gate>, agents: &Agents, req: Request) -> Response {
    let path = req.path.split('?').next().unwrap_or_default();
    if req.method == "GET" && path == "/health" {
        return Response::text(200, "ok");
    }
    let path_token = path.strip_prefix("/mcp/").filter(|t| !t.is_empty());
    if path != "/mcp" && path_token.is_none() {
        return Response::text(404, "not found");
    }
    let header_token = req
        .authorization
        .as_deref()
        .and_then(|h| {
            h.strip_prefix("Bearer ")
                .or_else(|| h.strip_prefix("bearer "))
        })
        .map(str::trim);
    let Some(caller) = header_token
        .and_then(|t| agents.who(t))
        .or_else(|| path_token.and_then(|t| agents.who(t)))
        .map(str::to_string)
    else {
        return Response::text(401, "unauthorized: нужен токен агента");
    };
    if req.method != "POST" {
        // Отдельный SSE-поток сервер не держит: всё отвечается на POST.
        return Response::text(405, "method not allowed: используйте POST");
    }
    let parsed: Value = match serde_json::from_slice(&req.body) {
        Ok(v) => v,
        Err(e) => {
            return Response::json(
                400,
                json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":format!("Parse error: {e}")}}),
            )
        }
    };
    let mut gate = gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut run = |msg: Value| -> Option<Value> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| gate.handle_as(&caller, msg)))
            .unwrap_or_else(|_| {
                Some(json!({"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"internal error"}}))
            })
    };
    let reply = match parsed {
        Value::Array(batch) => {
            let out: Vec<Value> = batch.into_iter().filter_map(&mut run).collect();
            (!out.is_empty()).then_some(Value::Array(out))
        }
        msg => run(msg),
    };
    match reply {
        Some(value) => Response::json(200, value),
        // Уведомления и ответы клиента: тела нет.
        None => Response {
            status: 202,
            content_type: "text/plain; charset=utf-8",
            body: String::new(),
        },
    }
}

fn read_request(stream: &mut TcpStream) -> Result<Request, (u16, &'static str)> {
    let mut reader = BufReader::new(stream);
    let mut head = Vec::new();
    loop {
        let mut line = Vec::new();
        let n = reader
            .by_ref()
            .take((HEADER_CAP - head.len().min(HEADER_CAP)) as u64 + 1)
            .read_until(b'\n', &mut line)
            .map_err(|_| (400, "bad request"))?;
        if n == 0 {
            return Err((400, "bad request"));
        }
        head.extend_from_slice(&line);
        if head.len() > HEADER_CAP {
            return Err((431, "headers too large"));
        }
        if line == b"\r\n" || line == b"\n" {
            break;
        }
    }
    let head = String::from_utf8_lossy(&head);
    let mut lines = head.lines();
    let mut start = lines.next().unwrap_or_default().split_whitespace();
    let (Some(method), Some(path)) = (start.next(), start.next()) else {
        return Err((400, "bad request"));
    };
    let mut length = 0usize;
    let mut chunked = false;
    let mut authorization = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => length = value.parse().map_err(|_| (400, "bad content-length"))?,
            "transfer-encoding" => chunked = value.to_ascii_lowercase().contains("chunked"),
            "authorization" => authorization = Some(value.to_string()),
            _ => {}
        }
    }
    let body = if chunked {
        read_chunked(&mut reader)?
    } else {
        if length > BODY_CAP {
            return Err((413, "body too large"));
        }
        let mut body = vec![0; length];
        reader
            .read_exact(&mut body)
            .map_err(|_| (400, "short body"))?;
        body
    };
    Ok(Request {
        method: method.to_string(),
        path: path.to_string(),
        authorization,
        body,
    })
}

fn read_chunked(reader: &mut impl BufRead) -> Result<Vec<u8>, (u16, &'static str)> {
    let mut body = Vec::new();
    loop {
        let mut size = String::new();
        reader
            .take(64)
            .read_line(&mut size)
            .map_err(|_| (400, "bad chunk"))?;
        let size = usize::from_str_radix(size.trim().split(';').next().unwrap_or(""), 16)
            .map_err(|_| (400, "bad chunk"))?;
        if body.len() + size > BODY_CAP {
            return Err((413, "body too large"));
        }
        if size == 0 {
            let mut trailer = String::new();
            let _ = reader.take(1024).read_line(&mut trailer);
            return Ok(body);
        }
        let mut chunk = vec![0; size + 2];
        reader
            .read_exact(&mut chunk)
            .map_err(|_| (400, "bad chunk"))?;
        body.extend_from_slice(&chunk[..size]);
    }
}

fn write_response(stream: &mut TcpStream, response: &Response) {
    let reason = match response.status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        _ => "Internal Server Error",
    };
    let head = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        response.content_type,
        response.body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(response.body.as_bytes());
    let _ = stream.flush();
}

fn serve_connection(mut stream: TcpStream, gate: &Mutex<Gate>, agents: &Agents) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(15)));
    let response = match read_request(&mut stream) {
        Ok(req) => respond(gate, agents, req),
        Err((status, msg)) => Response::text(status, msg),
    };
    write_response(&mut stream, &response);
}

/// Принимать соединения на `listener`; поток на соединение, общий шлюз.
pub fn serve(listener: TcpListener, agents: Agents, gate: Gate) -> anyhow::Result<()> {
    let gate = Arc::new(Mutex::new(gate));
    let agents = Arc::new(agents);
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let (gate, agents) = (gate.clone(), agents.clone());
        std::thread::spawn(move || serve_connection(stream, &gate, &agents));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev_gate::RemotePolicy;

    const T1: &str = "grok-token-aaaaaaaaaaaa";
    const T2: &str = "claude-token-bbbbbbbbbb";

    fn agents() -> Agents {
        Agents::parse(&format!("# агенты\ngrok-1 {T1}\nclaude {T2}\n")).unwrap()
    }

    fn post(path: &str, auth: Option<&str>, body: Value) -> Request {
        Request {
            method: "POST".into(),
            path: path.into(),
            authorization: auth.map(|t| format!("Bearer {t}")),
            body: body.to_string().into_bytes(),
        }
    }

    fn call(name: &str, args: Value) -> Value {
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}})
    }

    fn payload(r: &Response) -> Value {
        let v: Value = serde_json::from_str(&r.body).unwrap();
        serde_json::from_str(v["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    #[test]
    fn agents_file_rejects_short_and_duplicate_tokens() {
        assert!(Agents::parse("a short").is_err());
        assert!(Agents::parse(&format!("a {T1}\nb {T1}")).is_err());
        assert!(Agents::parse("# пусто").is_err());
        assert_eq!(agents().who(T2), Some("claude"));
        assert_eq!(agents().who("nope"), None);
    }

    #[test]
    fn unauthorized_requests_never_reach_the_gate() {
        let gate = Mutex::new(Gate::remote(RemotePolicy::default()));
        let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}});
        assert_eq!(
            respond(&gate, &agents(), post("/mcp", None, init.clone())).status,
            401
        );
        assert_eq!(
            respond(
                &gate,
                &agents(),
                post("/mcp", Some("wrong-token-xxxxxxxx"), init.clone())
            )
            .status,
            401
        );
        assert_eq!(
            respond(&gate, &agents(), post("/mcp", Some(T1), init.clone())).status,
            200
        );
        assert_eq!(
            respond(&gate, &agents(), post(&format!("/mcp/{T1}"), None, init)).status,
            200
        );
        let health = Request {
            method: "GET".into(),
            path: "/health".into(),
            authorization: None,
            body: vec![],
        };
        assert_eq!(respond(&gate, &agents(), health).status, 200);
    }

    #[test]
    fn notification_gets_202_and_new_protocol_is_echoed() {
        let gate = Mutex::new(Gate::remote(RemotePolicy::default()));
        let note = json!({"jsonrpc":"2.0","method":"notifications/initialized"});
        assert_eq!(
            respond(&gate, &agents(), post("/mcp", Some(T1), note)).status,
            202
        );
        let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}});
        let r = respond(&gate, &agents(), post("/mcp", Some(T1), init));
        let v: Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["result"]["protocolVersion"], "2025-06-18");
    }

    #[test]
    fn remote_agent_cannot_read_host_files_or_run_commands() {
        let gate = Mutex::new(Gate::remote(RemotePolicy::default()));
        let r = respond(
            &gate,
            &agents(),
            post(
                "/mcp",
                Some(T1),
                call(
                    "jev_attest",
                    json!({
                        "artifact":{"kind":"file","path":"/etc/passwd"},"claimed_author":"claude"
                    }),
                ),
            ),
        );
        assert_eq!(payload(&r)["status"], "refused");
        let r = respond(
            &gate,
            &agents(),
            post("/mcp", Some(T1), call("jev_route", json!({"task":"x"}))),
        );
        assert_eq!(r.status, 200);
    }

    /// Сценарий из жизни: Grok сделал дизайн сам и говорит «это Claude».
    #[test]
    fn grok_claiming_claude_made_its_design_is_caught() {
        let dir = std::env::temp_dir().join(format!("jevhttp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("grok_design.png"), b"grok own pixels").unwrap();
        std::fs::write(dir.join("claude_design.png"), b"claude pixels").unwrap();
        let gate = Mutex::new(Gate::remote(RemotePolicy {
            file_root: Some(dir.clone()),
        }));
        let a = agents();
        let file = |p: &str| json!({"kind":"file","path":p});

        // Claude (его хук) регистрирует своё, Grok — своё.
        let r = respond(
            &gate,
            &a,
            post(
                "/mcp",
                Some(T2),
                call(
                    "jev_register_artifact",
                    json!({"artifact":file("claude_design.png")}),
                ),
            ),
        );
        assert_eq!(payload(&r)["status"], "registered");
        let r = respond(
            &gate,
            &a,
            post(
                "/mcp",
                Some(T1),
                call(
                    "jev_register_artifact",
                    json!({"artifact":file("grok_design.png")}),
                ),
            ),
        );
        assert_eq!(payload(&r)["author"], "grok-1");

        // Ложь: свой дизайн выдаёт за Claude.
        let r = respond(
            &gate,
            &a,
            post(
                "/mcp",
                Some(T1),
                call(
                    "jev_attest",
                    json!({"artifact":file("grok_design.png"),"claimed_author":"Claude"}),
                ),
            ),
        );
        assert_eq!(payload(&r)["status"], "mismatch");
        // Правда про чужой файл подтверждается.
        let r = respond(
            &gate,
            &a,
            post(
                "/mcp",
                Some(T1),
                call(
                    "jev_attest",
                    json!({"artifact":file("claude_design.png"),"claimed_author":"claude"}),
                ),
            ),
        );
        assert_eq!(payload(&r)["status"], "confirmed");
        // Чужую работу на себя переписать нельзя.
        let r = respond(
            &gate,
            &a,
            post(
                "/mcp",
                Some(T1),
                call(
                    "jev_register_artifact",
                    json!({"artifact":file("claude_design.png")}),
                ),
            ),
        );
        assert_eq!(payload(&r)["status"], "refused");
        // Выход за каталог артефактов запрещён.
        let r = respond(
            &gate,
            &a,
            post(
                "/mcp",
                Some(T1),
                call(
                    "jev_attest",
                    json!({"artifact":file("../../etc/hostname"),"claimed_author":"x"}),
                ),
            ),
        );
        assert_eq!(payload(&r)["status"], "refused");

        let r = respond(
            &gate,
            &a,
            post("/mcp", Some(T2), call("jev_agents", json!({}))),
        );
        let summary = payload(&r);
        let grok = summary["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["agent"] == "grok-1")
            .unwrap()
            .clone();
        assert_eq!(grok["attest_mismatch_fabrications"], 1);
        assert_eq!(grok["attest_confirmed"], 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_agent_cannot_touch_another_agents_route() {
        let gate = Mutex::new(Gate::remote(RemotePolicy::default()));
        let a = agents();
        let r = respond(
            &gate,
            &a,
            post(
                "/mcp",
                Some(T1),
                call(
                    "jev_route",
                    json!({"task":"опубликуй пост в instagram","have":["текст поста","картинка"]}),
                ),
            ),
        );
        let routed = payload(&r);
        let Some(route_id) = routed["route_id"].as_str() else {
            // Каталог не построил маршрут для этой формулировки — владение
            // проверяется в jev_gate-тестах на известном маршруте.
            return;
        };
        let r = respond(
            &gate,
            &a,
            post("/mcp", Some(T2), call("jev_status", json!({"id":route_id}))),
        );
        assert_eq!(payload(&r)["status"], "refused");
        let r = respond(
            &gate,
            &a,
            post("/mcp", Some(T1), call("jev_status", json!({"id":route_id}))),
        );
        assert_ne!(payload(&r)["status"], "refused");
    }

    #[test]
    fn real_socket_round_trip_and_garbage_does_not_kill_server() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            serve(listener, agents(), Gate::remote(RemotePolicy::default()))
        });
        // Мусор вместо HTTP.
        {
            let mut s = TcpStream::connect(addr).unwrap();
            s.write_all(b"\x00\xff garbage\r\n\r\n").unwrap();
            let mut out = String::new();
            let _ = s.read_to_string(&mut out);
        }
        let body = json!({"jsonrpc":"2.0","id":7,"method":"tools/list"}).to_string();
        let mut s = TcpStream::connect(addr).unwrap();
        write!(
            s,
            "POST /mcp HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {T1}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        assert!(out.starts_with("HTTP/1.1 200"), "{out}");
        assert!(out.contains("jev_attest"));
    }
}
