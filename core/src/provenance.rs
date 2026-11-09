//! Происхождение артефактов: кто на самом деле сделал файл/картинку.
//!
//! Зачем. Облачный агент принёс дизайн и сказал «это сделал Claude», а
//! сделал сам. Слова агента об авторстве — та же выдумка, что и «на экране
//! видно…». Решение «чьё это» принимает только реестр шлюза:
//!
//!   * автор записи — это **личность вызывающего**, которую шлюз взял из
//!     его токена, а не из аргументов. Агент может зарегистрировать работу
//!     только на себя; записать её на Claude он физически не может;
//!   * артефакт опознаётся по SHA-256 содержимого, который считает шлюз
//!     (или производитель через свой хук — тогда запись помечена `declared`);
//!   * автор — тот, кто зарегистрировал хэш **первым**. Повторная
//!     регистрация того же хэша другим агентом — копия, а не авторство;
//!   * `attest` сверяет заявленного автора с реестром детерминированно:
//!     `confirmed` / `mismatch` (выдумка, засчитывается агенту) / `unproven`.
//!
//! Честный предел: перекодированная или изменённая картинка — это другой
//! хэш, она будет `unproven`. Это строгость, а не ошибка.

use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::PathBuf;

const EVENTS_CAP: usize = 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub sha256: String,
    pub bytes: u64,
    pub author: String,
    pub locator: String,
    pub at: String,
    /// `observed` — хэш посчитал шлюз; `declared` — хэш прислал сам автор.
    pub source: String,
}

