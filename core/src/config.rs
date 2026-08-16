//! Конфигурация и пути.
//!
//! Данные (база памяти, логи, распакованный роутер) всегда в %APPDATA%\PCAgent:
//! .exe может лежать в Program Files, куда без админа не запишешь.
//! А вот КОНФИГ ищется шире — человек естественно кладёт `.env`
//! рядом с бинарником, и раньше такой файл молча игнорировался.

use anyhow::Result;
use rand::Rng;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub struct Paths {
    #[allow(dead_code)]
    pub root: PathBuf,
    /// Найденный `.env` (если нигде нет — путь в %APPDATA%, туда ляжет шаблон).
    pub env_file: PathBuf,
    /// Найденный `config.json` (старший слой конфига), если есть.
    pub config_json: Option<PathBuf>,
    /// Где искали конфиг — чтобы лог отвечал на вопрос «откуда он читает?».
    pub searched: Vec<PathBuf>,
    pub memory_db: PathBuf,
    pub logs: PathBuf,
    pub router_exe: PathBuf,
    pub consent: PathBuf,
}

/// Порядок поиска конфига: явно указанная папка → рядом с .exe →
/// текущая папка → %APPDATA%\PCAgent.
fn config_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::var_os("PCAGENT_HOME") {
        dirs.push(PathBuf::from(dir));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(p) = exe.parent() {
            dirs.push(p.to_path_buf());
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        dirs.push(cwd);
    }
    dirs.push(root.to_path_buf());
    dirs.dedup();
    dirs
}

impl Paths {
    pub fn resolve() -> Result<Self> {
        let root = dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("PCAgent");
        std::fs::create_dir_all(&root)?;
        std::fs::create_dir_all(root.join("logs"))?;

        let candidates = config_dirs(&root);
        let env_file = candidates
            .iter()
            .map(|d| d.join(".env"))
            .find(|p| p.is_file())
            .unwrap_or_else(|| root.join(".env"));
        let config_json = candidates
            .iter()
            .map(|d| d.join("config.json"))
            .find(|p| p.is_file());

        Ok(Self {
            env_file,
            config_json,
            searched: candidates,
            memory_db: root.join("memory.db"),
            logs: root.join("logs"),
            router_exe: root.join("pcagent-router.exe"),
            consent: root.join("consent.json"),
            root,
        })
    }
}

/// Три слоя конфига с фиксированным старшинством: config.json → ENV → .env.
///
/// Зачем три: config.json удобен человеку и версионируется, ENV нужен для
/// разовой отладки без редактирования файлов, `.env` — то, что привычно.
pub struct Layers {
    json: BTreeMap<String, String>,
    dotenv: BTreeMap<String, String>,
    pub json_path: Option<PathBuf>,
    pub env_path: PathBuf,
}

/// Откуда взялось значение — нужно в логах для диагностики «ключ не виден».
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Json,
    Env,
    DotEnv,
    Default,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Json => "config.json",
            Source::Env => "ENV",
            Source::DotEnv => ".env",
            Source::Default => "default",
        }
    }
}

/// Дружелюбные имена из config.json → канонические ключи окружения.
/// Так можно писать как в запросе (`"api_key"`), так и полным именем
/// (`"OPENAI_API_KEY"`) — работает и то и другое.
fn canonical_key(k: &str) -> String {
    match k.trim().to_ascii_lowercase().as_str() {
        "llm_provider" | "provider" => "LLM_PROVIDER".into(),
        "base_url" | "llm_base_url" => "LLM_BASE_URL".into(),
        "api_key" | "llm_api_key" | "key" => "LLM_API_KEY".into(),
        "model" | "llm_model" => "LLM_MODEL".into(),
        other => other.to_ascii_uppercase(),
    }
}

