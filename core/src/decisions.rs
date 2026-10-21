//! Быстрые решения JEV через OpenRouter Decisions API.
//!
//! `POST https://openrouter.ai/api/alpha/decisions`, модель `typesafe/jev-1.13`.
//! `noul` — да/нет с вероятностью «да». `choice` — выбор одного варианта.
//! Ключ читается из `OPENROUTER_API_KEY` и ни в лог, ни в Display, ни в текст
//! ошибки не попадает. Нет ключа или сеть недоступна — это не сбой задачи:
//! вызывающий переходит на локальную статистику.
//!
//! Одинаковый запрос (модель, состояние, вопросы) берётся из кэша.
//! Ключ кэша — канонический JSON, поэтому порядок полей на ответ не влияет.

use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

pub const ENDPOINT: &str = "https://openrouter.ai/api/alpha/decisions";
pub const MODEL: &str = "typesafe/jev-1.13";

/// Ниже этого `noul` действие не делаем. Совпадает с порогом локального JEV.
pub const YES_AT: f64 = 0.6;

#[derive(Clone)]
struct Secret(String);

impl Secret {
    fn bearer(&self) -> String {
        format!("Bearer {}", self.0)
    }

    fn scrub(&self, text: String) -> String {
        if self.0.is_empty() {
            text
        } else {
            text.replace(&self.0, "<redacted>")
        }
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OPENROUTER_API_KEY=<redacted>")
    }
}

#[derive(Debug)]
pub enum DecisionError {
    /// Ключа нет или сервис не ответил. Задачу из-за этого не останавливаем.
    Unavailable(String),
    /// Ответ пришёл, но это не то решение, которое спрашивали.
    BadResponse(String),
}

impl fmt::Display for DecisionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecisionError::Unavailable(s) | DecisionError::BadResponse(s) => f.write_str(s),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    pub id: String,
    pub confidence: f64,
}

type Transport = Box<dyn Fn(&str, &str, &Value) -> Result<Value, String> + Send>;

pub struct Client {
    endpoint: String,
    key: Option<Secret>,
    cache: BTreeMap<String, Value>,
    transport: Transport,
    hits: u64,
    misses: u64,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("endpoint", &self.endpoint)
            .field("key", &self.key)
            .field("cache_len", &self.cache.len())
            .finish()
    }
}

impl Client {
    pub fn from_env() -> Self {
        let key = std::env::var("OPENROUTER_API_KEY")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        Self::with_transport(ENDPOINT, key, Box::new(http_transport))
    }

    pub fn with_transport(endpoint: &str, key: Option<String>, transport: Transport) -> Self {
        Self {
            endpoint: endpoint.to_string(),
            key: key.map(Secret),
            cache: BTreeMap::new(),
            transport,
            hits: 0,
            misses: 0,
        }
    }

    pub fn enabled(&self) -> bool {
        self.key.is_some()
    }

    pub fn cache_hits(&self) -> u64 {
        self.hits
    }

    pub fn cache_misses(&self) -> u64 {
        self.misses
    }

    /// Вероятность «да». Одинаковый вопрос второй раз сеть не трогает.
    pub fn noul(&mut self, state: &Value, instructions: &str) -> Result<f64, DecisionError> {
        let mut criteria = Map::new();
        criteria.insert("true".into(), json!("Условие выполнено, действие уместно."));
        criteria.insert(
            "false".into(),
            json!("Условие не выполнено или действие сейчас вредно."),
        );
        let answer = self.ask(state, "q", "noul", instructions, Value::Object(criteria))?;
        match answer.get("noul").and_then(|v| v.as_f64()) {
            Some(p) => Ok(p),
            None => Err(DecisionError::BadResponse(
                "в ответе noul нет вероятности".into(),
            )),
        }
    }

    pub fn yes(&mut self, state: &Value, instructions: &str) -> Result<bool, DecisionError> {
        Ok(self.noul(state, instructions)? >= YES_AT)
    }

    /// Выбор одного варианта. `options` — id и короткое описание.
    pub fn choice(
        &mut self,
        state: &Value,
        instructions: &str,
        options: &[(String, String)],
    ) -> Result<Choice, DecisionError> {
        if options.is_empty() {
            return Err(DecisionError::BadResponse("choice без вариантов".into()));
        }
        let mut criteria = Map::new();
        for (id, desc) in options {
            criteria.insert(id.clone(), json!(desc));
        }
        let answer = self.ask(state, "q", "choice", instructions, Value::Object(criteria))?;
        let id = answer
            .get("choice")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if id.is_empty() || !options.iter().any(|(k, _)| k == &id) {
            return Err(DecisionError::BadResponse(format!(
                "choice вернул неизвестный вариант «{id}»"
            )));
        }
        let confidence = answer
            .get("confidence")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        Ok(Choice { id, confidence })
    }

