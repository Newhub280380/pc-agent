//! Управление Android-телефоном через ADB (USB или Wi-Fi).
//!
//! Почему ADB, а не Appium/uiautomator2-сервер:
//!   - ADB уже есть в Android Platform Tools, ставится один раз, не требует
//!     ничего на телефоне кроме включённой отладки по USB;
//!   - Appium — это Node/Java-стек и вебдрайвер, что противоречит требованию
//!     «один .exe без рантаймов».
//!
//! Как агент «видит» телефон: `uiautomator dump` даёт XML со всеми элементами
//! и их bounds — это прямой аналог UI Automation на Windows, тоже без
//! распознавания картинки. Скриншот (screencap) идёт в LLM только когда
//! XML пуст (игры, Flutter/Canvas-приложения, кастомные вьюхи).

use anyhow::{anyhow, bail, Result};
use std::process::{Command, Stdio};
use std::time::Duration;

#[derive(Clone)]
pub struct Adb {
    exe: String,
    serial: Option<String>,
    /// Кэш проверки наличия adb: без него каждый tap порождал бы два
    /// процесса вместо одного (~30-50мс лишних на каждое действие).
    avail: std::cell::Cell<Option<bool>>,
}

#[derive(Debug, Clone)]
pub struct AndroidElement {
    pub text: String,
    pub desc: String,
    pub class: String,
    pub clickable: bool,
    pub cx: i32,
    pub cy: i32,
}

impl Adb {
    pub fn new(exe: Option<String>) -> Self {
        Self {
            exe: exe.unwrap_or_else(|| "adb".into()),
            serial: None,
            avail: std::cell::Cell::new(None),
        }
    }

    pub fn available(&self) -> bool {
        if let Some(v) = self.avail.get() {
            return v;
        }
        let v = self.probe();
        self.avail.set(Some(v));
        v
    }

    fn probe(&self) -> bool {
        Command::new(&self.exe)
            .arg("version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        if !self.available() {
            // Явная просьба вместо тихого отказа — как требовал пользователь.
            bail!("ADB не найден. Установи Android Platform Tools и укажи путь в .env (ADB_PATH), либо добавь adb в PATH");
        }
        let mut cmd = Command::new(&self.exe);
        if let Some(s) = &self.serial {
            cmd.args(["-s", s]);
        }
        cmd.args(args);
        let out = cmd.output()?;
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        if !out.status.success() {
            bail!(
                "adb {:?} упал: {}",
                args,
                if stderr.is_empty() { stdout } else { stderr }
            );
        }
        Ok(stdout)
    }

    /// Список устройств. Если ровно одно — сразу выбираем его.
    pub fn devices(&mut self) -> Result<Vec<String>> {
        let out = self.run(&["devices"])?;
        let list: Vec<String> = out
            .lines()
            .skip(1)
            .filter(|l| l.contains("\tdevice"))
            .filter_map(|l| l.split('\t').next().map(str::to_string))
            .collect();
        if list.len() == 1 {
            self.serial = Some(list[0].clone());
        }
        Ok(list)
    }

    pub fn select(&mut self, serial: &str) {
        self.serial = Some(serial.to_string());
    }

    /// Подключение по Wi-Fi: телефон и ПК в одной сети, порт 5555.
    /// Первый раз всё равно нужен кабель — это ограничение Android, не наше.
    pub fn connect_wifi(&mut self, ip: &str) -> Result<()> {
        self.run(&["tcpip", "5555"]).ok();
        std::thread::sleep(Duration::from_millis(1500));
        let out = self.run(&["connect", &format!("{ip}:5555")])?;
        if out.to_lowercase().contains("unable") || out.to_lowercase().contains("failed") {
            bail!("не удалось подключиться к {ip}:5555 — проверь, что телефон в той же Wi-Fi сети и отладка включена");
        }
        self.serial = Some(format!("{ip}:5555"));
        Ok(())
    }

    pub fn shell(&self, cmd: &str) -> Result<String> {
        self.run(&["shell", cmd])
    }

    pub fn tap(&self, x: i32, y: i32) -> Result<()> {
        self.shell(&format!("input tap {x} {y}")).map(|_| ())
    }

    pub fn swipe(&self, x1: i32, y1: i32, x2: i32, y2: i32, ms: i32) -> Result<()> {
        self.shell(&format!("input swipe {x1} {y1} {x2} {y2} {ms}"))
            .map(|_| ())
    }

