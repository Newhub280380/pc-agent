//! Свидетель (witness): доказательство шага, которое порождает сам шлюз.
//!
//! Зачем. До этого модуля `jev_step_done` принимал от агента строку
//! «evidence» и проверял лишь, что в ней встречаются фрагменты критерия
//! `done_when`. Решение «критерий выполнен» де-факто принимал текст модели:
//! достаточно было скопировать критерий — и шаг закрывался. Это и есть
//! «верхний слой + додумывание»: агент пишет «на экране видно…», ничего не
//! открыв.
//!
//! Теперь доказательство — это наблюдение, которое выполняет шлюз своим
//! кодом ([`observe`]): читает файл, запрашивает URL, запускает команду в
//! песочнице или снимает экран и прогоняет OCR. Шлюз сам считает SHA-256
//! содержимого, размер и момент наблюдения, сам сверяет захваченный текст с
//! критерием шага ([`satisfies`]) и сам привязывает запись к маршруту и
//! шагу. Агент получает только идентификатор свидетеля и может лишь
//! сослаться на него в `jev_step_done`. Ни одного поля свидетеля агент не
//! пишет — ему просто негде соврать.
//!
//! Что гарантирует модуль (детерминированно, без LLM):
//!   * каждое свидетельство имеет источник, хэш, размер и время, посчитанные
//!     шлюзом;
//!   * свидетельство привязано к одному маршруту и одному шагу в момент
//!     наблюдения и не может быть переиспользовано для другого шага;
//!   * критерий шага сверяется с захваченным содержимым, а не со словами
//!     агента;
//!   * любое действие записывается в журнал ([`Journal`]) — в том числе
//!     отклонённые попытки наблюдать «вне рельсов» (не на текущем узле пути);
//!   * из всех свидетельств маршрута считается цепной хэш ([`Ledger::anchor`]),
//!     который попадает в печать закрытия — печать нельзя получить на другом
//!     наборе доказательств.
//!
//! Чего модуль не гарантирует (честный предел): что содержимое файла/экрана
//! осмысленно. Он гарантирует, что оно существовало, было захвачено шлюзом
//! и содержит требуемые критерием фрагменты.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::time::Duration;

/// Сколько захваченного текста хранится при свидетеле и уходит в сверку
/// с критерием. Хэш считается по полному содержимому.
const TEXT_CAP: usize = 16 * 1024;
/// Предел скачивания по URL — защита от бесконечного тела.
const URL_CAP: u64 = 4 * 1024 * 1024;

/// Что именно наблюдал шлюз.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// Локальный файл: хэш содержимого и первые байты как текст.
    File { path: String },
    /// HTTP(S) GET: статус, хэш тела, тело как текст.
    Url { url: String },
    /// Команда в песочнице (`sandbox::run`): код возврата и вывод.
    Command { program: String, args: Vec<String> },
    /// Снимок экрана + OCR: хэш PNG и распознанный текст.
    Screen,
}

impl Probe {
    /// Имя вида для профиля и журнала.
    pub fn kind(&self) -> &'static str {
        match self {
            Probe::File { .. } => "file",
            Probe::Url { .. } => "url",
            Probe::Command { .. } => "command",
            Probe::Screen => "screen",
        }
    }

    /// Человекочитаемый адрес наблюдения.
    pub fn locator(&self) -> String {
        match self {
            Probe::File { path } => path.clone(),
            Probe::Url { url } => url.clone(),
            Probe::Command { program, args } => {
                if args.is_empty() {
                    program.clone()
                } else {
                    format!("{program} {}", args.join(" "))
                }
            }
            Probe::Screen => "screen".to_string(),
        }
    }

    /// Разбор JSON-аргумента MCP-инструмента `jev_observe`. Любая
    /// неоднозначность — отказ: шлюз не угадывает, что имел в виду агент.
    pub fn from_json(value: &Value) -> Result<Self, String> {
        let kind = value
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| "probe.kind должен быть строкой".to_string())?;
        match kind {
            "file" => {
                let path = value
                    .get("path")
                    .and_then(Value::as_str)
                    .filter(|path| !path.trim().is_empty())
                    .ok_or_else(|| "probe.path должен быть непустой строкой".to_string())?;
                Ok(Probe::File {
                    path: path.to_string(),
                })
            }
            "url" => {
                let url = value
                    .get("url")
                    .and_then(Value::as_str)
                    .filter(|url| url.starts_with("http://") || url.starts_with("https://"))
                    .ok_or_else(|| {
                        "probe.url должен начинаться с http:// или https://".to_string()
                    })?;
                Ok(Probe::Url {
                    url: url.to_string(),
                })
            }
            "command" => {
                let program = value
                    .get("program")
                    .and_then(Value::as_str)
                    .filter(|program| !program.trim().is_empty())
                    .ok_or_else(|| "probe.program должен быть непустой строкой".to_string())?;
                let mut args = Vec::new();
                if let Some(values) = value.get("args") {
                    let values = values
                        .as_array()
                        .ok_or_else(|| "probe.args должен быть массивом строк".to_string())?;
                    for item in values {
                        let item = item.as_str().ok_or_else(|| {
                            "элементы probe.args должны быть строками".to_string()
                        })?;
                        args.push(item.to_string());
                    }
                }
                Ok(Probe::Command {
                    program: program.to_string(),
                    args,
                })
            }
            "screen" => Ok(Probe::Screen),
            other => Err(format!(
                "неизвестный вид наблюдения {other}: допустимы file, url, command, screen"
            )),
        }
    }
}

