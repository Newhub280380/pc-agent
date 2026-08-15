//! Точка входа. Один .exe: GUI + ядро агента + вшитый Go-роутер.
//!
//! Порядок запуска:
//!   1. создать %APPDATA%\PCAgent и .env (первый запуск);
//!   2. показать окно согласия один раз и создать ярлык на рабочем столе;
//!   3. распаковать и поднять роутер, дождаться /health;
//!   4. поднять native-слой (DPI, COM, UIA);
//!   5. запустить рабочий поток агента и отдать управление GUI.

// Без консольного окна в релизе; в debug консоль оставляем для логов.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod agent;
mod android;
mod config;
#[cfg(test)]
mod fuzz;
mod gui;
mod llm;
mod memory;
mod platform;
mod supervisor;

use agent::{Agent, AgentCommand, AgentConfig, AgentEvent};
use anyhow::Result;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};

const CONSENT_TEXT: &str = "\
Агент будет работать ТОЛЬКО на этом компьютере и под твоей учётной записью Windows:

• видеть экран (скриншоты, UI-элементы, текст);
• двигать мышь, печатать, кликать — как это делаешь ты;
• открывать твой браузер с твоими профилями и сессиями;
• читать и писать буфер обмена;
• запускать приложения и файлы, которые ты попросишь;
• управлять Android-телефоном по ADB, если он подключён.

Что агент НЕ делает:
• не отправляет твои файлы и пароли куда-либо, кроме выбранного тобой LLM-провайдера;
• коды 2FA и SMS не сохраняет — вводит и забывает;
• перед необратимыми действиями (оплата, отправка, удаление) спрашивает тебя.

Ключи API берутся из .env в %APPDATA%\\PCAgent — ими распоряжаешься только ты.";

fn main() -> Result<()> {
    let paths = config::Paths::resolve()?;
    init_logging(&paths.logs);

    let first_run = config::ensure_env_file(&paths.env_file)?;
    if first_run || !paths.consent.exists() {
        if !gui::consent_dialog(CONSENT_TEXT) {
            return Ok(());
        }
        std::fs::write(&paths.consent, chrono::Utc::now().to_rfc3339())?;
        create_desktop_shortcut().ok();
        if first_run {
            // Ключей ещё нет — открываем .env в блокноте, чтобы человек
            // вставил свой ключ прямо сейчас, без чтения документации.
            let _ = open_in_editor(&paths.env_file);
        }
    }

    let settings = config::Settings::load(&paths.env_file)?;
    let _router = match supervisor::Router::start(
        &paths.router_exe,
        &paths.env_file,
        &format!("{}/health", settings.router_url),
    ) {
        Ok(r) => Some(r),
        Err(e) => {
            log::error!("роутер не поднялся: {e}");
            None
        }
    };

    if let Err(e) = platform::init() {
        log::error!("native-слой недоступен: {e}");
    }

    let llm = llm::LlmClient::new(&settings.router_url, &settings.router_token);
    let mem = Arc::new(Mutex::new(memory::Memory::open(&paths.memory_db)?));
    let mut adb = android::Adb::new(settings.adb_path.clone());
    // Телефон подключаем на старте, а не в середине задачи: если он не
    // отвечает, человек узнает об этом сразу, а не на 20-м шаге.
    if let Some(ip) = settings.adb_wifi.as_deref() {
        match adb.connect_wifi(ip.split(':').next().unwrap_or(ip)) {
            Ok(()) => log::info!("телефон подключён по Wi-Fi: {ip}"),
            Err(e) => log::warn!("ADB по Wi-Fi не поднялся: {e}"),
        }
    } else if adb.available() {
        match adb.devices() {
            Ok(list) if !list.is_empty() => {
                // При нескольких телефонах adb требует -s, иначе любая команда
                // падает с "more than one device". Берём первый и пишем в лог,
                // какой именно, чтобы человек мог задать ADB_SERIAL явно.
                if let Some(serial) = settings
                    .adb_serial
                    .as_deref()
                    .or(list.first().map(|s| s.as_str()))
                {
                    adb.select(serial);
                }
                log::info!("ADB-устройства: {list:?}");
            }
            Ok(_) => log::info!("ADB есть, но телефон не подключён"),
            Err(e) => log::warn!("ADB: {e}"),
        }
    }

    let (ev_tx, ev_rx) = channel::<AgentEvent>();
    let (cmd_tx, cmd_rx) = channel::<AgentCommand>();
    let stop = Arc::new(AtomicBool::new(false));

    let providers = describe_providers(&llm, &mem);

    // Рабочий поток: GUI не должен блокироваться на сетевых вызовах ни на мс.
    {
        let llm = llm.clone();
        let mem = mem.clone();
        let stop = stop.clone();
        let ev_tx2 = ev_tx.clone();
        let ocr_lang = settings.ocr_lang.clone();
        std::thread::spawn(move || {
            let mut agent = Agent::new(
                llm,
                mem,
                adb,
                AgentConfig {
                    ocr_lang: ocr_lang.clone(),
                    ..AgentConfig::default()
                },
                ev_tx2.clone(),
                cmd_rx,
                stop.clone(),
            );
            worker_loop(&mut agent, &ev_tx2);
        });
    }

    let app = gui::AppState {
        task: String::new(),
        log: vec![format!(
            "{}  Агент готов. Опиши задачу и нажми Старт.",
            chrono::Local::now().format("%H:%M:%S")
        )],
        thought: String::new(),
        plan: vec![],
        running: false,
        pending_question: None,
        answer: String::new(),
        status: "ожидание".into(),
        tx: cmd_tx,
        rx: ev_rx,
        stop,
        start_requested: Arc::new(AtomicBool::new(false)),
        providers,
    };

    let opts = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 680.0])
            .with_min_inner_size([820.0, 520.0]),
        ..Default::default()
    };
    eframe::run_native("PC Agent", opts, Box::new(|_cc| Ok(Box::new(app))))
        .map_err(|e| anyhow::anyhow!("GUI: {e}"))?;

    platform::shutdown();
    Ok(())
}

