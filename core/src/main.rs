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

mod acceptance;
mod agent;
mod android;
mod back_gate;
mod budget;
mod checkpoint;
mod config;
mod coordinator;
mod corpus;
mod decisions;
mod endoscope;
mod failure;
#[cfg(test)]
mod fuzz;
mod gui;
mod intent;
mod jev;
mod jev_gate;
mod ledger;
mod llm;
mod logic;
mod memory;
mod minimal;
mod optimize;
mod platform;
mod proof;
mod radar;
mod retry;
mod sandbox;
mod spans;
mod stage_bench;
mod state;
mod supervisor;
mod text;
mod vision;
mod watchdog;
mod witness;

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
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--mcp") {
        let paths = config::Paths::resolve()?;
        config::ensure_env_file(&paths.env_file)?;
        let layers = config::Layers::load(&paths)?;
        layers.export_core_env();
        let settings = config::Settings::load(&layers)?;
        let _router = headless_router(&paths, &layers, &settings);
        jev_gate::serve_stdio()?;
        return Ok(());
    }
    if let Some(question) = flag_value(&args, "--study") {
        let sources = flag_values(&args, "--source");
        let paths = config::Paths::resolve()?;
        config::ensure_env_file(&paths.env_file)?;
        let layers = config::Layers::load(&paths)?;
        layers.export_core_env();
        let settings = config::Settings::load(&layers)?;
        let _router = headless_router(&paths, &layers, &settings);
        println!(
            "{}",
            serde_json::to_string_pretty(&jev_gate::study_json(&question, &sources))?
        );
        return Ok(());
    }
    if let Some(task) = flag_value(&args, "--route") {
        let have: Vec<String> = flag_value(&args, "--have")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|fact| !fact.is_empty())
            .map(str::to_string)
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&jev_gate::route_json(&task, &have))?
        );
        return Ok(());
    }
    // Два служебных режима без GUI: нужны, чтобы Windows-машина (в том числе
    // CI) могла доказать, что .exe реально стартует и видит экран.
    if args.iter().any(|a| a == "--selfcheck") {
        return selfcheck(flag_value(&args, "--report").as_deref());
    }
    if let Some(path) = flag_value(&args, "--shot") {
        return shot(std::path::Path::new(&path));
    }
    if let Some(path) = flag_value(&args, "--radar") {
        return radar_scan(std::path::Path::new(&path), &args);
    }
    // Задача из командной строки: так агента запускают под ловушкой эндоскопа
    // и по расписанию, где окна нет вообще. Код выхода — приговор по задаче.
    if let Some(task) = flag_value(&args, "--task") {
        return headless(&task);
    }

    // В релизе это windows-подсистема: stderr никуда не ведёт, и любой `?`
    // в main раньше заканчивался тихим исчезновением процесса. Теперь
    // причина уходит в лог и в MessageBox.
    match run_app() {
        Ok(()) => Ok(()),
        Err(e) => {
            log::error!("агент не запустился: {e:#}");
            gui::error_dialog(&format!(
                "Агент не запустился.\n\n{e:#}\n\nПодробности в логе %LOCALAPPDATA%\\PCAgent\\logs."
            ));
            Err(e)
        }
    }
}

