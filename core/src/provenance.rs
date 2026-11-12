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
//! Две вещи, которые не зависят от честности агента:
//!
//!   * **перцептивный хэш** (dHash, 64 бита) картинок. Пережатая, уменьшенная
//!     или пересохранённая картинка — другой SHA-256, но почти тот же dHash
//!     (расстояние Хэмминга ≤ [`NEAR_MAX`]). Так узнаётся «свой дизайн,
//!     пересохранённый и выданный за чужой»;
//!   * **покрытые авторы** (`PCAGENT_COVERED_AUTHORS`, по умолчанию `claude`):
//!     у них хук регистрирует *каждый* файл. Если агент говорит «это сделал
//!     Claude», а в реестре Claude нет ни этого файла, ни похожего — это не
//!     «не доказано», а выдумка (`not_produced`): бремя доказательства на
//!     том, кто приписывает работу.

use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write;
use std::path::PathBuf;

const EVENTS_CAP: usize = 1000;
/// Порог похожести dHash (бит из 64). Пересжатие/ресайз дают 0–5.
pub const NEAR_MAX: u32 = 8;
/// Не декодируем картинки больше этого (защита от «бомб»).
const IMAGE_CAP: usize = 32 * 1024 * 1024;

/// Перцептивный хэш (difference hash 9×8) картинки; `None` — не картинка.
pub fn dhash(bytes: &[u8]) -> Option<u64> {
    if bytes.len() > IMAGE_CAP {
        return None;
    }
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(512 * 1024 * 1024);
    reader.limits(limits);
    let img = reader.decode().ok()?;
    let small = img
        .resize_exact(9, 8, image::imageops::FilterType::Triangle)
        .to_luma8();
    let mut hash = 0u64;
    for y in 0..8 {
        for x in 0..8 {
            hash <<= 1;
            if small.get_pixel(x, y)[0] > small.get_pixel(x + 1, y)[0] {
                hash |= 1;
            }
        }
    }
    Some(hash)
}

pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub sha256: String,
    pub bytes: u64,
    pub author: String,
    pub locator: String,
    pub at: String,
    /// `observed` — хэш посчитал шлюз; `declared` — хэш прислал сам автор;
    /// `proxied` — шлюз перехватил картинку в ответе API агента.
    pub source: String,
    pub dhash: Option<u64>,
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
            "dhash": self.dhash.map(|h| format!("{h:016x}")),
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
    /// Точного хэша нет, но картинка почти совпадает с работой заявленного
    /// автора (пересжатая/уменьшенная копия).
    NearConfirmed {
        record: Record,
        distance: u32,
    },
    /// Картинка почти совпадает с работой *другого* агента.
    NearMismatch {
        claimed: String,
        author: Record,
        distance: u32,
    },
    /// Заявленный автор покрыт хуком (регистрирует всё), а такого файла у
    /// него нет — выдумка.
    NotProduced {
        claimed: String,
    },
    /// Хэша нет в реестре: авторство не доказано.
    Unproven,
}

impl Verdict {
    pub fn code(&self) -> &'static str {
        match self {
            Verdict::Confirmed(_) => "confirmed",
            Verdict::NearConfirmed { .. } => "near_confirmed",
            Verdict::Mismatch { .. } => "mismatch",
            Verdict::NearMismatch { .. } => "near_mismatch",
            Verdict::NotProduced { .. } => "not_produced",
            Verdict::Unproven => "unproven",
        }
    }

    pub fn is_fabrication(&self) -> bool {
        matches!(
            self,
            Verdict::Mismatch { .. } | Verdict::NearMismatch { .. } | Verdict::NotProduced { .. }
        )
    }

    pub fn is_confirmed(&self) -> bool {
        matches!(self, Verdict::Confirmed(_) | Verdict::NearConfirmed { .. })
    }

    /// Человеческий вердикт — его бот обязан передать пользователю.
    pub fn text(&self, claimed: &str) -> String {
        match self {
            Verdict::Confirmed(r) => format!("ПОДТВЕРЖДЕНО ШЛЮЗОМ: автор {}", r.author),
            Verdict::NearConfirmed { record, distance } => format!(
                "ПОДТВЕРЖДЕНО ШЛЮЗОМ (копия): это пересохранённая работа {} (отличие dHash {distance}/64)",
                record.author
            ),
            Verdict::Mismatch { claimed, author } => format!(
                "ВЫДУМКА: заявлен автор {claimed}, а по реестру автор {}",
                author.author
            ),
            Verdict::NearMismatch {
                claimed,
                author,
                distance,
            } => format!(
                "ВЫДУМКА: заявлен автор {claimed}, а это пересохранённая работа {} (отличие dHash {distance}/64)",
                author.author
            ),
            Verdict::NotProduced { claimed } => format!(
                "ВЫДУМКА: {claimed} регистрирует все свои файлы, а этого файла (и похожего) у {claimed} нет"
            ),
            Verdict::Unproven => format!(
                "НЕ ПОДТВЕРЖДЕНО: этот файл никто не регистрировал, авторство {} не доказано",
                normalize(claimed)
            ),
        }
    }

    fn record(&self) -> Option<&Record> {
        match self {
            Verdict::Confirmed(r) => Some(r),
            Verdict::NearConfirmed { record, .. } => Some(record),
            Verdict::Mismatch { author, .. } | Verdict::NearMismatch { author, .. } => Some(author),
            _ => None,
        }
    }
}

