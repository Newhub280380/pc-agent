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
//!
//! Проверка авторства без участия бота (бот, который врёт, `jev_attest` не
//! вызовет):
//!
//!   * **прокси к API** (`--upstream https://api.x.ai`): бот ходит в
//!     `/v1/chat/completions`, `/v1/responses`, `/v1/images/generations`
//!     шлюза со своим токеном агента вместо ключа xAI. Сгенерированные
//!     картинки шлюз сам регистрирует на этого бота, а в каждом ответе сам
//!     ищет «это сделал Claude» и дописывает вердикт в текст ответа;
//!   * **`/check/<токен>`** — страница и API: вставить сообщение бота и файл,
//!     получить вердикт (для ботов на grok.com, где прокси не поставить).

use crate::jev_gate::Gate;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const HEADER_CAP: usize = 16 * 1024;
const BODY_CAP: usize = 1024 * 1024;
/// Для `/check` и прокси: там картинки в base64.
const BIG_BODY_CAP: usize = 48 * 1024 * 1024;
const FETCH_CAP: usize = 32 * 1024 * 1024;

/// Куда прокси пересылает запросы ботов и с каким ключом.
#[derive(Debug, Clone)]
pub struct Upstream {
    pub base: String,
    pub key: Option<String>,
}

/// Скачать не больше `cap` байт (картинки для хэшей).
pub fn fetch_capped(url: &str, cap: usize) -> Result<Vec<u8>, String> {
    let response = ureq::get(url)
        .timeout(Duration::from_secs(30))
        .call()
        .map_err(|e| format!("{url}: {e}"))?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(cap as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{url}: {e}"))?;
    if bytes.len() > cap {
        return Err(format!("{url}: больше {cap} байт"));
    }
    Ok(bytes)
}
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
#[cfg(test)]
pub fn respond(gate: &Mutex<Gate>, agents: &Agents, req: Request) -> Response {
    respond_with(gate, agents, None, req)
}

fn bearer(req: &Request) -> Option<&str> {
    req.authorization
        .as_deref()
        .and_then(|h| h.strip_prefix("Bearer ").or_else(|| h.strip_prefix("bearer ")))
        .map(str::trim)
}

pub fn respond_with(
    gate: &Mutex<Gate>,
    agents: &Agents,
    upstream: Option<&Upstream>,
    req: Request,
) -> Response {
    let path = req.path.split('?').next().unwrap_or_default().to_string();
    if req.method == "GET" && path == "/health" {
        return Response::text(200, "ok");
    }
    let (kind, path_token) = if path == "/mcp" {
        ("mcp", None)
    } else if let Some(t) = path.strip_prefix("/mcp/").filter(|t| !t.is_empty()) {
        ("mcp", Some(t))
    } else if path == "/check" {
        ("check", None)
    } else if let Some(t) = path.strip_prefix("/check/").filter(|t| !t.is_empty()) {
        ("check", Some(t))
    } else if matches!(
        path.as_str(),
        "/v1/chat/completions" | "/v1/responses" | "/v1/images/generations"
    ) {
        ("proxy", None)
    } else {
        return Response::text(404, "not found");
    };
    let Some(caller) = bearer(&req)
        .and_then(|t| agents.who(t))
        .or_else(|| path_token.and_then(|t| agents.who(t)))
        .map(str::to_string)
    else {
        return Response::text(401, "unauthorized: нужен токен агента");
    };
    if kind == "check" && req.method == "GET" {
        return Response {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: CHECK_PAGE.to_string(),
        };
    }
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
    match kind {
        "check" => return check(gate, &caller, parsed),
        "proxy" => {
            let Some(upstream) = upstream else {
                return Response::text(503, "прокси не настроен: запустите шлюз с --upstream");
            };
            return proxy(gate, &caller, upstream, &path, parsed);
        }
        _ => {}
    }
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

/// `POST /check`: тот же `jev_check_report`, ответ — его JSON без обёртки MCP.
fn check(gate: &Mutex<Gate>, caller: &str, args: Value) -> Response {
    let call = json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
        "params":{"name":"jev_check_report","arguments":args}});
    let mut gate = gate.lock().unwrap_or_else(|p| p.into_inner());
    let reply = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| gate.handle_as(caller, call)))
        .ok()
        .flatten();
    let Some(text) = reply
        .as_ref()
        .and_then(|r| r.pointer("/result/content/0/text"))
        .and_then(Value::as_str)
    else {
        return Response::text(500, "internal error");
    };
    let value: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    let status = if value["status"] == "refused" { 400 } else { 200 };
    Response::json(status, value)
}