    /// Ввод текста. `input text` не умеет кириллицу и пробелы — поэтому для
    /// не-ASCII используем ADBKeyboard, если он установлен, иначе честно
    /// сообщаем, что нужно поставить (агент просит инструмент, а не молчит).
    pub fn type_text(&self, text: &str) -> Result<()> {
        if text.is_ascii() {
            let escaped = text.replace(' ', "%s").replace('\'', "\\'");
            return self.shell(&format!("input text '{escaped}'")).map(|_| ());
        }
        let has_kb = self
            .shell("ime list -a")
            .unwrap_or_default()
            .contains("com.android.adbkeyboard");
        if !has_kb {
            bail!("для ввода кириллицы нужен ADBKeyboard: установи APK (github.com/senzhk/ADBKeyBoard) и включи его как способ ввода");
        }
        self.shell("ime set com.android.adbkeyboard/.AdbIME").ok();
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, text);
        self.shell(&format!("am broadcast -a ADB_INPUT_B64 --es msg '{b64}'"))
            .map(|_| ())
    }

    pub fn key(&self, keycode: &str) -> Result<()> {
        self.shell(&format!("input keyevent {keycode}")).map(|_| ())
    }

    pub fn open_app(&self, package: &str) -> Result<()> {
        self.shell(&format!(
            "monkey -p {package} -c android.intent.category.LAUNCHER 1"
        ))
        .map(|_| ())
    }

    pub fn current_app(&self) -> Result<String> {
        let out = self.shell("dumpsys window | grep -E 'mCurrentFocus'")?;
        Ok(out.trim().to_string())
    }

    pub fn screenshot_png(&self) -> Result<Vec<u8>> {
        let mut cmd = Command::new(&self.exe);
        if let Some(s) = &self.serial {
            cmd.args(["-s", s]);
        }
        let out = cmd.args(["exec-out", "screencap", "-p"]).output()?;
        if !out.status.success() || out.stdout.is_empty() {
            bail!("не удалось снять скриншот телефона");
        }
        Ok(out.stdout)
    }

    /// Элементы экрана из uiautomator XML.
    pub fn ui_elements(&self) -> Result<Vec<AndroidElement>> {
        // /dev/tty вместо файла: экономит два раунда adb pull.
        let xml = self.shell("uiautomator dump /dev/tty")?;
        Ok(parse_ui_xml(&xml))
    }
}

/// Минимальный парсер атрибутов uiautomator. Полноценный XML-парсер здесь
/// избыточен: структура плоская и генерируется машиной.
fn parse_ui_xml(xml: &str) -> Vec<AndroidElement> {
    let mut out = vec![];
    for node in xml.split("<node ").skip(1) {
        let attr = |k: &str| -> String {
            node.split(&format!("{k}=\""))
                .nth(1)
                .and_then(|s| s.split('"').next())
                .unwrap_or("")
                .to_string()
        };
        let bounds = attr("bounds"); // формат [x1,y1][x2,y2]
        let nums: Vec<i32> = bounds
            .split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse().ok())
            .collect();
        if nums.len() < 4 {
            continue;
        }
        let text = attr("text");
        let desc = attr("content-desc");
        if text.is_empty() && desc.is_empty() {
            continue; // безымянные контейнеры модели не нужны
        }
        out.push(AndroidElement {
            text,
            desc,
            class: attr("class"),
            clickable: attr("clickable") == "true",
            cx: (nums[0] + nums[2]) / 2,
            cy: (nums[1] + nums[3]) / 2,
        });
    }
    out
}

/// Текстовое описание экрана телефона для промпта.
pub fn android_screen_prompt(adb: &Adb) -> Result<String> {
    let app = adb.current_app().unwrap_or_default();
    let els = adb.ui_elements().map_err(|e| anyhow!("uiautomator: {e}"))?;
    let mut s = format!("ЭКРАН ТЕЛЕФОНА. Активное: {app}\nЭЛЕМЕНТЫ (текст | класс | центр):\n");
    for e in els.iter().take(120) {
        let label = if e.text.is_empty() { &e.desc } else { &e.text };
        s.push_str(&format!(
            "- {} | {} | {},{}{}\n",
            label,
            e.class.rsplit('.').next().unwrap_or(""),
            e.cx,
            e.cy,
            if e.clickable {
                " | кликабельно"
            } else {
                ""
            }
        ));
    }
    Ok(s)
}