fn run_app() -> Result<()> {
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

    let layers = config::Layers::load(&paths)?;
    // Три строки диагностики до всего остального: когда ключ «не виден»,
    // первый вопрос — какой файл он вообще прочитал.
    for line in layers.describe(&paths.searched) {
        log::info!("{line}");
    }
    // Настройки ядра, которые читаются из окружения, доезжают только так.
    layers.export_core_env();
    let settings = config::Settings::load(&layers)?;
    let _router = match supervisor::Router::start(
        &paths.router_exe,
        &paths.env_file,
        &format!("{}/health", settings.router_url),
        &layers.router_env(),
    ) {
        Ok(r) => Some(r),
        Err(e) => {
            log::error!("роутер не поднялся: {e}");
            None
        }
    };

    // Native-слой (COM/UIA) поднимаем ПОСЛЕ окна согласия и ДО главного
    // окна — но UIA теперь живёт в потоке агента (см. platform::init), так
    // что здесь только DPI и COM для главного потока. Ошибка не фатальна.
    if let Err(e) = platform::init() {
        log::error!("native-слой недоступен: {e}");
    }

    let llm = llm::LlmClient::new(&settings.router_url, &settings.router_token);
    // Память: если основной файл не открывается (заблокирован зомби-процессом,
    // папка в OneDrive только для чтения, повреждён WAL) — агент раньше умирал
    // здесь с `?` без окна. Теперь уходим на временную базу и говорим об этом.
    let mem = match memory::Memory::open(&paths.memory_db) {
        Ok(m) => m,
        Err(e) => {
            log::error!(
                "память {} не открылась: {e}; работаю на временной базе",
                paths.memory_db.display()
            );
            let tmp = std::env::temp_dir().join("pcagent-memory-fallback.db");
            memory::Memory::open(&tmp)?
        }
    };
    let mem = Arc::new(Mutex::new(mem));
    let mut adb = android::Adb::new(settings.adb_path.clone());
    connect_phone(&mut adb, &settings);

    let (ev_tx, ev_rx) = channel::<AgentEvent>();
    let (cmd_tx, cmd_rx) = channel::<AgentCommand>();
    let stop = Arc::new(AtomicBool::new(false));

    let providers = describe_providers(&llm, &mem, &layers);

    // Рабочий поток: GUI не должен блокироваться на сетевых вызовах ни на мс.
    {
        let llm = llm.clone();
        let mem = mem.clone();
        let stop = stop.clone();
        let ev_tx2 = ev_tx.clone();
        let ocr_lang = settings.ocr_lang.clone();
        let vision_provider = settings.vision_provider.clone();
        std::thread::spawn(move || {
            let mut agent = Agent::new(
                llm,
                mem,
                adb,
                AgentConfig {
                    ocr_lang: ocr_lang.clone(),
                    vision_provider: vision_provider.clone(),
                    ..AgentConfig::default()
                },
                ev_tx2.clone(),
                cmd_rx,
                stop.clone(),
            );
            worker_loop(&mut agent, &ev_tx2);
        });
    }

    // Диагностика конфига видна и в окне: человек, у которого «не видит ключ»,
    // читает лог в GUI, а не файл в %APPDATA%.
    let mut initial_log = vec![format!(
        "{}  Агент готов. Опиши задачу и нажми Старт.",
        chrono::Local::now().format("%H:%M:%S")
    )];
    initial_log.extend(layers.describe(&paths.searched));

    // Watchdog: брошенная задача с целым чекпоинтом продолжается сама.
    // Без снимка продолжать нечего — помечаем её прерванной и говорим об этом.
    {
        let guard = mem.lock().unwrap_or_else(|e| e.into_inner());
        if let Ok(stale) = guard.interrupted_tasks() {
            let store = checkpoint::Store::open(checkpoint::default_dir()).ok();
            let resume = store
                .as_ref()
                .and_then(|s| watchdog::resume_candidate(s, &stale).ok().and_then(|v| v));
            for (id, _) in &stale {
                let picked = resume
                    .as_ref()
                    .map(|(rid, _, _)| rid == id)
                    .unwrap_or(false);
                if !picked {
                    let _ = guard.task_finish(id, "interrupted");
                }
            }
            if let Some((id, task, snap)) = resume {
                let title: String = task.chars().take(60).collect();
                initial_log.push(format!(
                    "Прошлая сессия оборвалась. Продолжаю «{title}» с чекпоинта, шаг {}.",
                    snap.step
                ));
                let _ = cmd_tx.send(AgentCommand::Resume { task_id: id, task });
            } else if !stale.is_empty() {
                let names = stale
                    .iter()
                    .map(|(_, t)| t.chars().take(60).collect::<String>())
                    .collect::<Vec<_>>()
                    .join("; ");
                initial_log.push(format!(
                    "⚠ Прошлая сессия умерла посреди {} задач(и) без чекпоинта: {}",
                    stale.len(),
                    names
                ));
            }
        }
    }

    let app = gui::AppState {
        task: String::new(),
        log: initial_log,
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

    run_gui(app)?;

    platform::shutdown();
    Ok(())
}

/// Окно агента с откатом рендерера. Зачем: glow требует OpenGL 2.0+, а на
/// машинах без GPU (VM, RDP-сессия, Windows Server, CI-раннер) его нет —
/// приложение молча умирало с «egui_glow requires opengl 2.0+» в логе. wgpu
/// умеет DX12, в том числе программный WARP, и там окно всё равно откроется.
/// Альтернативы: тащить ANGLE/Mesa рядом с .exe (ломает «один файл») или
/// сразу жить на wgpu (дороже по старту на обычном ПК).
fn run_gui(app: gui::AppState) -> Result<()> {
    let pending = Arc::new(Mutex::new(Some(app)));
    let mut last: Option<String> = None;
    // Порядок рендереров: сначала wgpu (DX12 есть на любой Windows 10/11,
    // включая программный WARP), потом glow. Раньше было наоборот, и это
    // и было «запустился и сразу умер»: glow без OpenGL 2.0+ ICD не
    // возвращает Err, а ПАНИКУЕТ внутри glutin
    // (`config_iterator.next().expect(...)`), а в релизе panic = "abort" —
    // процесс исчезает до того, как цикл доберётся до второго рендерера.
    // Переопределение: PCAGENT_RENDERER=glow|wgpu.
    let order: Vec<eframe::Renderer> = match std::env::var("PCAGENT_RENDERER")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "glow" => vec![eframe::Renderer::Glow, eframe::Renderer::Wgpu],
        "wgpu" => vec![eframe::Renderer::Wgpu],
        _ => vec![eframe::Renderer::Wgpu, eframe::Renderer::Glow],
    };
    for renderer in order {
        let opts = eframe::NativeOptions {
            viewport: eframe::egui::ViewportBuilder::default()
                .with_inner_size([1100.0, 680.0])
                .with_min_inner_size([820.0, 520.0])
                // Drag&drop нам не нужен, а winit ради него вызывает
                // OleInitialize и паникует на RPC_E_CHANGED_MODE, если COM
                // в этом потоке уже поднят в другом режиме (наш native-слой
                // и UIA инициализируют COM раньше окна).
                .with_drag_and_drop(false),
            renderer,
            // DX12 первым: на машинах без видеокарты Windows всё равно даёт
            // программный адаптер WARP, а Vulkan/GL там просто отсутствуют.
            wgpu_options: eframe::egui_wgpu::WgpuConfiguration {
                supported_backends: eframe::wgpu::util::backend_bits_from_env().unwrap_or(
                    eframe::wgpu::Backends::DX12
                        | eframe::wgpu::Backends::VULKAN
                        | eframe::wgpu::Backends::GL,
                ),
                power_preference: eframe::wgpu::PowerPreference::None,
                ..Default::default()
            },
            ..Default::default()
        };
        let slot = pending.clone();
        // Паника внутри рендерера (glutin/egui_glow/wgpu-hal) — это не
        // «ошибка окна», а смерть процесса при panic=abort. Ловим её здесь
        // и превращаем в обычный Err, чтобы сработал откат на следующий
        // рендерер и нативный MessageBox с причиной.
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            eframe::run_native(
                "PC Agent",
                opts,
                Box::new(move |_cc| {
                    let app = slot
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .take()
                        .ok_or("окно агента уже было создано")?;
                    Ok(Box::new(app) as Box<dyn eframe::App>)
                }),
            )
        }));
        let res: std::result::Result<(), String> = match res {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e.to_string()),
            Err(payload) => Err(format!("паника рендерера: {}", panic_message(&payload))),
        };
        match res {
            Ok(()) => return Ok(()),
            Err(e) => {
                log::error!("GUI на {renderer:?} не поднялась: {e}");
                last = Some(e);
                // Состояние уже забрали в умершее окно — повторять нечем.
                if pending.lock().unwrap_or_else(|e| e.into_inner()).is_none() {
                    break;
                }
            }
        }
    }
    let why = last.unwrap_or_else(|| "неизвестная ошибка".into());
    // Окна нет, консоли в релизе тоже нет: без этого сообщения двойной клик
    // по ярлыку выглядит как «ничего не произошло».
    gui::error_dialog(&format!(
        "Не удалось открыть окно агента.\n\n{why}\n\nСкорее всего нет драйвера видеокарты или сессия без рабочего стола. Подробности в логе %APPDATA%\\PCAgent\\logs."
    ));
    anyhow::bail!("GUI: {why}")
}