fn upstream_post(upstream: &Upstream, path: &str, body: &Value) -> (u16, String) {
    let Some(key) = &upstream.key else {
        return (503, "прокси: нет ключа API (PCAGENT_UPSTREAM_KEY / XAI_API_KEY)".into());
    };
    let url = format!("{}{}", upstream.base, path);
    let result = ureq::post(&url)
        .timeout(Duration::from_secs(600))
        .set("Content-Type", "application/json")
        .set("Authorization", &format!("Bearer {key}"))
        .send_string(&body.to_string());
    match result {
        Ok(r) => (r.status(), r.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap_or_default()),
        Err(e) => (502, json!({"error":{"message":format!("upstream: {e}")}}).to_string()),
    }
}

/// Байты по ссылке из ответа: data-URI или URL (с кэшем по реестру).
fn seen_from_ref(gate: &Mutex<Gate>, reference: &str) -> Option<crate::provenance::Seen> {
    use sha2::Digest;
    if !reference.starts_with("data:") {
        let gate = gate.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(r) = gate.provenance_ref().by_locator(reference) {
            return Some(crate::provenance::Seen {
                locator: reference.to_string(),
                sha256: r.sha256.clone(),
                dhash: r.dhash,
            });
        }
    }
    let bytes = if reference.starts_with("data:") {
        crate::claims::decode_data_uri(reference)?
    } else {
        fetch_capped(reference, FETCH_CAP).ok()?
    };
    let locator = if reference.starts_with("data:") {
        "inline-image".to_string()
    } else {
        reference.to_string()
    };
    Some(crate::provenance::Seen {
        locator,
        sha256: crate::witness::hex(&sha2::Sha256::digest(&bytes)),
        dhash: crate::provenance::dhash(&bytes),
    })
}