/// Приём команд из GUI. Живёт всё время работы приложения: после задачи
/// агент возвращается сюда и ждёт следующую.
fn worker_loop(agent: &mut Agent, ev_tx: &Sender<AgentEvent>) {
    // Канал обрывается, когда GUI закрылся — это и есть выход из цикла.
    while let Some(cmd) = agent.next_command() {
        match cmd {
            AgentCommand::Start(task) => {
                agent.reset_stop();
                match agent.run_task(&task) {
                    Ok(report) => {
                        let _ = ev_tx.send(AgentEvent::Done(report));
                    }
                    Err(e) => {
                        let _ = ev_tx.send(AgentEvent::Failed(e.to_string()));
                    }
                }
            }
            AgentCommand::Stop => {
                agent.request_stop();
                let _ = ev_tx.send(AgentEvent::Idle);
            }
            // Ответ пришёл, когда никто не спрашивал — игнорируем.
            AgentCommand::Answer(_) => {}
        }
    }
}

fn describe_providers(llm: &llm::LlmClient, mem: &Arc<Mutex<memory::Memory>>) -> String {
    let health = if llm.health() {
        "роутер: ок"
    } else {
        "роутер: НЕ ОТВЕЧАЕТ (проверь .env)"
    };
    let stats = mem
        .lock()
        .ok()
        .and_then(|m| m.stats().ok())
        .map(|(mm, sh, ls)| format!("память: {mm} записей / {sh} полок / {ls} уроков"))
        .unwrap_or_default();
    format!("{health} | {stats}")
}

fn init_logging(dir: &std::path::Path) {
    let path = dir.join(format!(
        "agent-{}.log",
        chrono::Local::now().format("%Y-%m-%d")
    ));
    if let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
            .target(env_logger::Target::Pipe(Box::new(file)))
            .init();
    } else {
        env_logger::init();
    }
}

#[cfg(windows)]
fn open_in_editor(path: &std::path::Path) -> Result<()> {
    std::process::Command::new("notepad").arg(path).spawn()?;
    Ok(())
}

#[cfg(not(windows))]
fn open_in_editor(path: &std::path::Path) -> Result<()> {
    std::process::Command::new("xdg-open").arg(path).spawn()?;
    Ok(())
}

/// Ярлык на рабочем столе создаём через PowerShell (WScript.Shell):
/// это единственный способ сделать .lnk без COM-биндингов и внешних крейтов.
#[cfg(windows)]
fn create_desktop_shortcut() -> Result<()> {
    let exe = std::env::current_exe()?;
    let desktop = dirs::desktop_dir().ok_or_else(|| anyhow::anyhow!("нет папки рабочего стола"))?;
    let lnk = desktop.join("PC Agent.lnk");
    if lnk.exists() {
        return Ok(());
    }
    let ps = format!(
        "$s=(New-Object -ComObject WScript.Shell).CreateShortcut('{}');$s.TargetPath='{}';$s.WorkingDirectory='{}';$s.Save()",
        lnk.display(),
        exe.display(),
        exe.parent().map(|p| p.display().to_string()).unwrap_or_default()
    );
    std::process::Command::new("powershell")
        .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &ps])
        .status()?;
    Ok(())
}

#[cfg(not(windows))]
fn create_desktop_shortcut() -> Result<()> {
    Ok(())
}