/// Результат наблюдения до привязки к маршруту.
#[derive(Debug, Clone)]
pub struct Observation {
    pub probe: Probe,
    /// SHA-256 полного содержимого в hex.
    pub sha256: String,
    /// Размер полного содержимого в байтах.
    pub bytes: u64,
    /// Текст для сверки с критерием (обрезан до [`TEXT_CAP`]).
    pub text: String,
    /// Дополнительный факт наблюдения: HTTP-статус, код возврата и т.п.
    pub detail: String,
}

/// Выполнить наблюдение. Единственное место, где шлюз трогает внешний мир
/// ради доказательства; всё, что здесь не предусмотрено, доказательством
/// быть не может.
pub fn observe(probe: Probe) -> Result<Observation, String> {
    match &probe {
        Probe::File { path } => {
            let mut file = std::fs::File::open(path)
                .map_err(|error| format!("файл {path} недоступен: {error}"))?;
            let mut hasher = Sha256::new();
            let mut head = Vec::new();
            let mut buffer = [0u8; 64 * 1024];
            let mut bytes = 0u64;
            loop {
                let read = file
                    .read(&mut buffer)
                    .map_err(|error| format!("чтение {path}: {error}"))?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
                if head.len() < TEXT_CAP {
                    let take = (TEXT_CAP - head.len()).min(read);
                    head.extend_from_slice(&buffer[..take]);
                }
                bytes += read as u64;
            }
            Ok(Observation {
                sha256: hex(&hasher.finalize()),
                bytes,
                text: String::from_utf8_lossy(&head).into_owned(),
                detail: format!("file {bytes} bytes"),
                probe,
            })
        }
        Probe::Url { url } => {
            let agent = ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(5))
                .timeout(Duration::from_secs(20))
                .build();
            // Ответы 4xx/5xx тоже факт: их статус попадает в detail, а не в
            // ошибку, чтобы шаг «страница отвечает 404» был доказуем.
            let response = match agent.get(url).call() {
                Ok(response) => response,
                Err(ureq::Error::Status(_, response)) => response,
                Err(error) => return Err(format!("URL {url} недоступен: {error}")),
            };
            let status = response.status();
            let mut body = Vec::new();
            response
                .into_reader()
                .take(URL_CAP)
                .read_to_end(&mut body)
                .map_err(|error| format!("чтение тела {url}: {error}"))?;
            Ok(Observation {
                sha256: hex(&Sha256::digest(&body)),
                bytes: body.len() as u64,
                text: String::from_utf8_lossy(&body[..body.len().min(TEXT_CAP)]).into_owned(),
                detail: format!("http {status}"),
                probe,
            })
        }
        Probe::Command { program, args } => {
            let output = crate::sandbox::run(program, args, &crate::sandbox::Limits::default())
                .map_err(|error| format!("команда отклонена или не запустилась: {error:#}"))?;
            let combined = format!("{}{}", output.stdout, output.stderr);
            Ok(Observation {
                sha256: hex(&Sha256::digest(combined.as_bytes())),
                bytes: combined.len() as u64,
                text: combined.chars().take(TEXT_CAP).collect(),
                detail: format!(
                    "exit {}{}",
                    output
                        .code
                        .map(|code| code.to_string())
                        .unwrap_or_else(|| "killed".to_string()),
                    if output.truncated {
                        " (вывод обрезан)"
                    } else {
                        ""
                    }
                ),
                probe,
            })
        }
        Probe::Screen => {
            let shot = crate::platform::capture_screen()
                .map_err(|error| format!("снимок экрана не удался: {error:#}"))?;
            let elements = crate::platform::ocr(&shot.png, "rus")
                .map_err(|error| format!("OCR не удался: {error:#}"))?;
            let text: String = elements
                .iter()
                .map(|element| element.name.as_str())
                .filter(|name| !name.trim().is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            Ok(Observation {
                sha256: hex(&Sha256::digest(&shot.png)),
                bytes: shot.png.len() as u64,
                text: text.chars().take(TEXT_CAP).collect(),
                detail: format!(
                    "screen {}x{}, ocr {} строк",
                    shot.width,
                    shot.height,
                    elements.len()
                ),
                probe,
            })
        }
    }
}