/// Текст контента сообщения chat/responses (строка или массив частей).
fn content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Картинки последнего сообщения пользователя: «кто сделал этот дизайн?».
fn last_user_images(request: &Value) -> Vec<String> {
    let messages = request
        .get("messages")
        .or_else(|| request.get("input"))
        .and_then(Value::as_array);
    let Some(last) = messages.and_then(|m| m.iter().rev().find(|m| m["role"] == "user")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for part in last["content"].as_array().into_iter().flatten() {
        let url = part
            .pointer("/image_url/url")
            .or_else(|| part.get("image_url"))
            .and_then(Value::as_str);
        if let Some(url) = url {
            out.push(url.to_string());
        }
    }
    out.extend(crate::claims::artifact_refs(&content_text(&last["content"])));
    out
}

fn proxy(gate: &Mutex<Gate>, caller: &str, upstream: &Upstream, path: &str, mut request: Value) -> Response {
    let stream = request.get("stream") == Some(&Value::Bool(true));
    if stream && path == "/v1/responses" {
        return Response::text(400, "прокси Jev: stream для /v1/responses не поддерживается, используйте stream=false или /v1/chat/completions");
    }
    if let Some(obj) = request.as_object_mut() {
        // Ответ нужен целиком, чтобы проверить его до того, как он уйдёт.
        obj.remove("stream");
        obj.remove("stream_options");
    }
    let (status, text) = upstream_post(upstream, path, &request);
    let Ok(mut out) = serde_json::from_str::<Value>(&text) else {
        return Response { status, content_type: "application/json", body: text };
    };
    if status != 200 {
        return Response::json(status, out);
    }
    if path == "/v1/images/generations" {
        register_generated(gate, caller, &mut out);
        return Response::json(200, out);
    }
    // Текст ответа бота.
    let reply = if path == "/v1/chat/completions" {
        content_text(&out["choices"][0]["message"]["content"])
    } else {
        out["output"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|o| o.get("content"))
            .map(content_text)
            .collect::<Vec<_>>()
            .join("\n")
    };
    let mut refs = crate::claims::artifact_refs(&reply);
    for r in last_user_images(&request) {
        if !refs.contains(&r) {
            refs.push(r);
        }
    }
    let seen: Vec<_> = refs.iter().filter_map(|r| seen_from_ref(gate, r)).collect();
    let report = {
        let mut gate = gate.lock().unwrap_or_else(|p| p.into_inner());
        gate.provenance_mut().check_report(caller, &reply, &seen)
    };
    let notice = report["notice"].as_str().unwrap_or_default().to_string();
    if !notice.is_empty() {
        append_notice(&mut out, path, &notice);
    }
    out["jev"] = report;
    if stream {
        return sse_from_completion(&out);
    }
    Response::json(200, out)
}

/// Дописать вердикт шлюза в текст ответа — пользователь видит его в чате,
/// бот убрать его не может.
fn append_notice(out: &mut Value, path: &str, notice: &str) {
    let add = |slot: &mut Value| {
        let text = slot.as_str().unwrap_or_default();
        *slot = Value::String(format!("{text}\n\n{notice}"));
    };
    if path == "/v1/chat/completions" {
        let slot = &mut out["choices"][0]["message"]["content"];
        if let Some(parts) = slot.as_array_mut() {
            parts.push(json!({"type":"text","text":format!("\n\n{notice}")}));
        } else {
            add(slot);
        }
        return;
    }
    let last_text = out["output"].as_array_mut().and_then(|items| {
        items
            .iter_mut()
            .rev()
            .filter_map(|i| i.get_mut("content").and_then(Value::as_array_mut))
            .flat_map(|c| c.iter_mut().rev())
            .find(|p| p["type"] == "output_text")
    });
    if let Some(part) = last_text {
        add(&mut part["text"]);
    }
    if out.get("output_text").is_some() {
        add(&mut out["output_text"]);
    }
}

fn sse_from_completion(out: &Value) -> Response {
    let choice = &out["choices"][0];
    let chunk = json!({
        "id": out["id"], "object": "chat.completion.chunk", "created": out["created"],
        "model": out["model"],
        "choices": [{"index": 0, "delta": choice["message"], "finish_reason": choice["finish_reason"]}],
        "jev": out["jev"],
    });
    Response {
        status: 200,
        content_type: "text/event-stream",
        body: format!("data: {chunk}\n\ndata: [DONE]\n\n"),
    }
}

/// Картинки, которые бот сгенерировал через API, — на этого бота. Его
/// согласия не нужно: он их не регистрирует, их регистрирует шлюз.
fn register_generated(gate: &Mutex<Gate>, caller: &str, out: &mut Value) {
    use base64::Engine;
    use sha2::Digest;
    let items = out["data"].as_array().cloned().unwrap_or_default();
    let mut registered = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let (bytes, locator) = if let Some(b64) = item.get("b64_json").and_then(Value::as_str) {
            let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) else {
                continue;
            };
            (bytes, format!("images/generations#{i}"))
        } else if let Some(url) = item.get("url").and_then(Value::as_str) {
            let Ok(bytes) = fetch_capped(url, FETCH_CAP) else { continue };
            (bytes, url.to_string())
        } else {
            continue;
        };
        let sha = crate::witness::hex(&sha2::Sha256::digest(&bytes));
        let dhash = crate::provenance::dhash(&bytes);
        let mut gate = gate.lock().unwrap_or_else(|p| p.into_inner());
        let record =
            gate.provenance_mut()
                .register(caller, &sha, bytes.len() as u64, &locator, "proxied", dhash);
        registered.push(json!({"sha256": sha, "author": record.author}));
    }
    out["jev"] = json!({"registered": registered});
}

