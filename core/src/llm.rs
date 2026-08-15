//! Клиент к Go-роутеру. Ядро НЕ знает ни одного вендора LLM — только
//! localhost:порт. Смена провайдера = правка .env, перекомпиляция не нужна.

use anyhow::{anyhow, bail, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Serialize)]
pub struct Message {
    pub role: String,
    pub text: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
}

impl Message {
    pub fn system(t: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            text: t.into(),
            images: vec![],
        }
    }
    pub fn user(t: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            text: t.into(),
            images: vec![],
        }
    }
    pub fn user_with_image(t: impl Into<String>, png: &[u8]) -> Self {
        Self {
            role: "user".into(),
            text: t.into(),
            images: vec![base64::engine::general_purpose::STANDARD.encode(png)],
        }
    }
}

#[derive(Debug, Serialize)]
struct CompleteRequest<'a> {
    messages: &'a [Message],
    temperature: f64,
    max_tokens: i32,
    json_mode: bool,
    need_vision: bool,
    provider: String,
    purpose: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct CompleteResponse {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub latency_ms: i64,
    #[serde(default)]
    pub error: String,
}

#[derive(Clone)]
pub struct LlmClient {
    base: String,
    token: String,
    agent: ureq::Agent,
}

impl LlmClient {
    pub fn new(base: &str, token: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(5))
                .timeout(Duration::from_secs(180))
                .build(),
        }
    }

    pub fn health(&self) -> bool {
        self.agent
            .get(&format!("{}/health", self.base))
            .call()
            .is_ok()
    }

    pub fn complete(
        &self,
        messages: &[Message],
        purpose: &str,
        json_mode: bool,
        temperature: f64,
    ) -> Result<CompleteResponse> {
        let need_vision = messages.iter().any(|m| !m.images.is_empty());
        let req = CompleteRequest {
            messages,
            temperature,
            max_tokens: 4096,
            json_mode,
            need_vision,
            provider: String::new(),
            purpose: purpose.to_string(),
        };
        let resp = self
            .agent
            .post(&format!("{}/v1/complete", self.base))
            .set("X-Agent-Token", &self.token)
            .send_json(serde_json::to_value(&req)?);

        match resp {
            Ok(r) => {
                let out: CompleteResponse = r.into_json()?;
                if !out.error.is_empty() {
                    bail!("LLM: {}", out.error);
                }
                // Кто и за сколько ответил — видно в логе. Это единственный
                // способ понять, что основной провайдер деградировал и роутер
                // молча ушёл на резерв.
                log::info!(
                    "llm[{purpose}] {} / {} за {} мс",
                    out.provider,
                    out.model,
                    out.latency_ms
                );
                Ok(out)
            }
            Err(ureq::Error::Status(code, r)) => {
                let body: CompleteResponse = r.into_json().unwrap_or_default();
                Err(anyhow!("LLM HTTP {code}: {}", body.error))
            }
            Err(e) => Err(anyhow!("роутер недоступен: {e}")),
        }
    }
}

/// Модели любят обрамлять JSON текстом и ```-блоками. Дёргать их повторно
/// из-за этого — трата денег и секунд, поэтому вырезаем сами.
pub fn extract_json(text: &str) -> Result<serde_json::Value> {
    let t = text.trim();
    if let Ok(v) = serde_json::from_str(t) {
        return Ok(v);
    }
    let cleaned = t
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    if let Ok(v) = serde_json::from_str(cleaned) {
        return Ok(v);
    }
    let start = cleaned.find(['{', '[']).ok_or_else(|| {
        // Обрезаем: ответ модели может содержать пересказ введённых данных,
        // а ошибка уходит в лог на диске.
        let head: String = t.chars().take(200).collect();
        anyhow!("в ответе нет JSON: {head}")
    })?;
    let open = cleaned.as_bytes()[start] as char;
    let close = if open == '{' { '}' } else { ']' };
    let end = cleaned
        .rfind(close)
        .ok_or_else(|| anyhow!("незакрытый JSON"))?;
    // Ответ вида "} текст {" даёт end < start — срез такого диапазона
    // паникует и убивает поток агента, поэтому возвращаем ошибку.
    if end < start {
        bail!("в ответе нет целого JSON-объекта");
    }
    Ok(serde_json::from_str(&cleaned[start..=end])?)
}

#[cfg(test)]
mod tests {
    use super::extract_json;

    /// Мини-фаззер: агент не имеет права падать ни на одном ответе модели.
    #[test]
    fn extract_json_never_panics() {
        let cases = [
            "",
            "   ",
            "```",
            "```json```",
            "}{",
            "] тут текст [",
            "{\"a\":",
            "{{{{{{{{",
            "]]]]]]",
            "привет {ключ} мир",
            "```json\n{\"a\":1}\n```",
            "текст [1,2,3] хвост",
            "{\"кириллица\":\"да\"} и ещё } скобка",
            "\u{1f600}{}\u{1f600}",
            "{\"a\":\"\u{0}\"}",
        ];
        for c in cases {
            let _ = extract_json(c); // важно: Result, а не паника
        }
        assert!(extract_json("}{").is_err());
        assert!(extract_json("текст [1,2,3] хвост").is_ok());
    }

    #[test]
    fn extract_json_handles_long_garbage() {
        let long = "ц".repeat(100_000) + "{\"a\":1}";
        assert!(extract_json(&long).is_ok());
        assert!(extract_json(&"{".repeat(50_000)).is_err());
    }
}