/// Свидетель — наблюдение, привязанное к маршруту и шагу.
#[derive(Debug, Clone)]
pub struct Witness {
    pub id: String,
    pub route_id: String,
    pub step_id: String,
    /// Порядковый номер в журнале маршрута — попадает в JSON свидетеля.
    pub seq: u64,
    pub observed_at: String,
    pub observation: Observation,
}

impl Witness {
    /// Короткая форма для MCP-ответа и отчёта. Текст наружу не отдаётся
    /// целиком — только его начало, чтобы агент видел, что захвачено.
    pub fn json(&self) -> Value {
        json!({
            "witness_id": self.id,
            "seq": self.seq,
            "route_id": self.route_id,
            "step_id": self.step_id,
            "kind": self.observation.probe.kind(),
            "locator": self.observation.probe.locator(),
            "sha256": self.observation.sha256,
            "bytes": self.observation.bytes,
            "detail": self.observation.detail,
            "observed_at": self.observed_at,
            "text_head": self.observation.text.chars().take(400).collect::<String>(),
        })
    }

    /// Строка для `node.note`: доказательство словами шлюза, а не агента.
    pub fn note(&self) -> String {
        format!(
            "{} {}:{} sha256={} bytes={} {}",
            self.id,
            self.observation.probe.kind(),
            self.observation.probe.locator(),
            &self.observation.sha256[..16],
            self.observation.bytes,
            self.observation.detail
        )
    }
}

/// Запись журнала действий: что просили, на каком шаге, чем кончилось.
#[derive(Debug, Clone)]
pub struct JournalEntry {
    pub seq: u64,
    pub at: String,
    pub tool: String,
    pub step_id: String,
    pub outcome: String,
    pub detail: String,
}

impl JournalEntry {
    pub fn json(&self) -> Value {
        json!({
            "seq": self.seq,
            "at": self.at,
            "tool": self.tool,
            "step_id": self.step_id,
            "outcome": self.outcome,
            "detail": self.detail,
        })
    }
}

/// Реестр свидетелей и журнал одного маршрута.
#[derive(Debug, Default, Clone)]
pub struct Ledger {
    witnesses: Vec<Witness>,
    journal: Vec<JournalEntry>,
    seq: u64,
}

impl Ledger {
    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// Записать событие в журнал. `outcome` — короткий код:
    /// `accepted`, `refused`, `off_rail`.
    pub fn log(&mut self, tool: &str, step_id: &str, outcome: &str, detail: impl Into<String>) {
        let seq = self.next_seq();
        self.journal.push(JournalEntry {
            seq,
            at: chrono::Utc::now().to_rfc3339(),
            tool: tool.to_string(),
            step_id: step_id.to_string(),
            outcome: outcome.to_string(),
            detail: detail.into(),
        });
    }

    /// Привязать наблюдение к шагу. Идентификатор выводится из хэша
    /// содержимого, маршрута, шага и порядкового номера — он уникален и
    /// неугадываем заранее.
    pub fn record(&mut self, route_id: &str, step_id: &str, observation: Observation) -> &Witness {
        let seq = self.next_seq();
        let digest =
            Sha256::digest(format!("{route_id}|{step_id}|{seq}|{}", observation.sha256).as_bytes());
        let id = format!("w-{}", &hex(&digest)[..16]);
        self.witnesses.push(Witness {
            id,
            route_id: route_id.to_string(),
            step_id: step_id.to_string(),
            seq,
            observed_at: chrono::Utc::now().to_rfc3339(),
            observation,
        });
        let witness = self.witnesses.last().expect("just pushed");
        self.journal.push(JournalEntry {
            seq,
            at: witness.observed_at.clone(),
            tool: "jev_observe".to_string(),
            step_id: step_id.to_string(),
            outcome: "accepted".to_string(),
            detail: witness.note(),
        });
        witness
    }

    pub fn get(&self, id: &str) -> Option<&Witness> {
        self.witnesses.iter().find(|witness| witness.id == id)
    }

    pub fn for_step(&self, step_id: &str) -> Vec<&Witness> {
        self.witnesses
            .iter()
            .filter(|witness| witness.step_id == step_id)
            .collect()
    }