/// Подключение телефона на старте, а не в середине задачи: если он не
/// отвечает, человек узнает об этом сразу, а не на 20-м шаге.
fn connect_phone(adb: &mut android::Adb, settings: &config::Settings) {
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
}

/// Один прогон без окна. Печатает отчёт в stdout и выходит с кодом 1, если
/// задача не закрыта: иначе «готово» нельзя отличить от падения в скриптах.
fn headless(task: &str) -> Result<()> {
    let paths = config::Paths::resolve()?;
    init_logging(&paths.logs);
    config::ensure_env_file(&paths.env_file)?;
    let layers = config::Layers::load(&paths)?;
    layers.export_core_env();
    let settings = config::Settings::load(&layers)?;
    let _router = supervisor::Router::start(
        &paths.router_exe,
        &paths.env_file,
        &format!("{}/health", settings.router_url),
        &layers.router_env(),
    )
    .map_err(|e| anyhow::anyhow!("роутер не поднялся: {e}"))?;
    if let Err(e) = platform::init() {
        log::error!("native-слой недоступен: {e}");
    }

    let llm = llm::LlmClient::new(&settings.router_url, &settings.router_token);
    let mem = Arc::new(Mutex::new(memory::Memory::open(&paths.memory_db)?));
    let mut adb = android::Adb::new(settings.adb_path.clone());
    // Телефон подключаем и здесь: иначе прогон из скрипта видел «устройства
    // нет» при настроенном adb_wifi и уходил решать задачу на компьютере.
    connect_phone(&mut adb, &settings);
    let (ev_tx, ev_rx) = channel::<AgentEvent>();
    let (cmd_tx, cmd_rx) = channel::<AgentCommand>();
    let stop = Arc::new(AtomicBool::new(false));
    let mut agent = Agent::new(
        llm,
        mem,
        adb,
        AgentConfig {
            ocr_lang: settings.ocr_lang.clone(),
            vision_provider: settings.vision_provider.clone(),
            // На бесплатной модели один шаг стоит 10-80 с, и в 45 минут
            // многошаговая задача просто не укладывается. Срок задаётся
            // снаружи: PCAGENT_MAX_MIN.
            max_minutes: std::env::var("PCAGENT_MAX_MIN")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(AgentConfig::default().max_minutes),
            ..AgentConfig::default()
        },
        ev_tx,
        cmd_rx,
        stop,
    );
    // Отвечать на вопросы в этом режиме некому: обрываем канал сразу, чтобы
    // агент не ждал человека вечно, а честно записал это как причину.
    drop(cmd_tx);
    // События печатаем по ходу дела, а не в конце: иначе долгая задача
    // выглядит зависшей, и непонятно, на каком шаге она стоит.
    let printer = std::thread::spawn(move || {
        while let Ok(ev) = ev_rx.recv() {
            match ev {
                AgentEvent::Log(l) => println!("{l}"),
                AgentEvent::Thought(t) => println!("мысль: {t}"),
                AgentEvent::Done(t) => println!("ГОТОВО: {t}"),
                AgentEvent::Failed(t) => println!("НЕ ЗАКРЫТО: {t}"),
                AgentEvent::Ask { question, .. } => println!("нужен человек: {question}"),
                AgentEvent::NeedTool { what, why, .. } => {
                    println!("нужен доступ: {what} — {why}")
                }
                _ => {}
            }
        }
    });

    let report = agent.run_task(task);
    drop(agent);
    let _ = printer.join();
    match report {
        Ok(r) if r.closed || r.chat => {
            println!("{}", r.text);
            Ok(())
        }
        Ok(r) => {
            println!("{}", r.text);
            std::process::exit(1);
        }
        Err(e) => {
            println!("сорвалось: {e}");
            std::process::exit(1);
        }
    }
}