    fn ask(
        &mut self,
        state: &Value,
        qid: &str,
        kind: &str,
        instructions: &str,
        criteria: Value,
    ) -> Result<Value, DecisionError> {
        let key = self
            .key
            .as_ref()
            .ok_or_else(|| DecisionError::Unavailable("OPENROUTER_API_KEY не задан".into()))?;
        let body = body_of(state, qid, kind, instructions, criteria);
        let cache_key = canonical(&body);
        if let Some(hit) = self.cache.get(&cache_key) {
            self.hits += 1;
            return Ok(hit.clone());
        }
        let endpoint = self.endpoint.clone();
        let bearer = key.bearer();
        let secret = key.clone();
        let raw = (self.transport)(&endpoint, &bearer, &body).map_err(|e| {
            DecisionError::Unavailable(secret.scrub(format!("jev недоступен: {e}")))
        })?;
        let answer = raw
            .get("answers")
            .and_then(|a| a.get(qid))
            .cloned()
            .ok_or_else(|| DecisionError::BadResponse("в ответе нет answers.q".into()))?;
        let got = answer.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if got != kind {
            return Err(DecisionError::BadResponse(format!(
                "ждали {kind}, пришло {got}"
            )));
        }
        self.cache.insert(cache_key, answer.clone());
        self.misses += 1;
        Ok(answer)
    }
}

fn body_of(state: &Value, qid: &str, kind: &str, instructions: &str, criteria: Value) -> Value {
    let mut questions = Map::new();
    questions.insert(
        qid.to_string(),
        json!({
            "type": kind,
            "instructions": instructions,
            "criteria": criteria,
        }),
    );
    json!({
        "model": MODEL,
        "state": state,
        "questions": questions,
    })
}

/// Объекты сериализуем с отсортированными ключами, чтобы тот же вопрос
/// всегда давал тот же ключ кэша.
fn canonical(value: &Value) -> String {
    fn sort(v: &Value) -> Value {
        match v {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                let mut out = Map::new();
                for k in keys {
                    out.insert(k.clone(), sort(&map[k]));
                }
                Value::Object(out)
            }
            Value::Array(items) => Value::Array(items.iter().map(sort).collect()),
            other => other.clone(),
        }
    }
    serde_json::to_string(&sort(value)).unwrap_or_else(|_| value.to_string())
}

fn http_transport(url: &str, bearer: &str, body: &Value) -> Result<Value, String> {
    let response = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(8))
        .build()
        .post(url)
        .set("Authorization", bearer)
        .set("Content-Type", "application/json")
        .send_json(body);
    match response {
        Ok(r) => r
            .into_json::<Value>()
            .map_err(|e| format!("jev: не json ({e})")),
        Err(ureq::Error::Status(code, _)) => Err(format!("jev http {code}")),
        Err(e) => Err(format!("jev сеть: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn noul_body(p: f64) -> Value {
        json!({"answers": {"q": {"type": "noul", "noul": p}}})
    }

    #[test]
    fn identical_noul_is_cached_and_key_is_not_in_debug_or_errors() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls2 = calls.clone();
        let key = "sk-test-secret-do-not-log".to_string();
        let mut client = Client::with_transport(
            "https://openrouter.ai/api/alpha/decisions",
            Some(key.clone()),
            Box::new(move |_url, bearer, _body| {
                calls2.fetch_add(1, Ordering::SeqCst);
                if bearer.contains("sk-test-secret") {
                    // Транспорт нарочно возвращает ключ в ошибке: клиент обязан его вычистить.
                    // Первый вызов успешен, порчу ответа не делаем здесь.
                }
                Ok(noul_body(0.91))
            }),
        );
        let state = json!({"action": "клик", "goal": "войти"});
        let a = client.noul(&state, "Делать ли клик?").unwrap();
        let b = client.noul(&state, "Делать ли клик?").unwrap();
        assert_eq!(a, b);
        assert!(a >= YES_AT);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "второй вызов обязан быть из кэша"
        );
        assert_eq!(client.cache_hits(), 1);
        assert_eq!(client.cache_misses(), 1);
        let debug = format!("{client:?}");
        assert!(!debug.contains(&key), "{debug}");
        assert!(debug.contains("<redacted>"));

        let calls3 = Arc::new(AtomicUsize::new(0));
        let leaked = key.clone();
        let mut broken = Client::with_transport(
            "https://example.invalid/decisions",
            Some(leaked.clone()),
            Box::new(move |_u, _b, _body| {
                calls3.fetch_add(1, Ordering::SeqCst);
                Err(format!("unauthorized {leaked}"))
            }),
        );
        let err = broken.noul(&state, "ещё вопрос").unwrap_err();
        let text = err.to_string();
        assert!(!text.contains("sk-test-secret"), "{text}");
        assert!(text.contains("<redacted>"), "{text}");
    }

    #[test]
    fn choice_is_deterministic_for_the_same_options() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls2 = calls.clone();
        let mut client = Client::with_transport(
            "http://jev.local",
            Some("k".into()),
            Box::new(move |_u, _b, _body| {
                calls2.fetch_add(1, Ordering::SeqCst);
                Ok(json!({"answers": {"q": {
                    "type": "choice",
                    "choice": "disk",
                    "confidence": 0.8,
                    "probabilities": {"disk": 0.8, "dl": 0.2}
                }}}))
            }),
        );
        let state = json!({"task": "установить"});
        let opts = vec![
            ("dl".into(), "скачать".into()),
            ("disk".into(), "проверить диск".into()),
        ];
        let mut reversed = opts.clone();
        reversed.reverse();
        let a = client.choice(&state, "какой узел?", &opts).unwrap();
        let b = client.choice(&state, "какой узел?", &reversed).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.id, "disk");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn missing_key_degrades() {
        let mut client = Client::with_transport(
            "http://jev.local",
            None,
            Box::new(|_, _, _| {
                panic!("без ключа сеть вызываться не должна");
            }),
        );
        let err = client.yes(&json!({}), "делать?").unwrap_err();
        assert!(matches!(err, DecisionError::Unavailable(_)));
    }
}