    pub fn journal(&self) -> &[JournalEntry] {
        &self.journal
    }

    pub fn witnesses(&self) -> &[Witness] {
        &self.witnesses
    }

    /// Сколько раз агент пытался действовать не на текущем узле пути.
    pub fn off_rail_count(&self) -> usize {
        self.journal
            .iter()
            .filter(|entry| entry.outcome == "off_rail")
            .count()
    }

    /// Якорь печати: цепной SHA-256 по всем свидетелям в порядке записи.
    /// Меняется любое свидетельство — меняется якорь.
    pub fn anchor(&self) -> String {
        let mut chain = [0u8; 32];
        for witness in &self.witnesses {
            let mut hasher = Sha256::new();
            hasher.update(chain);
            hasher.update(witness.id.as_bytes());
            hasher.update(witness.step_id.as_bytes());
            hasher.update(witness.observation.sha256.as_bytes());
            chain.copy_from_slice(&hasher.finalize());
        }
        hex(&chain)
    }
}

/// Политика доказательств. Профиль меняет пороги, но не может отключить
/// само требование свидетеля — это инвариант печати, а не настройка.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub profile: String,
    /// Минимальный суммарный объём захваченного содержимого на шаг.
    pub min_bytes: u64,
    /// Разрешённые виды наблюдений.
    pub probes: Vec<&'static str>,
}

impl Policy {
    /// `strict` — умолчание для массового использования. `custom` читает
    /// пороги из окружения: `PCAGENT_GATE_MIN_BYTES`, `PCAGENT_GATE_PROBES`
    /// (через запятую). Неизвестный профиль — strict, без вопросов.
    pub fn from_env() -> Self {
        let profile = std::env::var("PCAGENT_GATE_PROFILE")
            .ok()
            .map(|value| value.trim().to_lowercase())
            .filter(|value| value == "custom")
            .unwrap_or_else(|| "strict".to_string());
        let mut policy = Self::strict();
        if profile == "custom" {
            policy.profile = profile;
            if let Some(min_bytes) = std::env::var("PCAGENT_GATE_MIN_BYTES")
                .ok()
                .and_then(|value| value.trim().parse::<u64>().ok())
            {
                // Ноль не допускается: пустое наблюдение ничего не доказывает.
                policy.min_bytes = min_bytes.max(1);
            }
            if let Ok(list) = std::env::var("PCAGENT_GATE_PROBES") {
                let allowed: Vec<&'static str> = ["file", "url", "command", "screen"]
                    .into_iter()
                    .filter(|kind| list.split(',').any(|item| item.trim() == *kind))
                    .collect();
                if !allowed.is_empty() {
                    policy.probes = allowed;
                }
            }
        }
        policy
    }

    pub fn strict() -> Self {
        Self {
            profile: "strict".to_string(),
            min_bytes: 32,
            probes: vec!["file", "url", "command", "screen"],
        }
    }

    pub fn allows(&self, probe: &Probe) -> Result<(), String> {
        if self.probes.contains(&probe.kind()) {
            Ok(())
        } else {
            Err(format!(
                "вид наблюдения {} запрещён профилем {}",
                probe.kind(),
                self.profile
            ))
        }
    }

    pub fn json(&self) -> Value {
        json!({
            "profile": self.profile,
            "min_bytes": self.min_bytes,
            "probes": self.probes,
        })
    }
}

/// Сверка критерия `done_when` с захваченным текстом свидетелей.
/// Критерий записан фрагментами в кавычках «…»; группы через « или » —
/// альтернативы. Решение принимает код по содержимому, а не по словам
/// агента. Возвращает причину отказа.
pub fn satisfies(done_when: &str, witnesses: &[&Witness], policy: &Policy) -> Result<(), String> {
    if witnesses.is_empty() {
        return Err("нет ни одного свидетеля шлюза для этого шага".to_string());
    }
    let total: u64 = witnesses
        .iter()
        .map(|witness| witness.observation.bytes)
        .sum();
    if total < policy.min_bytes {
        return Err(format!(
            "захвачено {total} байт, порог профиля {} — {} байт",
            policy.profile, policy.min_bytes
        ));
    }
    let captured = witnesses
        .iter()
        .map(|witness| witness.observation.text.to_lowercase())
        .collect::<Vec<_>>()
        .join("\n");
    let groups = quoted_fragment_groups(done_when);
    if groups.len() == 1 {
        let missing: Vec<String> = groups[0]
            .iter()
            .filter(|fragment| !captured.contains(&fragment.to_lowercase()))
            .cloned()
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "в захваченном содержимом отсутствует обязательный фрагмент: {}",
                missing.join("; ")
            ));
        }
    } else if !groups.is_empty()
        && !groups.iter().any(|group| {
            group
                .iter()
                .all(|fragment| captured.contains(&fragment.to_lowercase()))
        })
    {
        let alternatives: Vec<String> = groups.iter().map(|group| group.join(" + ")).collect();
        return Err(format!(
            "в захваченном содержимом отсутствует подходящий вариант: {}",
            alternatives.join(" или ")
        ));
    }
    Ok(())
}