/// Артефакт, который шлюз сам прочитал/хэшировал для проверки отчёта.
#[derive(Debug, Clone)]
pub struct Seen {
    pub locator: String,
    pub sha256: String,
    pub dhash: Option<u64>,
}

#[derive(Debug, Default, Clone)]
struct Tally {
    calls: u64,
    refusals: u64,
    registered: u64,
    confirmed: u64,
    mismatches: u64,
    unproven: u64,
    claims_seen: u64,
    unverifiable_claims: u64,
}

#[derive(Debug, Default)]
pub struct Registry {
    by_sha: HashMap<String, Vec<Record>>,
    tallies: BTreeMap<String, Tally>,
    events: Vec<Value>,
    file: Option<PathBuf>,
    /// Авторы, у которых хук регистрирует каждый файл.
    covered: BTreeSet<String>,
    /// Имена агентов, которых ищем в тексте отчётов.
    known: BTreeSet<String>,
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
        let mut registry = match std::env::var("PCAGENT_PROVENANCE_FILE") {
            Ok(path) if !path.trim().is_empty() => Self::with_file(PathBuf::from(path.trim())),
            _ => Self::default(),
        };
        let covered = std::env::var("PCAGENT_COVERED_AUTHORS").unwrap_or_else(|_| "claude".into());
        registry.set_covered(covered.split(',').map(str::to_string));
        registry
    }

    pub fn set_covered(&mut self, names: impl IntoIterator<Item = String>) {
        self.covered = names
            .into_iter()
            .map(|n| normalize(&n))
            .filter(|n| !n.is_empty())
            .collect();
        self.known.extend(self.covered.iter().cloned());
    }

    /// Имена агентов (из файла агентов), которых шлюз ищет в тексте.
    pub fn set_known(&mut self, names: impl IntoIterator<Item = String>) {
        self.known.extend(names.into_iter().map(|n| normalize(&n)));
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
                    dhash: value
                        .get("dhash")
                        .and_then(Value::as_str)
                        .and_then(|h| u64::from_str_radix(h, 16).ok()),
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
        dhash: Option<u64>,
    ) -> Record {
        self.known.insert(normalize(caller));
        let record = Record {
            sha256: sha256.to_lowercase(),
            bytes,
            author: normalize(caller),
            locator: locator.to_string(),
            at: chrono::Utc::now().to_rfc3339(),
            source: source.to_string(),
            dhash,
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

    /// Запись по месту, где артефакт был увиден (URL картинки из API).
    pub fn by_locator(&self, locator: &str) -> Option<&Record> {
        self.by_sha
            .values()
            .filter_map(|list| list.first())
            .find(|r| r.locator == locator)
    }

    /// Ближайшая по dHash запись (первый автор каждого хэша).
    fn nearest(&self, dhash: u64) -> Option<(Record, u32)> {
        self.by_sha
            .values()
            .filter_map(|list| list.first())
            .filter_map(|r| r.dhash.map(|h| (r, hamming(h, dhash))))
            .filter(|(_, d)| *d <= NEAR_MAX)
            .min_by_key(|(r, d)| (*d, r.at.clone()))
            .map(|(r, d)| (r.clone(), d))
    }

    /// Сверить заявленного автора с реестром. Решение — сравнение строк и
    /// битов хэшей, без модели.
    pub fn attest(
        &mut self,
        caller: &str,
        claimed: &str,
        sha256: &str,
        dhash: Option<u64>,
        locator: &str,
    ) -> Verdict {
        let claimed = normalize(claimed);
        let verdict = match self.author_of(sha256) {
            Some(record) if record.author == claimed => Verdict::Confirmed(record.clone()),
            Some(record) => Verdict::Mismatch {
                claimed: claimed.clone(),
                author: record.clone(),
            },
            None => match dhash.and_then(|h| self.nearest(h)) {
                Some((record, distance)) if record.author == claimed => {
                    Verdict::NearConfirmed { record, distance }
                }
                Some((author, distance)) => Verdict::NearMismatch {
                    claimed: claimed.clone(),
                    author,
                    distance,
                },
                None if self.covered.contains(&claimed) => Verdict::NotProduced {
                    claimed: claimed.clone(),
                },
                None => Verdict::Unproven,
            },
        };
        let tally = self.tally(caller);
        if verdict.is_confirmed() {
            tally.confirmed += 1;
        } else if verdict.is_fabrication() {
            tally.mismatches += 1;
        } else {
            tally.unproven += 1;
        }
        let author = verdict.record().map(|r| r.author.clone());
        self.event(json!({
            "event":"attest",
            "at": chrono::Utc::now().to_rfc3339(),
            "caller": normalize(caller),
            "claimed_author": claimed,
            "verdict": verdict.code(),
            "registered_author": author,
            "sha256": sha256.to_lowercase(),
            "locator": locator,
        }));
        verdict
    }

    /// Проверить текст агента целиком: найти утверждения об авторстве и
    /// сверить каждое с каждым артефактом, который шлюз видел сам. Агент
    /// для этого ничего не вызывает — текст приходит через прокси или
    /// вставкой пользователя.
    pub fn check_report(&mut self, reporter: &str, text: &str, seen: &[Seen]) -> Value {
        let names: Vec<String> = self.known.iter().cloned().collect();
        let claims = crate::claims::extract(text, &names);
        let reporter = normalize(reporter);
        // «Claude сделал» в тексте самого Claude — это не приписывание.
        let claims: Vec<_> = claims
            .into_iter()
            .filter(|c| c.author != reporter)
            .collect();
        self.tally(&reporter).claims_seen += claims.len() as u64;
        let mut checks = Vec::new();
        let mut fabrication = false;
        let mut confirmed = 0usize;
        for claim in &claims {
            if seen.is_empty() {
                self.tally(&reporter).unverifiable_claims += 1;
                checks.push(json!({
                    "claimed_author": claim.author,
                    "sentence": claim.sentence,
                    "verdict": "unverifiable",
                    "text": format!("НЕ ПОДТВЕРЖДЕНО: «{}» — без файла, который шлюз мог бы проверить", claim.sentence),
                }));
                continue;
            }
            for artifact in seen {
                let verdict = self.attest(
                    &reporter,
                    &claim.author,
                    &artifact.sha256,
                    artifact.dhash,
                    &artifact.locator,
                );
                fabrication |= verdict.is_fabrication();
                confirmed += verdict.is_confirmed() as usize;
                checks.push(json!({
                    "claimed_author": claim.author,
                    "sentence": claim.sentence,
                    "artifact": artifact.locator,
                    "sha256": artifact.sha256,
                    "verdict": verdict.code(),
                    "text": verdict.text(&claim.author),
                    "record": verdict.record().map(Record::json),
                }));
            }
        }
        let status = if claims.is_empty() {
            "no_claims"
        } else if fabrication {
            "fabrication"
        } else if confirmed > 0
            && checks.iter().all(|c| {
                c["verdict"]
                    .as_str()
                    .is_some_and(|v| v.contains("confirmed"))
            })
        {
            "confirmed"
        } else {
            "unproven"
        };
        let notice = match status {
            "no_claims" => String::new(),
            "fabrication" => {
                let first = checks
                    .iter()
                    .find(|c| {
                        matches!(
                            c["verdict"].as_str(),
                            Some("mismatch" | "near_mismatch" | "not_produced")
                        )
                    })
                    .and_then(|c| c["text"].as_str())
                    .unwrap_or_default();
                format!("[Jev] {reporter}: {first}")
            }
            "confirmed" => "[Jev] авторство подтверждено шлюзом".to_string(),
            _ => format!("[Jev] {reporter}: утверждение об авторстве НЕ ПОДТВЕРЖДЕНО шлюзом"),
        };
        self.event(json!({
            "event":"report_checked",
            "at": chrono::Utc::now().to_rfc3339(),
            "reporter": reporter,
            "status": status,
            "claims": claims.len(),
            "artifacts": seen.len(),
        }));
        json!({"status": status, "notice": notice, "reporter": reporter, "checks": checks})
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
                    "authorship_claims_in_text": t.claims_seen,
                    "claims_without_checkable_file": t.unverifiable_claims,
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
        reg.register("grok-1", SHA, 10, "design.png", "observed", None);
        // Grok сделал сам, а говорит «Claude».
        let verdict = reg.attest("grok-1", "Claude", SHA, None, "design.png");
        assert!(
            matches!(verdict, Verdict::Mismatch { ref author, .. } if author.author == "grok-1")
        );
        let agents = reg.agents_json();
        assert_eq!(agents["agents"][0]["attest_mismatch_fabrications"], 1);
    }

    #[test]
    fn true_authorship_is_confirmed_and_unknown_is_unproven() {
        let mut reg = Registry::default();
        reg.register("claude", SHA, 10, "a.png", "declared", None);
        assert!(matches!(
            reg.attest("grok-1", " CLAUDE ", SHA, None, "a.png"),
            Verdict::Confirmed(_)
        ));
        assert_eq!(
            reg.attest("grok-1", "claude", SHA2, None, "b.png"),
            Verdict::Unproven
        );
    }

    #[test]
    fn re_registering_a_copy_does_not_steal_authorship() {
        let mut reg = Registry::default();
        reg.register("claude", SHA, 10, "a.png", "declared", None);
        let author = reg.register("grok-1", SHA, 10, "copy.png", "observed", None);
        assert_eq!(author.author, "claude");
        assert!(matches!(
            reg.attest("grok-1", "grok-1", SHA, None, "copy.png"),
            Verdict::Mismatch { .. }
        ));
    }

    /// PNG с градиентом; `shift` слегка меняет картинку.
    fn png(width: u32, shift: u8, reverse: bool) -> Vec<u8> {
        let img = image::RgbImage::from_fn(width, width * 3 / 4, |x, y| {
            let v = ((x * 255 / width) as u8).wrapping_add(shift);
            let v = if reverse { 255 - v } else { v };
            image::Rgb([v, (y % 7) as u8 * 20, 255 - v])
        });
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    fn sha(bytes: &[u8]) -> String {
        use sha2::Digest;
        crate::witness::hex(&sha2::Sha256::digest(bytes))
    }

    #[test]
    fn resaved_own_design_passed_off_as_claudes_is_caught_by_dhash() {
        let mut reg = Registry::default();
        let original = png(320, 0, false);
        let resaved = png(160, 3, false); // уменьшен и чуть изменён
        assert_ne!(sha(&original), sha(&resaved));
        reg.register(
            "grok-1",
            &sha(&original),
            1,
            "g.png",
            "proxied",
            dhash(&original),
        );
        let v = reg.attest("grok-1", "claude", &sha(&resaved), dhash(&resaved), "x.png");
        assert!(
            matches!(v, Verdict::NearMismatch { ref author, .. } if author.author == "grok-1"),
            "{v:?}"
        );
        assert!(v.is_fabrication());
        // А совсем другая картинка не «похожа».
        let other = png(320, 0, true);
        assert_eq!(
            reg.attest("grok-1", "claude", &sha(&other), dhash(&other), "o.png"),
            Verdict::Unproven
        );
    }

    #[test]
    fn covered_author_without_such_file_means_fabrication() {
        let mut reg = Registry::default();
        reg.set_covered(["claude".to_string()]);
        let v = reg.attest("grok-1", "Claude", SHA2, None, "b.png");
        assert_eq!(
            v,
            Verdict::NotProduced {
                claimed: "claude".into()
            }
        );
        // Непокрытый автор — по-прежнему просто «не доказано».
        assert_eq!(
            reg.attest("grok-1", "grok-2", SHA2, None, "b.png"),
            Verdict::Unproven
        );
    }

    #[test]
    fn report_text_is_checked_without_the_bot_calling_anything() {
        let mut reg = Registry::default();
        reg.set_covered(["claude".to_string()]);
        reg.set_known(["grok-1".to_string()]);
        let design = png(200, 0, false);
        reg.register("grok-1", &sha(&design), 1, "img", "proxied", dhash(&design));
        let seen = [Seen {
            locator: "img".into(),
            sha256: sha(&design),
            dhash: dhash(&design),
        }];
        let out = reg.check_report("grok-1", "Готово! Дизайн сделал Claude.", &seen);
        assert_eq!(out["status"], "fabrication", "{out}");
        assert!(out["notice"].as_str().unwrap().contains("ВЫДУМКА"));
        let out = reg.check_report("grok-1", "Дизайн сделал Claude.", &[]);
        assert_eq!(out["status"], "unproven");
        assert_eq!(
            reg.check_report("grok-1", "Вот дизайн.", &seen)["status"],
            "no_claims"
        );
        let a = reg.agents_json();
        assert_eq!(a["agents"][0]["attest_mismatch_fabrications"], 1);
        assert_eq!(a["agents"][0]["claims_without_checkable_file"], 1);
    }

    #[test]
    fn registry_survives_restart_through_journal() {
        let dir = std::env::temp_dir().join(format!("prov-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("prov.jsonl");
        let _ = std::fs::remove_file(&path);
        {
            let mut reg = Registry::with_file(path.clone());
            reg.register("claude", SHA, 10, "a.png", "declared", None);
        }
        let mut reg = Registry::with_file(path.clone());
        assert_eq!(reg.author_of(SHA).unwrap().author, "claude");
        assert!(matches!(
            reg.attest("x", "claude", SHA, None, ""),
            Verdict::Confirmed(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