fn dispatch(ev_tx: &Sender<AgentEvent>, result: Result<agent::TaskReport>) {
    match result {
        // «Готово» — только доказанное решение или обычный ответ
        // на разговор. Незакрытая задача не маскируется под успех.
        Ok(report) if report.closed || report.chat => {
            let _ = ev_tx.send(AgentEvent::Done(report.text));
        }
        Ok(report) => {
            let _ = ev_tx.send(AgentEvent::Failed(report.text));
        }
        Err(e) => {
            let _ = ev_tx.send(AgentEvent::Failed(e.to_string()));
        }
    }
}

/// Приём команд из GUI. Живёт всё время работы приложения: после задачи
/// агент возвращается сюда и ждёт следующую.
fn worker_loop(agent: &mut Agent, ev_tx: &Sender<AgentEvent>) {
    // Канал обрывается, когда GUI закрылся — это и есть выход из цикла.
    while let Some(cmd) = agent.next_command() {
        match cmd {
            AgentCommand::Start(task) => {
                agent.reset_stop();
                let result = agent.run_task(&task);
                dispatch(ev_tx, result);
            }
            AgentCommand::Resume { task_id, task } => {
                agent.reset_stop();
                let result = agent.resume_task(&task_id, &task);
                dispatch(ev_tx, result);
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

/// Значение флага в виде `--flag value`. Своего парсера аргументов достаточно:
/// флагов два, тянуть clap ради них — лишние 300 КБ в .exe.
fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .filter(|v| !v.starts_with("--"))
        .cloned()
}

fn flag_values(args: &[String], flag: &str) -> Vec<String> {
    args.windows(2)
        .filter(|pair| pair[0] == flag && !pair[1].starts_with("--"))
        .map(|pair| pair[1].clone())
        .collect()
}

fn headless_router(
    paths: &config::Paths,
    layers: &config::Layers,
    settings: &config::Settings,
) -> Option<supervisor::Router> {
    if layers.llm_summary().provider == "нет" {
        return None;
    }
    match supervisor::Router::start(
        &paths.router_exe,
        &paths.env_file,
        &format!("{}/health", settings.router_url),
        &layers.router_env(),
    ) {
        Ok(router) => Some(router),
        Err(error) => {
            eprintln!("JEV LLM router unavailable: {error}");
            None
        }
    }
}

/// Самопроверка: пути, .env, роутер, native-слой, экран, память.
/// Пишет отчёт в файл, потому что в релизе консоли у GUI-приложения нет.
fn selfcheck(report: Option<&str>) -> Result<()> {
    let mut lines: Vec<String> = vec![format!(
        "pcagent selfcheck {}  exe={}",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    )];
    let mut failed = 0usize;
    let mut step = |name: &str, r: Result<String>| -> bool {
        match r {
            Ok(d) => {
                lines.push(format!("[ok]   {name}: {d}"));
                true
            }
            Err(e) => {
                failed += 1;
                lines.push(format!("[FAIL] {name}: {e}"));
                false
            }
        }
    };

    let paths = config::Paths::resolve()?;
    step("пути", Ok(paths.root.display().to_string()));
    step(
        ".env",
        config::ensure_env_file(&paths.env_file).map(|created| {
            if created {
                "создан шаблон".to_string()
            } else {
                "уже есть".to_string()
            }
        }),
    );
    let layers = config::Layers::load(&paths)?;
    for line in layers.describe(&paths.searched) {
        step("config", Ok(line));
    }
    layers.export_core_env();
    let settings = config::Settings::load(&layers)?;
    let router = supervisor::Router::start(
        &paths.router_exe,
        &paths.env_file,
        &format!("{}/health", settings.router_url),
        &layers.router_env(),
    );
    match router {
        Ok(r) => {
            step(
                "роутер",
                Ok(format!("{} /health отвечает", settings.router_url)),
            );
            drop(r);
        }
        Err(e) => {
            step("роутер", Err(e));
        }
    }
    if step(
        "native-слой",
        platform::init().map(|()| "инициализирован".into()),
    ) {
        step(
            "экран",
            platform::screen_size().map(|(w, h)| format!("{w}x{h}")),
        );
        step(
            "скриншот",
            platform::capture_screen()
                .map(|s| format!("{}x{}, PNG {} КБ", s.width, s.height, s.png.len() / 1024)),
        );
        step(
            "активное окно",
            platform::foreground_window().map(|(_, title)| title),
        );
    }
    step(
        "память",
        memory::Memory::open(&paths.memory_db).and_then(|m| {
            let (mm, sh, ls) = m.stats()?;
            Ok(format!("{mm} записей / {sh} полок / {ls} уроков"))
        }),
    );
    platform::shutdown();

    lines.push(format!(
        "итог: проверок {}, провалено {failed}",
        lines.len() - 1
    ));
    let text = lines.join("\n");
    println!("{text}");
    if let Some(path) = report {
        if let Some(dir) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(dir).ok();
        }
        std::fs::write(path, format!("{text}\n"))?;
    }
    if failed > 0 {
        anyhow::bail!("selfcheck: провалено проверок: {failed}");
    }
    Ok(())
}

/// Снимок экрана в файл — тем же путём, которым агент «видит» экран.
fn shot(path: &std::path::Path) -> Result<()> {
    platform::init()?;
    let s = platform::capture_screen();
    platform::shutdown();
    let s = s?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    std::fs::write(path, &s.png)?;
    println!("{}x{} -> {}", s.width, s.height, path.display());
    Ok(())
}

/// Разовый прогон RADAR без GUI: `pcagent.exe --radar radar.json [--out leads.json]`.
/// Отдельный режим, а не действие агента, потому что это долгая пакетная
/// работа с сетью — её гоняют по расписанию, а не в диалоге.
fn radar_scan(cfg_path: &std::path::Path, args: &[String]) -> Result<()> {
    let paths = config::Paths::resolve()?;
    init_logging(&paths.logs);
    let layers = config::Layers::load(&paths)?;
    let cfg = radar::config::RadarConfig::load(cfg_path)?;
    let db = paths.root.join("radar.db");
    let out = flag_value(args, "--out").map(std::path::PathBuf::from);
    let report = radar::run(&cfg, &db, layers.get("TWOGIS_API_KEY"), out.as_deref())?;
    let text = format!(
        "radar «{}» ({} м вокруг {:.5},{:.5}): {}\nбаза: {}{}",
        cfg.niche,
        cfg.geo.radius_m,
        cfg.geo.lat,
        cfg.geo.lon,
        report.summary(),
        db.display(),
        out.map(|p| format!("\njson: {}", p.display()))
            .unwrap_or_default()
    );
    log::info!("{text}");
    println!("{text}");
    Ok(())
}

fn describe_providers(
    llm: &llm::LlmClient,
    mem: &Arc<Mutex<memory::Memory>>,
    layers: &config::Layers,
) -> String {
    let health = if llm.health() {
        "роутер: ок"
    } else {
        "роутер: НЕ ОТВЕЧАЕТ (проверь .env)"
    };
    // «роутер: ок» вводил в заблуждение: локальный процесс жив, а ключа
    // нет. Показываем рядом провайдера и факт наличия ключа.
    let s = layers.llm_summary();
    let key = if s.key_found {
        format!("ключ: {} ({})", s.provider, s.key_source.as_str())
    } else {
        "ключ: нет — впиши в .env или config.json".to_string()
    };
    let stats = mem
        .lock()
        .ok()
        .and_then(|m| m.stats().ok())
        .map(|(mm, sh, ls)| format!("память: {mm} записей / {sh} полок / {ls} уроков"))
        .unwrap_or_default();
    format!("{health} | {key} | {stats}")
}

fn init_logging(dir: &std::path::Path) {
    let path = dir.join(format!(
        "agent-{}.log",
        chrono::Local::now().format("%Y-%m-%d")
    ));
    if let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        // wgpu/naga на info вываливают в лог весь сгенерированный HLSL —
        // это десятки КБ на каждый запуск и лог агента становится нечитаемым.
        env_logger::Builder::from_env(
            env_logger::Env::default()
                .default_filter_or("info,wgpu=warn,wgpu_core=warn,wgpu_hal=warn,naga=warn"),
        )
        .target(env_logger::Target::Pipe(Box::new(file)))
        .init();
    } else {
        env_logger::init();
    }
    install_panic_hook(path);
}

/// Паника в GUI-сборке раньше была невидимой: консоли нет, окна ещё нет,
/// а panic=abort убивал процесс до записи в лог. Хук пишет место и текст
/// паники в тот же файл лога, что и всё остальное, — иначе «запустился и
/// сразу умер» нечем диагностировать.
fn install_panic_hook(log_path: std::path::PathBuf) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let msg = panic_message(info.payload());
        let where_ = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "?".into());
        let line = format!(
            "{} PANIC at {where_}: {msg}\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
        );
        log::error!("{}", line.trim_end());
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            use std::io::Write;
            let _ = f.write_all(line.as_bytes());
        }
        previous(info);
    }));
}

/// Текст из payload паники: &str или String, иначе «неизвестно».
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "неизвестная паника".into()
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

/// На Linux ярлык — это .desktop-файл. Кладём и в меню приложений, и на
/// рабочий стол: человек ищет иконку там, где привык.
#[cfg(target_os = "linux")]
fn create_desktop_shortcut() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let exe = std::env::current_exe()?;
    let entry = format!(
        "[Desktop Entry]\nType=Application\nName=PC Agent\nComment=Автономный агент за компьютером\nExec={}\nTerminal=false\nCategories=Utility;\n",
        exe.display()
    );
    let mut targets: Vec<std::path::PathBuf> = Vec::new();
    if let Some(data) = dirs::data_dir() {
        let apps = data.join("applications");
        std::fs::create_dir_all(&apps)?;
        targets.push(apps.join("pcagent.desktop"));
    }
    if let Some(desktop) = dirs::desktop_dir() {
        targets.push(desktop.join("pcagent.desktop"));
    }
    for path in targets {
        std::fs::write(&path, &entry)?;
        // Без бита исполнения GNOME показывает ярлык как «недоверенный».
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

#[cfg(not(any(windows, target_os = "linux")))]
fn create_desktop_shortcut() -> Result<()> {
    Ok(())
}
