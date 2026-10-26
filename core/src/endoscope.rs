//! Эндоскоп: независимая проверка того, что задача действительно сделана.
//!
//! Зачем: отчёт шага — это слова самого агента. Он может ошибиться или
//! написать «готово» по инерции, и тогда задача закрывается ложью. Эндоскоп
//! — отдельный процесс, который сам пишет, что происходило (прокси, дозор
//! каталогов, дерево процессов), и сверяет это с текстом отчёта. Агент на
//! его запись не влияет: он только отдаёт отчёт и получает приговор.
//!
//! Правило простое: закрыть задачу можно только по «готово». «Не доказано»
//! — закрывать нельзя, это расхождение слов и факта. «Проверка неполная» —
//! тоже нельзя: непроверяемость не успех. Эндоскоп настроен, но не отвечает
//! — тоже нельзя, иначе достаточно погасить проверку, чтобы пройти.
//!
//! Альтернативы: держать проверку внутри ядра (как `acceptance`/`logic`) —
//! уже есть, но это самопроверка, и она не ловит «действия не было вовсе».
//! Писать свой сборщик фактов в ядре — дублирование того, что у эндоскопа
//! уже работает, поэтому берём его по HTTP.

use anyhow::{anyhow, Result};
use serde::Deserialize;
use std::time::Duration;

/// Приговор независимой проверки.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Эндоскоп не настроен: закрытие решается обычными правилами ядра.
    NotConfigured,
    /// Факт совпал с отчётом.
    Proven,
    /// Отчёт расходится с записью прогона.
    Disproven(String),
    /// Проверить нечем: заявлений нет, часть неизвестна или запись обрезана.
    Incomplete(String),
    /// Настроен, но недоступен. Это не успех.
    Unavailable(String),
}

impl Verdict {
    /// Можно ли закрывать задачу. Всё, кроме «готово» и «не настроен», —
    /// нельзя.
    pub fn closes(&self) -> bool {
        matches!(self, Verdict::Proven | Verdict::NotConfigured)
    }

    /// Причина отказа для отчёта человеку.
    pub fn why(&self) -> String {
        match self {
            Verdict::Proven | Verdict::NotConfigured => String::new(),
            Verdict::Disproven(d) => format!("эндоскоп: не доказано — {d}"),
            Verdict::Incomplete(d) => format!("эндоскоп: проверка неполная — {d}"),
            Verdict::Unavailable(d) => {
                format!("эндоскоп настроен, но проверить нечем — {d}; задача не закрыта")
            }
        }
    }
}

#[derive(Deserialize, Default)]
struct Detail {
    #[serde(default)]
    verification: Verification,
}

#[derive(Deserialize, Default)]
struct Verification {
    #[serde(default)]
    status: String,
    #[serde(default)]
    claims: Vec<Claim>,
}

/// Заявление из отчёта и приговор по нему. Поля — как их отдаёт
/// эндоскоп: `verdict` это «ПРАВДА», «СОВРАЛ», «НЕ ПОЛНОСТЬЮ» или
/// «НЕ ПРОВЕРИТЬ» (проверено на живом API).
#[derive(Deserialize, Default)]
struct Claim {
    #[serde(default)]
    text: String,
    #[serde(default)]
    verdict: String,
}

#[derive(Clone)]
pub struct Endoscope {
    base: String,
    run_id: String,
    agent: ureq::Agent,
}

impl Endoscope {
    /// Настройки из окружения: без `ENDOSCOPE_URL` эндоскопа просто нет, и
    /// ядро работает как раньше. `ENDOSCOPE_RUN_ID` — прогон, который
    /// эндоскоп завёл под эту задачу.
    pub fn from_env() -> Option<Self> {
        let base = std::env::var("ENDOSCOPE_URL").ok()?;
        let base = base.trim().trim_end_matches('/').to_string();
        let run_id = std::env::var("ENDOSCOPE_RUN_ID")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())?;
        if base.is_empty() {
            return None;
        }
        Some(Self {
            base,
            run_id,
            // Проверка не должна висеть дольше сторожа по зависанию.
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(3))
                .timeout(Duration::from_secs(15))
                .build(),
        })
    }

    /// Отдать отчёт и получить приговор. Сам текст приговора эндоскоп
    /// считает по своей записи прогона, а не по этому отчёту.
    pub fn judge(&self, report: &str) -> Verdict {
        match self.ask(report) {
            Ok(v) => v,
            Err(e) => Verdict::Unavailable(e.to_string()),
        }
    }

    fn ask(&self, report: &str) -> Result<Verdict> {
        // У эндоскопа тело ограничено 128 КБ, и отчёт шага туда влезает с
        // запасом; обрезаем на всякий случай, чтобы не получить 413 вместо
        // приговора.
        let body: String = report.chars().take(60_000).collect();
        let resp = self
            .agent
            .post(&format!("{}/api/reports", self.base))
            .set("Content-Type", "application/json")
            .send_json(serde_json::json!({ "run_id": self.run_id, "report": body }));
        let detail: Detail = match resp {
            Ok(r) => r.into_json()?,
            Err(ureq::Error::Status(404, _)) => {
                return Err(anyhow!("прогон {} не найден", self.run_id))
            }
            Err(ureq::Error::Status(code, _)) => return Err(anyhow!("HTTP {code}")),
            Err(e) => return Err(anyhow!("{e}")),
        };
        Ok(classify(&detail.verification))
    }
}