const CHECK_PAGE: &str = include_str!("check_page.html");

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
    let cap = if path.starts_with("/check") || path.starts_with("/v1/") {
        BIG_BODY_CAP
    } else {
        BODY_CAP
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
        read_chunked(&mut reader, cap)?
    } else {
        if length > cap {
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

fn read_chunked(reader: &mut impl BufRead, cap: usize) -> Result<Vec<u8>, (u16, &'static str)> {
    let mut body = Vec::new();
    loop {
        let mut size = String::new();
        reader
            .take(64)
            .read_line(&mut size)
            .map_err(|_| (400, "bad chunk"))?;
        let size = usize::from_str_radix(size.trim().split(';').next().unwrap_or(""), 16)
            .map_err(|_| (400, "bad chunk"))?;
        if body.len() + size > cap {
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
        502 => "Bad Gateway",
        503 => "Service Unavailable",
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

fn serve_connection(
    mut stream: TcpStream,
    gate: &Mutex<Gate>,
    agents: &Agents,
    upstream: Option<&Upstream>,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(15)));
    let response = match read_request(&mut stream) {
        Ok(req) => respond_with(gate, agents, upstream, req),
        Err((status, msg)) => Response::text(status, msg),
    };
    write_response(&mut stream, &response);
}

/// Принимать соединения на `listener`; поток на соединение, общий шлюз.
pub fn serve(
    listener: TcpListener,
    agents: Agents,
    mut gate: Gate,
    upstream: Option<Upstream>,
) -> anyhow::Result<()> {
    gate.provenance_mut()
        .set_known(agents.names().into_iter().map(str::to_string));
    let gate = Arc::new(Mutex::new(gate));
    let agents = Arc::new(agents);
    let upstream = Arc::new(upstream);
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let (gate, agents, upstream) = (gate.clone(), agents.clone(), upstream.clone());
        std::thread::spawn(move || serve_connection(stream, &gate, &agents, upstream.as_ref().as_ref()));
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
            serve(listener, agents(), Gate::remote(RemotePolicy::default()), None)
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

    fn png(width: u32, shift: u8) -> Vec<u8> {
        let img = image::RgbImage::from_fn(width, width * 3 / 4, |x, y| {
            let v = ((x * 255 / width) as u8).wrapping_add(shift);
            image::Rgb([v, (y % 5) as u8 * 30, 255 - v])
        });
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    /// Поддельный xAI: /v1/images/generations отдаёт картинку b64, chat —
    /// заданный текст, /img.png — пересохранённую (уменьшенную) картинку.
    fn fake_upstream(reply: String, original: Vec<u8>, resaved: Vec<u8>) -> String {
        use base64::Engine;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let Ok(req) = read_request(&mut stream) else { continue };
                if req.path.starts_with("/v1/") {
                    // Ключ xAI подставляет шлюз; токен агента наружу не уходит.
                    assert_eq!(req.authorization.as_deref(), Some("Bearer xai-secret"));
                }
                let (ct, body): (&str, Vec<u8>) = match req.path.as_str() {
                    "/v1/images/generations" => ("application/json", json!({"data":[{"b64_json":
                        base64::engine::general_purpose::STANDARD.encode(&original)}]}).to_string().into_bytes()),
                    "/v1/chat/completions" => ("application/json", json!({"id":"c1","created":1,"model":"grok",
                        "choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":reply}}]}).to_string().into_bytes()),
                    _ => ("image/png", resaved.clone()),
                };
                let head = format!("HTTP/1.1 200 OK\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        base
    }

    #[test]
    fn proxy_catches_bot_lying_about_claude_without_bot_calling_any_tool() {
        // Ответ бота ссылается на картинку на фейковом сервере; адрес узнаем
        // после старта, поэтому текст собираем в два шага.
        let original = png(320, 0);
        let resaved = png(200, 2);
        let placeholder = fake_upstream(String::new(), original.clone(), resaved.clone());
        let reply = format!("Готово! Этот дизайн сделал Claude: {placeholder}/img.png");
        let base = fake_upstream(reply, original, resaved);
        let up = Upstream { base, key: Some("xai-secret".into()) };
        let gate = Mutex::new(Gate::remote(RemotePolicy::default()));
        gate.lock().unwrap().provenance_mut().set_covered(["claude".to_string()]);
        let a = agents();
        // 1. Бот генерирует картинку через прокси — шлюз сам пишет её на бота.
        let r = respond_with(&gate, &a, Some(&up), post("/v1/images/generations", Some(T1), json!({"prompt":"баннер"})));
        assert_eq!(r.status, 200, "{}", r.body);
        let v: Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["jev"]["registered"][0]["author"], "grok-1");
        // 2. Бот отвечает «это сделал Claude» со ссылкой на пересохранённую копию.
        let r = respond_with(&gate, &a, Some(&up), post("/v1/chat/completions", Some(T1),
            json!({"model":"grok","messages":[{"role":"user","content":"кто сделал дизайн?"}]})));
        assert_eq!(r.status, 200, "{}", r.body);
        let v: Value = serde_json::from_str(&r.body).unwrap();
        let text = v["choices"][0]["message"]["content"].as_str().unwrap();
        assert!(text.contains("[Jev] grok-1: ВЫДУМКА"), "{text}");
        assert_eq!(v["jev"]["status"], "fabrication");
        // 3. Стрим отдаётся одним SSE-чанком с той же припиской.
        let r = respond_with(&gate, &a, Some(&up), post("/v1/chat/completions", Some(T1),
            json!({"model":"grok","stream":true,"messages":[{"role":"user","content":"ещё раз"}]})));
        assert_eq!(r.content_type, "text/event-stream");
        assert!(r.body.contains("ВЫДУМКА") && r.body.ends_with("data: [DONE]\n\n"));
        // 4. Без токена агента прокси закрыт, без --upstream — 503.
        assert_eq!(respond_with(&gate, &a, Some(&up), post("/v1/chat/completions", None, json!({}))).status, 401);
        assert_eq!(respond(&gate, &a, post("/v1/chat/completions", Some(T1), json!({}))).status, 503);
    }

    #[test]
    fn check_page_and_api_judge_a_pasted_report() {
        use base64::Engine;
        let gate = Mutex::new(Gate::remote(RemotePolicy::default()));
        gate.lock().unwrap().provenance_mut().set_covered(["claude".to_string()]);
        let a = Agents::parse(&format!("grok-1 {T1}\nclaude {T2}\nadmin admin-token-cccccccccc\n")).unwrap();
        gate.lock().unwrap().provenance_mut().set_known(a.names().into_iter().map(str::to_string));
        let page = respond(&gate, &a, Request { method: "GET".into(), path: "/check/admin-token-cccccccccc".into(), authorization: None, body: vec![] });
        assert_eq!(page.status, 200);
        assert!(page.body.contains("Проверка авторства"));
        assert_eq!(respond(&gate, &a, Request { method: "GET".into(), path: "/check/nope".into(), authorization: None, body: vec![] }).status, 401);
        let design = base64::engine::general_purpose::STANDARD.encode(png(240, 9));
        let body = json!({"text":"Дизайн сделал Claude, принимай.","agent":"grok-1",
            "attachments":[{"name":"kolhoz.png","base64":design}]});
        let r = respond(&gate, &a, post("/check/admin-token-cccccccccc", None, body.clone()));
        let v: Value = serde_json::from_str(&r.body).unwrap();
        // Claude регистрирует всё через хук, а этого файла у него нет.
        assert_eq!(v["status"], "fabrication", "{v}");
        assert_eq!(v["checks"][0]["verdict"], "not_produced");
        // Не-admin не может выдать свой текст за отчёт другого бота.
        let r = respond(&gate, &a, post("/check", Some(T2), body));
        assert_eq!(r.status, 400);
    }
}