impl Layers {
    pub fn load(paths: &Paths) -> Result<Self> {
        let mut json = BTreeMap::new();
        if let Some(p) = &paths.config_json {
            let text =
                read_text(p).ok_or_else(|| anyhow::anyhow!("{}: не читается", p.display()))?;
            let v: serde_json::Value = serde_json::from_str(&text)
                .map_err(|e| anyhow::anyhow!("{}: битый JSON: {e}", p.display()))?;
            if let Some(obj) = v.as_object() {
                for (k, val) in obj {
                    let s = match val {
                        serde_json::Value::String(s) => s.clone(),
                        serde_json::Value::Null => continue,
                        other => other.to_string(),
                    };
                    if !s.trim().is_empty() {
                        json.insert(canonical_key(k), s.trim().to_string());
                    }
                }
            }
        }
        Ok(Self {
            json,
            dotenv: parse_dotenv(&paths.env_file),
            json_path: paths.config_json.clone(),
            env_path: paths.env_file.clone(),
        })
    }

    pub fn get(&self, key: &str) -> Option<String> {
        self.get_with_source(key).map(|(v, _)| v)
    }

    pub fn get_with_source(&self, key: &str) -> Option<(String, Source)> {
        if let Some(v) = self.json.get(key) {
            return Some((v.clone(), Source::Json));
        }
        if let Some(v) = std::env::var(key).ok().filter(|v| !v.trim().is_empty()) {
            return Some((v.trim().to_string(), Source::Env));
        }
        self.dotenv
            .get(key)
            .map(|v| (v.clone(), Source::DotEnv))
            .filter(|(v, _)| !v.is_empty())
    }

