//! Конфигурация и пути. Всё рядом с .exe или в %APPDATA%\PCAgent.

use anyhow::Result;
use rand::Rng;
use std::path::{Path, PathBuf};

pub struct Paths {
    #[allow(dead_code)]
    pub root: PathBuf,
    pub env_file: PathBuf,
    pub memory_db: PathBuf,
    pub logs: PathBuf,
    pub router_exe: PathBuf,
    pub consent: PathBuf,
}

impl Paths {
    /// Данные лежат в %APPDATA%\PCAgent, а не рядом с .exe: пользователь может
    /// положить exe в Program Files, куда запись без админа запрещена.
    pub fn resolve() -> Result<Self> {
        let root = dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("PCAgent");
        std::fs::create_dir_all(&root)?;
        std::fs::create_dir_all(root.join("logs"))?;
        Ok(Self {
            env_file: root.join(".env"),
            memory_db: root.join("memory.db"),
            logs: root.join("logs"),
            router_exe: root.join("pcagent-router.exe"),
            consent: root.join("consent.json"),
            root,
        })
    }
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
    pub fn load(env_file: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(env_file).unwrap_or_default();
        let get = |key: &str| -> Option<String> {
            text.lines()
                .map(str::trim)
                .filter(|l| !l.starts_with('#'))
                .find_map(|l| l.strip_prefix(&format!("{key}=")))
                .map(|v| v.trim().trim_matches('"').to_string())
                .filter(|v| !v.is_empty())
        };
        let listen = get("ROUTER_LISTEN").unwrap_or_else(|| "127.0.0.1:8713".into());
        Ok(Self {
            router_url: format!("http://{listen}"),
            router_token: get("ROUTER_TOKEN").unwrap_or_default(),
            adb_path: get("ADB_PATH"),
            ocr_lang: get("OCR_LANG").unwrap_or_else(|| "ru-RU".into()),
            adb_wifi: get("ADB_WIFI"),
            adb_serial: get("ADB_SERIAL"),
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

# Локальная модель (LM Studio / Ollama / llama.cpp в OpenAI-режиме)
LOCAL_BASE_URL=
LOCAL_MODEL=qwen2.5-vl-7b

# Кого пробовать первым (через запятую)
LLM_PRIORITY=anthropic,openai,gemini,openrouter,qwen,deepseek,grok,local

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