impl Record {
    pub fn json(&self) -> Value {
        json!({
            "sha256": self.sha256,
            "bytes": self.bytes,
            "author": self.author,
            "locator": self.locator,
            "at": self.at,
            "source": self.source,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Confirmed(Record),
    /// Заявлен один автор, а реестр знает другого — это выдумка.
    Mismatch {
        claimed: String,
        author: Record,
    },
    /// Хэша нет в реестре: авторство не доказано.
    Unproven,
}

#[derive(Debug, Default, Clone)]
struct Tally {
    calls: u64,
    refusals: u64,
    registered: u64,
    confirmed: u64,
    mismatches: u64,
    unproven: u64,
}

#[derive(Debug, Default)]
pub struct Registry {
    by_sha: HashMap<String, Vec<Record>>,
    tallies: BTreeMap<String, Tally>,
    events: Vec<Value>,
    file: Option<PathBuf>,
}

pub fn normalize(name: &str) -> String {
    name.trim().to_lowercase()
}

pub fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

impl Registry {
    /// Реестр с журналом на диске (`PCAGENT_PROVENANCE_FILE`, JSONL), чтобы
    /// авторство переживало перезапуск шлюза.
    pub fn from_env() -> Self {
        match std::env::var("PCAGENT_PROVENANCE_FILE") {
            Ok(path) if !path.trim().is_empty() => Self::with_file(PathBuf::from(path.trim())),
            _ => Self::default(),
        }
    }

    pub fn with_file(path: PathBuf) -> Self {
        let mut registry = Self::default();
        if let Ok(text) = std::fs::read_to_string(&path) {
            for line in text.lines() {
                let Ok(value) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                let field = |k: &str| value.get(k).and_then(Value::as_str).unwrap_or_default();
                if !valid_sha256(field("sha256")) || field("author").is_empty() {
                    continue;
                }
                registry.insert(Record {
                    sha256: field("sha256").to_lowercase(),
                    bytes: value.get("bytes").and_then(Value::as_u64).unwrap_or(0),
                    author: normalize(field("author")),
                    locator: field("locator").to_string(),
                    at: field("at").to_string(),
                    source: field("source").to_string(),
                });
            }
        }
        registry.file = Some(path);
        registry
    }

    fn insert(&mut self, record: Record) -> bool {
        let list = self.by_sha.entry(record.sha256.clone()).or_default();
        if list.iter().any(|r| r.author == record.author) {
            return false;
        }
        list.push(record);
        true
    }

    fn tally(&mut self, caller: &str) -> &mut Tally {
        self.tallies.entry(normalize(caller)).or_default()
    }

    fn event(&mut self, value: Value) {
        if self.events.len() >= EVENTS_CAP {
            self.events.remove(0);
        }
        self.events.push(value);
    }

    /// Учесть вызов инструмента агентом (для сводки `jev_agents`).
    pub fn note_call(&mut self, caller: &str, refused: bool) {
        let tally = self.tally(caller);
        tally.calls += 1;
        if refused {
            tally.refusals += 1;
        }
    }

    /// Зарегистрировать артефакт на вызывающего. Автор = `caller`, никогда
    /// не аргумент. Возвращает автора хэша (первого регистратора) — если это
    /// не `caller`, значит он принёс чужую работу.
    pub fn register(
        &mut self,
        caller: &str,
        sha256: &str,
        bytes: u64,
        locator: &str,
        source: &str,
    ) -> Record {
        let record = Record {
            sha256: sha256.to_lowercase(),
            bytes,
            author: normalize(caller),
            locator: locator.to_string(),
            at: chrono::Utc::now().to_rfc3339(),
            source: source.to_string(),
        };
        if self.insert(record.clone()) {
            self.tally(caller).registered += 1;
            if let Some(path) = &self.file {
                // Ошибка записи журнала не роняет шлюз: реестр в памяти
                // остаётся верным, а сбой виден в событиях.
                let written = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .and_then(|mut f| writeln!(f, "{}", record.json()));
                if let Err(error) = written {
                    self.event(json!({"event":"journal_error","detail":error.to_string()}));
                }
            }
            self.event(json!({"event":"registered","record":record.json()}));
        }
        self.author_of(&record.sha256).cloned().unwrap_or(record)
    }

    pub fn author_of(&self, sha256: &str) -> Option<&Record> {
        self.by_sha
            .get(&sha256.to_lowercase())
            .and_then(|list| list.first())
    }

    /// Сверить заявленного автора с реестром. Решение — сравнение строк по
    /// хэшу, без модели.
    pub fn attest(&mut self, caller: &str, claimed: &str, sha256: &str, locator: &str) -> Verdict {
        let claimed = normalize(claimed);
        let verdict = match self.author_of(sha256) {
            None => Verdict::Unproven,
            Some(record) if record.author == claimed => Verdict::Confirmed(record.clone()),
            Some(record) => Verdict::Mismatch {
                claimed: claimed.clone(),
                author: record.clone(),
            },
        };
        let (code, author) = match &verdict {
            Verdict::Confirmed(r) => {
                self.tally(caller).confirmed += 1;
                ("confirmed", Some(r.author.clone()))
            }
            Verdict::Mismatch { author, .. } => {
                self.tally(caller).mismatches += 1;
                ("mismatch", Some(author.author.clone()))
            }
            Verdict::Unproven => {
                self.tally(caller).unproven += 1;
                ("unproven", None)
            }
        };
        self.event(json!({
            "event":"attest",
            "at": chrono::Utc::now().to_rfc3339(),
            "caller": normalize(caller),
            "claimed_author": claimed,
            "verdict": code,
            "registered_author": author,
            "sha256": sha256.to_lowercase(),
            "locator": locator,
        }));
        verdict
    }

    /// Сводка по агентам: сколько вызовов, отказов и сколько раз агент
    /// приписал работу не тому автору.
    pub fn agents_json(&self) -> Value {
        let agents: Vec<Value> = self
            .tallies
            .iter()
            .map(|(name, t)| {
                json!({
                    "agent": name,
                    "calls": t.calls,
                    "refusals": t.refusals,
                    "registered_artifacts": t.registered,
                    "attest_confirmed": t.confirmed,
                    "attest_mismatch_fabrications": t.mismatches,
                    "attest_unproven": t.unproven,
                })
            })
            .collect();
        let recent: Vec<Value> = self.events.iter().rev().take(50).cloned().collect();
        json!({"agents": agents, "recent_events": recent})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const SHA2: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn claiming_someone_elses_authorship_is_a_mismatch() {
        let mut reg = Registry::default();
        reg.register("grok-1", SHA, 10, "design.png", "observed");
        // Grok сделал сам, а говорит «Claude».
        let verdict = reg.attest("grok-1", "Claude", SHA, "design.png");
        assert!(
            matches!(verdict, Verdict::Mismatch { ref author, .. } if author.author == "grok-1")
        );
        let agents = reg.agents_json();
        assert_eq!(agents["agents"][0]["attest_mismatch_fabrications"], 1);
    }

    #[test]
    fn true_authorship_is_confirmed_and_unknown_is_unproven() {
        let mut reg = Registry::default();
        reg.register("claude", SHA, 10, "a.png", "declared");
        assert!(matches!(
            reg.attest("grok-1", " CLAUDE ", SHA, "a.png"),
            Verdict::Confirmed(_)
        ));
        assert_eq!(
            reg.attest("grok-1", "claude", SHA2, "b.png"),
            Verdict::Unproven
        );
    }

    #[test]
    fn re_registering_a_copy_does_not_steal_authorship() {
        let mut reg = Registry::default();
        reg.register("claude", SHA, 10, "a.png", "declared");
        let author = reg.register("grok-1", SHA, 10, "copy.png", "observed");
        assert_eq!(author.author, "claude");
        assert!(matches!(
            reg.attest("grok-1", "grok-1", SHA, "copy.png"),
            Verdict::Mismatch { .. }
        ));
    }

    #[test]
    fn registry_survives_restart_through_journal() {
        let dir = std::env::temp_dir().join(format!("prov-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("prov.jsonl");
        let _ = std::fs::remove_file(&path);
        {
            let mut reg = Registry::with_file(path.clone());
            reg.register("claude", SHA, 10, "a.png", "declared");
        }
        let mut reg = Registry::with_file(path.clone());
        assert_eq!(reg.author_of(SHA).unwrap().author, "claude");
        assert!(matches!(
            reg.attest("x", "claude", SHA, ""),
            Verdict::Confirmed(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