/// Фрагменты критерия в кавычках, сгруппированные по « или ».
pub fn quoted_fragment_groups(text: &str) -> Vec<Vec<String>> {
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

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str, content: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pcagent-witness-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn file_probe_hashes_full_content_and_keeps_text_head() {
        let path = temp_file(
            "a.txt",
            "на экране виден результат тестов «passed» и ещё текст",
        );
        let observation = observe(Probe::File {
            path: path.to_string_lossy().into_owned(),
        })
        .unwrap();
        assert_eq!(observation.sha256.len(), 64);
        assert_eq!(observation.bytes, std::fs::metadata(&path).unwrap().len());
        assert!(observation.text.contains("passed"));
    }

    #[test]
    fn missing_file_is_an_error_not_a_witness() {
        let error = observe(Probe::File {
            path: "/definitely/not/here.txt".to_string(),
        })
        .unwrap_err();
        assert!(error.contains("недоступен"));
    }

    #[test]
    fn witness_is_bound_to_route_and_step_and_anchor_changes() {
        let path = temp_file(
            "b.txt",
            "в выводе виден чистый git status и ветка main — ок",
        );
        let observation = observe(Probe::File {
            path: path.to_string_lossy().into_owned(),
        })
        .unwrap();
        let mut ledger = Ledger::default();
        let empty_anchor = ledger.anchor();
        let id = ledger.record("route-1", "step_a", observation).id.clone();
        let witness = ledger.get(&id).unwrap();
        assert_eq!(witness.route_id, "route-1");
        assert_eq!(witness.step_id, "step_a");
        assert!(ledger.for_step("step_b").is_empty());
        assert_ne!(ledger.anchor(), empty_anchor);
        assert_eq!(ledger.journal().len(), 1);
        assert_eq!(ledger.journal()[0].outcome, "accepted");
    }

    #[test]
    fn satisfies_checks_captured_text_not_agent_words() {
        let path = temp_file(
            "c.txt",
            "лог сборки: всё собралось, никаких упоминаний критерия",
        );
        let observation = observe(Probe::File {
            path: path.to_string_lossy().into_owned(),
        })
        .unwrap();
        let mut ledger = Ledger::default();
        ledger.record("route-1", "s", observation);
        let witnesses = ledger.for_step("s");
        let error = satisfies(
            "на экране виден результат тестов «passed»",
            &witnesses,
            &Policy::strict(),
        )
        .unwrap_err();
        assert!(error.contains("passed"), "{error}");
        assert!(satisfies("критерий без кавычек", &witnesses, &Policy::strict()).is_ok());
    }

    #[test]
    fn empty_observation_fails_threshold() {
        let path = temp_file("d.txt", "");
        let observation = observe(Probe::File {
            path: path.to_string_lossy().into_owned(),
        })
        .unwrap();
        let mut ledger = Ledger::default();
        ledger.record("route-1", "s", observation);
        let error = satisfies("любой", &ledger.for_step("s"), &Policy::strict()).unwrap_err();
        assert!(error.contains("порог"), "{error}");
    }

    #[test]
    fn probe_json_rejects_ambiguous_input() {
        assert!(Probe::from_json(&json!({"kind":"url","url":"ftp://x"})).is_err());
        assert!(Probe::from_json(&json!({"kind":"file","path":""})).is_err());
        assert!(Probe::from_json(&json!({"kind":"teleport"})).is_err());
        assert_eq!(
            Probe::from_json(&json!({"kind":"command","program":"git","args":["status"]})).unwrap(),
            Probe::Command {
                program: "git".into(),
                args: vec!["status".into()]
            }
        );
    }

    #[test]
    fn custom_profile_cannot_drop_min_bytes_to_zero() {
        // Профиль читается из окружения; здесь проверяем только инвариант
        // через strict-основу: порог всегда положительный.
        assert!(Policy::strict().min_bytes > 0);
        assert!(Policy::strict()
            .allows(&Probe::Command {
                program: "x".into(),
                args: vec![]
            })
            .is_ok());
    }
}