    /// Значения из config.json для дочернего роутера: он читает только
    /// окружение и `.env`, поэтому старший слой передаём ему как ENV.
    pub fn router_env(&self) -> Vec<(String, String)> {
        self.json
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}

/// Какой провайдер будет использован и виден ли ключ — ровно то, чего
/// не хватало в диагностике «LLM недоступна».
pub struct LlmSummary {
    pub provider: String,
    pub base_url: String,
    pub model: String,
    pub key_found: bool,
    pub key_source: Source,
}

/// Таблица провайдеров для диагностики и пинга (сами запросы делает роутер).
/// Порядок имеет значение: берём первый сконфигурированный.
const PROVIDERS: &[(&str, &str, &str, &str, &str)] = &[
    // имя, ключ, переопределение base_url, base_url по умолчанию, модель
    ("custom", "LLM_API_KEY", "LLM_BASE_URL", "", "LLM_MODEL"),
    (
        "openai",
        "OPENAI_API_KEY",
        "OPENAI_BASE_URL",
        "https://api.openai.com/v1",
        "OPENAI_MODEL",
    ),
    (
        "anthropic",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_BASE_URL",
        "https://api.anthropic.com/v1",
        "ANTHROPIC_MODEL",
    ),
    (
        "gemini",
        "GEMINI_API_KEY",
        "GEMINI_BASE_URL",
        "https://generativelanguage.googleapis.com/v1beta",
        "GEMINI_MODEL",
    ),
    (
        "youtoria",
        "YOUTORIA_API_KEY",
        "YOUTORIA_BASE_URL",
        "https://api.youtoria.ai/v1",
        "YOUTORIA_MODEL",
    ),
    (
        "deepseek",
        "DEEPSEEK_API_KEY",
        "DEEPSEEK_BASE_URL",
        "https://api.deepseek.com/v1",
        "DEEPSEEK_MODEL",
    ),
    (
        "grok",
        "XAI_API_KEY",
        "XAI_BASE_URL",
        "https://api.x.ai/v1",
        "XAI_MODEL",
    ),
    (
        "qwen",
        "QWEN_API_KEY",
        "QWEN_BASE_URL",
        "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
        "QWEN_MODEL",
    ),
    (
        "openrouter",
        "OPENROUTER_API_KEY",
        "OPENROUTER_BASE_URL",
        "https://openrouter.ai/api/v1",
        "OPENROUTER_MODEL",
    ),
    (
        "local",
        "LOCAL_API_KEY",
        "LOCAL_BASE_URL",
        "",
        "LOCAL_MODEL",
    ),
];

impl Layers {
    /// Выбирает того же провайдера, которого возьмёт роутер: явно указанный
    /// в LLM_PROVIDER, иначе первый из LLM_PRIORITY, иначе первый с ключом.
    pub fn llm_summary(&self) -> LlmSummary {
        let named = self.get("LLM_PROVIDER").map(|s| s.to_ascii_lowercase());
        let priority: Vec<String> = self
            .get("LLM_PRIORITY")
            .map(|s| {
                s.split(',')
                    .map(|p| p.trim().to_ascii_lowercase())
                    .filter(|p| !p.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        let mut order: Vec<&str> = Vec::new();
        if let Some(n) = named.as_deref() {
            // Провайдер из config.json может быть и не из таблицы — тогда он
            // настраивается через LLM_BASE_URL/LLM_API_KEY (запись "custom").
            if PROVIDERS.iter().any(|p| p.0 == n) {
                order.push(
                    PROVIDERS
                        .iter()
                        .find(|p| p.0 == n)
                        .map(|p| p.0)
                        .unwrap_or("custom"),
                );
            } else {
                order.push("custom");
            }
        }
        for n in &priority {
            if let Some(p) = PROVIDERS.iter().find(|p| p.0 == n.as_str()) {
                order.push(p.0);
            }
        }
        for p in PROVIDERS {
            order.push(p.0);
        }

        for name in order {
            let spec = match PROVIDERS.iter().find(|p| p.0 == name) {
                Some(s) => s,
                None => continue,
            };
            let key = self.get_with_source(spec.1);
            let base = self.get(spec.2).unwrap_or_else(|| spec.3.to_string());
            // Локальная модель и custom живут без ключа, остальным ключ обязателен.
            let usable =
                key.is_some() || (!base.is_empty() && (name == "local" || name == "custom"));
            if !usable {
                continue;
            }
            let display_name = if name == "custom" {
                named.clone().unwrap_or_else(|| "custom".into())
            } else {
                name.to_string()
            };
            return LlmSummary {
                provider: display_name,
                base_url: base,
                model: self.get(spec.4).unwrap_or_default(),
                key_found: key.is_some(),
                key_source: key.map(|(_, s)| s).unwrap_or(Source::Default),
            };
        }

        LlmSummary {
            provider: named.unwrap_or_else(|| "нет".into()),
            base_url: String::new(),
            model: String::new(),
            key_found: false,
            key_source: Source::Default,
        }
    }

    /// Три строки, которых не хватало, чтобы понять, откуда агент берёт ключ.
    /// Сам ключ НИКОГДА не логируется — только факт наличия и источник.
    pub fn describe(&self, searched: &[PathBuf]) -> Vec<String> {
        let s = self.llm_summary();
        let mut lines = vec![
            format!(
                "Loading LLM config from: {} | .env: {} | искали в: {}",
                self.json_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "config.json не найден".into()),
                if self.env_path.is_file() {
                    self.env_path.display().to_string()
                } else {
                    format!("{} (нет файла)", self.env_path.display())
                },
                searched
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            format!(
                "Found key: {} (provider: {}, source: {})",
                s.key_found,
                s.provider,
                s.key_source.as_str()
            ),
            format!(
                "Using base_url: {} (model: {})",
                if s.base_url.is_empty() {
                    "не задан"
                } else {
                    s.base_url.as_str()
                },
                if s.model.is_empty() {
                    "по умолчанию"
                } else {
                    s.model.as_str()
                }
            ),
        ];
        // Файл есть, но ни одной переменной — почти всегда кодировка UTF-16
        // из Блокнота или строки без `=`. Без этой подсказки выглядит так,
        // будто агент игнорирует ключ.
        if self.env_path.is_file() && self.dotenv.is_empty() {
            lines.push(format!(
                "{}: файл прочитан, но переменных не найдено — сохрани его как UTF-8 в виде КЛЮЧ=значение",
                self.env_path.display()
            ));
        }
        lines
    }
}

/// Блокнот на Windows охотно сохраняет файл в UTF-16 или с BOM, а тогда
/// `read_to_string` либо падает, либо приклеивает BOM к первому ключу — и
/// ключ «есть в .env, но не виден». Поэтому декодируем сами.
fn read_text(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let text = match bytes.as_slice() {
        [0xFF, 0xFE, rest @ ..] => decode_utf16(rest, true),
        [0xFE, 0xFF, rest @ ..] => decode_utf16(rest, false),
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        rest => String::from_utf8_lossy(rest).into_owned(),
    };
    Some(text)
}

fn decode_utf16(bytes: &[u8], little: bool) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| {
            if little {
                u16::from_le_bytes([c[0], c[1]])
            } else {
                u16::from_be_bytes([c[0], c[1]])
            }
        })
        .collect();
    String::from_utf16_lossy(&units)
}

/// Привычные короткие имена из чужих туториалов (`BASE_URL=`, `API_KEY=`)
/// раньше просто игнорировались: агент молчал, что строка не понята.
fn dotenv_key(k: &str) -> String {
    match k.trim().to_ascii_uppercase().as_str() {
        "BASE_URL" => "LLM_BASE_URL".into(),
        "API_KEY" => "LLM_API_KEY".into(),
        "MODEL" => "LLM_MODEL".into(),
        "PROVIDER" => "LLM_PROVIDER".into(),
        other => other.to_string(),
    }
}

fn parse_dotenv(path: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Some(text) = read_text(path) else {
        return out;
    };
    for line in text.lines() {
        let line = line.trim().trim_start_matches("export ").trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        // Хвостовой комментарий после значения — частая причина «ключ с мусором».
        let v = match v.split_once(" #") {
            Some((head, _)) => head,
            None => v,
        };
        let v = v.trim().trim_matches('"').trim_matches('\'').trim();
        if !v.is_empty() {
            out.insert(dotenv_key(k), v.to_string());
        }
    }
    out
}

pub struct Settings {
    pub router_url: String,
    pub router_token: String,
    pub adb_path: Option<String>,
    pub ocr_lang: String,
    /// host:port телефона для ADB по Wi-Fi (пусто = только USB).
    pub adb_wifi: Option<String>,
    /// Serial конкретного телефона, если их подключено несколько.
    pub adb_serial: Option<String>,
}

impl Settings {
    pub fn load(layers: &Layers) -> Result<Self> {
        let listen = layers
            .get("ROUTER_LISTEN")
            .unwrap_or_else(|| "127.0.0.1:8713".into());
        Ok(Self {
            router_url: format!("http://{listen}"),
            router_token: layers.get("ROUTER_TOKEN").unwrap_or_default(),
            adb_path: layers.get("ADB_PATH"),
            ocr_lang: layers.get("OCR_LANG").unwrap_or_else(|| "ru-RU".into()),
            adb_wifi: layers.get("ADB_WIFI"),
            adb_serial: layers.get("ADB_SERIAL"),
        })
    }
}

/// Первый запуск: создаём .env с шаблоном и случайным локальным токеном.
/// Пользователю остаётся вписать любой один ключ — этого достаточно.
pub fn ensure_env_file(path: &Path) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    let token: String = {
        let mut rng = rand::thread_rng();
        (0..32)
            .map(|_| char::from(b'a' + rng.gen_range(0..26)))
            .collect()
    };
    std::fs::write(path, template(&token))?;
    Ok(true)
}

fn template(token: &str) -> String {
    format!(
        r#"# ===== PC AGENT =====
# Вставь ключ ЛЮБОГО провайдера (можно несколько — будет резерв).
# Порядок использования задаётся LLM_PRIORITY.

# Любой OpenAI-совместимый сервис (включая Youtoria) — три строки и готово.
# Если заполнено — используется в первую очередь.
LLM_PROVIDER=
LLM_BASE_URL=
LLM_API_KEY=
LLM_MODEL=

OPENAI_API_KEY=
OPENAI_MODEL=gpt-4o

ANTHROPIC_API_KEY=
ANTHROPIC_MODEL=claude-sonnet-4-20250514

GEMINI_API_KEY=
GEMINI_MODEL=gemini-2.0-flash

DEEPSEEK_API_KEY=
DEEPSEEK_MODEL=deepseek-chat

XAI_API_KEY=
XAI_MODEL=grok-2-vision-1212

QWEN_API_KEY=
QWEN_MODEL=qwen-vl-max

OPENROUTER_API_KEY=
OPENROUTER_MODEL=qwen/qwen2.5-vl-72b-instruct

# Youtoria и любой другой OpenAI-совместимый сервис.
# base_url и имя модели бери из личного кабинета провайдера: значения ниже —
# только заготовка, если они не совпадут, будет "Сервер LLM недоступен".
YOUTORIA_API_KEY=
YOUTORIA_BASE_URL=https://api.youtoria.ai/v1
YOUTORIA_MODEL=gpt-4o

# Локальная модель (LM Studio / Ollama / llama.cpp в OpenAI-режиме)
LOCAL_BASE_URL=
LOCAL_MODEL=qwen2.5-vl-7b

# Кого пробовать первым (через запятую)
LLM_PRIORITY=anthropic,openai,youtoria,gemini,openrouter,qwen,deepseek,grok,local

# ===== Локальные настройки (менять не обязательно) =====
ROUTER_LISTEN=127.0.0.1:8713
ROUTER_TOKEN={token}
LLM_MAX_RETRIES=3
LLM_TIMEOUT_SEC=120
OCR_LANG=ru-RU
# Путь к adb.exe, если он не в PATH
ADB_PATH=
# Телефон по Wi-Fi: 192.168.1.50:5555 (сначала один раз по USB: adb tcpip 5555)
ADB_WIFI=
# Serial телефона, если их несколько (список: adb devices)
ADB_SERIAL=
# URL манифеста автообновления (пусто = выключено)
UPDATE_MANIFEST_URL=
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pcagent-cfg-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn paths_in(dir: &Path) -> Paths {
        Paths {
            root: dir.to_path_buf(),
            env_file: dir.join(".env"),
            config_json: Some(dir.join("config.json")).filter(|p| p.is_file()),
            searched: vec![dir.to_path_buf()],
            memory_db: dir.join("memory.db"),
            logs: dir.join("logs"),
            router_exe: dir.join("router.exe"),
            consent: dir.join("consent.json"),
        }
    }

    #[test]
    fn env_file_next_to_exe_is_read() {
        let dir = tmp("dotenv");
        std::fs::write(dir.join(".env"), "OPENAI_API_KEY=sk-test\nOCR_LANG=en-US\n").unwrap();
        let l = Layers::load(&paths_in(&dir)).unwrap();
        assert_eq!(l.get_with_source("OCR_LANG").unwrap().1, Source::DotEnv);
        let s = l.llm_summary();
        assert!(s.key_found);
        assert_eq!(s.provider, "openai");
        assert_eq!(s.base_url, "https://api.openai.com/v1");
    }

    #[test]
    fn config_json_wins_over_dotenv_and_maps_friendly_keys() {
        let dir = tmp("json");
        std::fs::write(dir.join(".env"), "OPENAI_API_KEY=sk-from-dotenv\n").unwrap();
        std::fs::write(
            dir.join("config.json"),
            r#"{"llm_provider":"youtoria","base_url":"https://api.youtoria.ai/v1","api_key":"sk-json","model":"gpt-4o"}"#,
        )
        .unwrap();
        let l = Layers::load(&paths_in(&dir)).unwrap();
        let s = l.llm_summary();
        assert_eq!(s.provider, "youtoria");
        assert_eq!(s.base_url, "https://api.youtoria.ai/v1");
        assert_eq!(s.key_source, Source::Json);
        assert_eq!(s.model, "gpt-4o");
        // Роутер читает окружение — значения из JSON должны до него доехать.
        let env = l.router_env();
        assert!(env.iter().any(|(k, _)| k == "LLM_API_KEY"));
        assert!(env
            .iter()
            .any(|(k, v)| k == "LLM_PROVIDER" && v == "youtoria"));
    }

    #[test]
    fn process_env_beats_dotenv() {
        let dir = tmp("procenv");
        std::fs::write(dir.join(".env"), "PCAGENT_TEST_X=from-dotenv\n").unwrap();
        std::env::set_var("PCAGENT_TEST_X", "from-env");
        let l = Layers::load(&paths_in(&dir)).unwrap();
        assert_eq!(
            l.get_with_source("PCAGENT_TEST_X").unwrap(),
            ("from-env".to_string(), Source::Env)
        );
        std::env::remove_var("PCAGENT_TEST_X");
    }

    #[test]
    fn describe_never_prints_the_key() {
        let dir = tmp("secret");
        std::fs::write(dir.join(".env"), "YOUTORIA_API_KEY=sk-super-secret-42\n").unwrap();
        let p = paths_in(&dir);
        let l = Layers::load(&p).unwrap();
        let text = l.describe(&p.searched).join("\n");
        assert!(!text.contains("sk-super-secret-42"), "{text}");
        assert!(text.contains("Found key: true"), "{text}");
        assert!(
            text.contains("Using base_url: https://api.youtoria.ai/v1"),
            "{text}"
        );
    }

    #[test]
    fn missing_key_is_reported_as_false() {
        let dir = tmp("nokey");
        std::fs::write(dir.join(".env"), "OCR_LANG=ru-RU\n").unwrap();
        let p = paths_in(&dir);
        let l = Layers::load(&p).unwrap();
        assert!(!l.llm_summary().key_found);
        assert!(l
            .describe(&p.searched)
            .join("\n")
            .contains("Found key: false"));
    }

    #[test]
    fn utf16_dotenv_from_notepad_is_read() {
        let dir = tmp("utf16");
        let text = "OPENAI_API_KEY=sk-utf16\n";
        let mut bytes = vec![0xFF, 0xFE];
        for u in text.encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        std::fs::write(dir.join(".env"), bytes).unwrap();
        let s = Layers::load(&paths_in(&dir)).unwrap().llm_summary();
        assert!(s.key_found, "UTF-16 .env из Блокнота должен читаться");
        assert_eq!(s.provider, "openai");
    }

    #[test]
    fn bom_short_names_and_trailing_comment_are_understood() {
        let dir = tmp("aliases");
        std::fs::write(
            dir.join(".env"),
            "\u{feff}BASE_URL=https://api.openai.com/v1 # мой ключ\nAPI_KEY=\"sk-short\"\n",
        )
        .unwrap();
        let l = Layers::load(&paths_in(&dir)).unwrap();
        assert_eq!(
            l.get("LLM_BASE_URL").as_deref(),
            Some("https://api.openai.com/v1")
        );
        assert_eq!(l.get("LLM_API_KEY").as_deref(), Some("sk-short"));
    }

    #[test]
    fn unreadable_dotenv_is_flagged_in_diagnostics() {
        let dir = tmp("garbage");
        std::fs::write(dir.join(".env"), "тут просто текст без знака равно\n").unwrap();
        let p = paths_in(&dir);
        let text = Layers::load(&p).unwrap().describe(&p.searched).join("\n");
        assert!(text.contains("переменных не найдено"), "{text}");
    }

    #[test]
    fn broken_json_fails_loudly() {
        let dir = tmp("brokenjson");
        std::fs::write(dir.join("config.json"), "{ not json").unwrap();
        // Debug на Layers сознательно не выводится (внутри ключи), поэтому
        // разбираем Result вручную.
        let err = match Layers::load(&paths_in(&dir)) {
            Ok(_) => panic!("битый config.json должен ломать загрузку"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("битый JSON"), "{err}");
    }

    #[test]
    fn settings_come_from_layers() {
        let dir = tmp("settings");
        std::fs::write(
            dir.join(".env"),
            "ROUTER_LISTEN=127.0.0.1:9999\nROUTER_TOKEN=t\n",
        )
        .unwrap();
        let s = Settings::load(&Layers::load(&paths_in(&dir)).unwrap()).unwrap();
        assert_eq!(s.router_url, "http://127.0.0.1:9999");
        assert_eq!(s.router_token, "t");
    }
}