/// Статусы эндоскопа на русском, как он их и отдаёт. Незнакомый статус —
/// это не успех: лучше оставить задачу открытой, чем закрыть по непонятному
/// слову.
fn classify(v: &Verification) -> Verdict {
    let bad: Vec<&str> = v
        .claims
        .iter()
        .filter(|c| c.verdict != "ПРАВДА")
        .map(|c| c.text.as_str())
        .filter(|t| !t.is_empty())
        .take(5)
        .collect();
    let detail = if bad.is_empty() {
        "подробности в карточке прогона".to_string()
    } else {
        bad.join("; ")
    };
    match v.status.trim() {
        "готово" => Verdict::Proven,
        "не доказано" => Verdict::Disproven(detail),
        "проверка неполная" => Verdict::Incomplete(detail),
        other if other.is_empty() => Verdict::Incomplete("приговор пустой".to_string()),
        other => Verdict::Incomplete(format!("незнакомый статус «{other}»")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(status: &str, claims: &[(&str, &str)]) -> Verification {
        Verification {
            status: status.to_string(),
            claims: claims
                .iter()
                .map(|(t, s)| Claim {
                    text: t.to_string(),
                    verdict: s.to_string(),
                })
                .collect(),
        }
    }

    /// Закрывает задачу только «готово». Всё остальное — открытая задача, в
    /// том числе непроверяемость: «не знаю» успехом не считается.
    #[test]
    fn only_proven_closes_the_task() {
        assert!(classify(&v("готово", &[("скачал 2 файла", "ПРАВДА")])).closes());
        assert!(!classify(&v("не доказано", &[("скачал 50 файлов", "СОВРАЛ")])).closes());
        assert!(!classify(&v("проверка неполная", &[])).closes());
        assert!(!classify(&v("", &[])).closes());
        assert!(!classify(&v("всё ок наверное", &[])).closes());
        assert!(!Verdict::Unavailable("сеть".into()).closes());
        assert!(Verdict::NotConfigured.closes());
    }

    /// В отчёт человеку должно попасть, что именно не сошлось.
    #[test]
    fn refusal_names_the_unproven_claim() {
        let why = classify(&v(
            "не доказано",
            &[
                ("скачал 50 файлов", "СОВРАЛ"),
                ("открыл unsplash", "ПРАВДА"),
            ],
        ))
        .why();
        assert!(why.contains("скачал 50 файлов"), "{why}");
        assert!(!why.contains("открыл unsplash"), "{why}");
    }

    /// Живая проверка по HTTP: отчёт уходит на /api/reports, приговор
    /// читается из ответа. Поддельный эндоскоп говорит «не доказано» —
    /// задача закрыться не должна.
    #[test]
    fn http_roundtrip_reads_the_verdict() {
        use std::io::{Read, Write};
        let srv = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = srv.local_addr().unwrap().port();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let got = seen.clone();
        std::thread::spawn(move || {
            let (mut st, _) = srv.accept().unwrap();
            // Тело может прийти отдельным пакетом от заголовков.
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            while !String::from_utf8_lossy(&raw).contains("run-1") {
                let n = st.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&buf[..n]);
            }
            *got.lock().unwrap() = String::from_utf8_lossy(&raw).to_string();
            let body = r#"{"verification":{"status":"не доказано","claims":[{"text":"скачал 50 файлов","verdict":"СОВРАЛ"}]}}"#;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            st.write_all(head.as_bytes()).unwrap();
            st.write_all(body.as_bytes()).unwrap();
        });
        std::env::set_var("ENDOSCOPE_URL", format!("http://127.0.0.1:{port}/"));
        std::env::set_var("ENDOSCOPE_RUN_ID", "run-1");
        let e = Endoscope::from_env().expect("эндоскоп настроен");
        std::env::remove_var("ENDOSCOPE_URL");
        std::env::remove_var("ENDOSCOPE_RUN_ID");
        let verdict = e.judge("ИТОГ: скачал 50 файлов");
        assert_eq!(
            verdict,
            Verdict::Disproven("скачал 50 файлов".to_string()),
            "{verdict:?}"
        );
        assert!(!verdict.closes());
        let req = seen.lock().unwrap().clone();
        assert!(req.contains("POST /api/reports"), "{req}");
        assert!(req.contains("run-1"), "{req}");
    }

    /// Настроен, но не отвечает — тоже не успех: иначе достаточно погасить
    /// проверку, чтобы задача закрылась.
    #[test]
    fn dead_endoscope_does_not_close() {
        let e = Endoscope {
            base: "http://127.0.0.1:1".to_string(),
            run_id: "run-1".to_string(),
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_millis(200))
                .build(),
        };
        let verdict = e.judge("ИТОГ: сделано");
        assert!(matches!(verdict, Verdict::Unavailable(_)), "{verdict:?}");
        assert!(!verdict.closes());
    }

    /// Без настроек эндоскопа ядро работает как раньше.
    #[test]
    fn missing_config_is_not_a_blocker() {
        std::env::remove_var("ENDOSCOPE_URL");
        std::env::remove_var("ENDOSCOPE_RUN_ID");
        assert!(Endoscope::from_env().is_none());
        // Адрес без прогона — тоже не настройка: сверять нечего.
        std::env::set_var("ENDOSCOPE_URL", "http://127.0.0.1:7098");
        assert!(Endoscope::from_env().is_none());
        std::env::remove_var("ENDOSCOPE_URL");
    }
}
