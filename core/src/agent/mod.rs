//! Ядро: цикл Восприятие → Мышление → Действие → Рефлексия.
//!
//! Инварианты, которые здесь защищены (каждый — из реального опыта того,
//! как ломаются агенты):
//!   - агент никогда не «думает» без свежего наблюдения экрана;
//!   - у каждой подзадачи есть критерий готовности, иначе цикл бесконечен;
//!   - два одинаковых действия подряд без изменения экрана = зацикливание,
//!     запускаем рефлексию и перепланирование;
//!   - необратимые действия при низкой уверенности требуют человека;
//!   - лимит шагов и таймаут — жёсткие, чтобы не сжечь бюджет API за ночь.

pub mod action;
pub mod executor;
pub mod perception;
pub mod planner;
pub mod reflection;

use crate::android::Adb;
use crate::llm::{extract_json, LlmClient, Message};
use crate::memory::Memory;
use action::{Action, Decision};
use anyhow::Result;
use executor::{Executor, Outcome};
use planner::Plan;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Событие для GUI.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    Log(String),
    Thought(String),
    Plan(Vec<(String, bool)>),
    /// Нужен человек: 2FA/SMS/решение. GUI показывает поле ввода.
    Ask {
        question: String,
        secret: bool,
    },
    /// Нужен инструмент/доступ.
    NeedTool {
        what: String,
        why: String,
        how: String,
    },
    Done(String),
    Failed(String),
    Idle,
}

/// Команда от GUI.
pub enum AgentCommand {
    Start(String),
    Answer(String),
    Stop,
}

pub struct AgentConfig {
    pub max_steps: u32,
    pub max_minutes: u64,
    /// Порог уверенности для необратимых действий (оплата, отправка, удаление).
    pub destructive_confidence: f64,
    /// true = спрашивать человека перед каждым необратимым действием.
    pub confirm_destructive: bool,
    pub ocr_lang: String,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_steps: 120,
            max_minutes: 45,
            destructive_confidence: 0.85,
            confirm_destructive: true,
            ocr_lang: "ru-RU".into(),
        }
    }
}

const ACT_SYSTEM: &str = r#"Ты — автономный агент, управляющий компьютером Windows и телефоном Android как живой человек.
Ты видишь состояние экрана (UI-элементы, текст, иногда скриншот) и выбираешь РОВНО ОДНО следующее действие.

Правила:
1. Думай на 2-3 шага вперёд, но делай один шаг.
2. Предпочитай click_element (по имени элемента) вместо click_xy — это надёжнее.
3. Если элемента нет — сначала проверь: не нужно ли закрыть баннер/модалку, проскроллить, подождать загрузку, сменить окно.
4. Никогда не выдумывай, что действие удалось. Смотри на реальный экран.
5. Если нужен код из SMS/приложения, 2FA или решение человека — action ask_user.
6. Если не хватает инструмента или доступа (нет ADB, нет приложения, нет аккаунта) — action need_tool с конкретной просьбой. Не отказывайся от задачи.
7. Важные факты о сайте/приложении сохраняй через action remember — они пригодятся завтра.
8. Когда критерий текущей подзадачи выполнен — action subtask_done. Когда вся цель достигнута — action finish.

Отвечай СТРОГО JSON:
{"observation":"что вижу","thought":"рассуждение","confidence":0.0-1.0,"next":{"action":"click_element","name":"Войти"}}

Доступные action: click_element{name,control_type}, click_xy{x,y,button,double}, type_text{text,press_enter},
key_combo{combo}, scroll{clicks,horizontal}, drag{x1,y1,x2,y2}, open_url{url}, launch_app{path,args},
focus_window{title_contains}, wait{seconds,reason}, remember{title,body,importance}, ask_user{question,secret},
adb_tap{x,y}, adb_swipe{x1,y1,x2,y2,ms}, adb_text{text}, adb_key{keycode}, adb_open_app{package}, adb_shell{cmd},
subtask_done{result}, finish{report}, need_tool{what,why,how_to_get}"#;

pub struct Agent {
    llm: LlmClient,
    mem: Arc<Mutex<Memory>>,
    exec: Executor,
    cfg: AgentConfig,
    tx: Sender<AgentEvent>,
    rx: Receiver<AgentCommand>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl Agent {
    pub fn new(
        llm: LlmClient,
        mem: Arc<Mutex<Memory>>,
        adb: Adb,
        cfg: AgentConfig,
        tx: Sender<AgentEvent>,
        rx: Receiver<AgentCommand>,
        stop: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self {
            llm,
            mem,
            exec: Executor::new(adb),
            cfg,
            tx,
            rx,
            stop,
        }
    }

    /// Доступ к памяти. Отравленный мьютекс (паника в другом потоке) не должен
    /// добивать агента: данные SQLite от этого не портятся, поэтому берём
    /// содержимое и работаем дальше.
    fn mem_lock(&self) -> std::sync::MutexGuard<'_, Memory> {
        self.mem.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Блокирующее ожидание команды от GUI. None = GUI закрылся.
    pub fn next_command(&self) -> Option<AgentCommand> {
        self.rx.recv().ok()
    }

    pub fn request_stop(&self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn reset_stop(&self) {
        self.stop.store(false, std::sync::atomic::Ordering::Relaxed);
    }

    fn log(&self, s: impl Into<String>) {
        let s = s.into();
        log::info!("{s}");
        let _ = self.tx.send(AgentEvent::Log(s));
    }

    /// Главный цикл. Возвращает отчёт для пользователя.
    pub fn run_task(&mut self, task: &str) -> Result<String> {
        let task_id = format!("t{}", chrono::Utc::now().timestamp());
        let started = Instant::now();
        self.log(format!("Задача: {task}"));

        // 1. Поднимаем прошлый опыт ДО планирования.
        let hints = {
            let m = self.mem_lock();
            m.recall_global(task, 8)?
                .into_iter()
                .map(|i| format!("- [{}] {}: {}", i.shelf, i.title, i.summary))
                .collect::<Vec<_>>()
                .join("\n")
        };
        if !hints.is_empty() {
            self.log("Нашёл прошлый опыт по этой теме, учитываю в плане");
        }

        let mut plan: Plan = planner::make_plan(&self.llm, task, &hints)?;
        self.send_plan(&plan);
        self.log(format!("План из {} подзадач", plan.subtasks.len()));

        let mut step: u32 = 0;
        let mut failures: Vec<String> = vec![];
        let mut last_action = String::new();
        let mut same_action_count = 0u32;
        let mut history: Vec<String> = vec![];
        // Подпись последнего урока: если после него шаг прошёл — урок рабочий
        // и его надо поднимать в ранжировании. Так память сама отбирает советы,
        // которые реально помогают, а не те, что красиво звучали.
        let mut pending_lesson: Option<String> = None;
        // Режим телефона: определяем по формулировке задачи, дальше он
        // переключается сам — по тому, куда агент реально ходит.
        let mut phone_mode = looks_like_phone_task(task);
        if phone_mode {
            self.log("Задача про телефон — смотрю экран Android");
        }

        while step < self.cfg.max_steps {
            if self.stop.load(std::sync::atomic::Ordering::Relaxed) {
                return Ok("Остановлено пользователем".into());
            }
            if started.elapsed() > Duration::from_secs(self.cfg.max_minutes * 60) {
                return Ok(format!(
                    "Стоп по таймауту {} мин. Прогресс: {:?}",
                    self.cfg.max_minutes,
                    plan.progress()
                ));
            }
            let Some(sub) = plan.current().cloned() else {
                return Ok(self.finish_report(&plan, &history));
            };
            step += 1;

            // --- ВОСПРИЯТИЕ ---
            // Скриншот подключаем, когда текстовых данных не хватило или
            // когда предыдущий шаг провалился (нужны «глаза», а не догадки).
            let need_shot = !failures.is_empty() || same_action_count > 0;
            let view = if phone_mode {
                match self.perceive_phone(need_shot) {
                    Ok(v) => v,
                    Err(e) => {
                        // Телефон отвалился — не выдумываем, а просим подключить.
                        self.log(format!("Не вижу телефон: {e}"));
                        let ans = self.ask_human(
                            &format!("Нужен телефон по ADB: {e}\nПодключи кабелем, разреши отладку и напиши «готово» (или «пк», чтобы работать на компьютере)"),
                            false,
                        );
                        if ans.to_lowercase().contains("пк") {
                            phone_mode = false;
                        }
                        continue;
                    }
                }
            } else {
                match perception::perceive(need_shot, &self.cfg.ocr_lang) {
                    Ok(o) => View {
                        ctx: o.memory_context(),
                        prompt: o.to_prompt(),
                        screenshot_png: o.screenshot_png,
                    },
                    Err(e) => {
                        self.log(format!("Не вижу экран: {e}"));
                        std::thread::sleep(Duration::from_secs(1));
                        continue;
                    }
                }
            };
            let ctx = view.ctx.clone();
            let shelf = Memory::shelf_for(&ctx);

            // --- ПАМЯТЬ: только нужная полка ---
            let (shelf_notes, lessons, digest) = {
                let m = self.mem_lock();
                let notes = m
                    .recall(&shelf, &sub.goal, 6)?
                    .into_iter()
                    .map(|i| {
                        format!(
                            "- {}: {}",
                            i.title,
                            if i.summary.is_empty() {
                                i.body
                            } else {
                                i.summary
                            }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let les = m
                    .lessons_for(&ctx, 4)?
                    .into_iter()
                    .map(|(c, f)| format!("- было: {c} → делай: {f}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                (notes, les, m.shelf_digest(&shelf).unwrap_or_default())
            };

            // --- МЫШЛЕНИЕ ---
            let mut prompt = format!(
                "{}\n\nТЕКУЩАЯ ПОДЗАДАЧА #{}: {}\nГОТОВО КОГДА: {}\n\n{}\n",
                plan.to_prompt(),
                sub.id,
                sub.goal,
                sub.done_when,
                view.prompt
            );
            if !digest.is_empty() {
                prompt.push_str(&format!("\nЧТО Я УЖЕ ЗНАЮ ПРО {shelf}: {digest}\n"));
            }
            if !shelf_notes.is_empty() {
                prompt.push_str(&format!(
                    "\nПАМЯТЬ ПО ЭТОМУ МЕСТУ ({shelf}):\n{shelf_notes}\n"
                ));
            }
            if !lessons.is_empty() {
                prompt.push_str(&format!(
                    "\nУРОКИ ИЗ ПРОШЛЫХ ОШИБОК (учти обязательно):\n{lessons}\n"
                ));
            }
            if !history.is_empty() {
                let tail: Vec<String> = history.iter().rev().take(6).rev().cloned().collect();
                prompt.push_str(&format!("\nПОСЛЕДНИЕ ШАГИ:\n{}\n", tail.join("\n")));
            }
            if same_action_count >= 2 {
                prompt.push_str("\nВНИМАНИЕ: ты повторяешь одно и то же действие без результата. Смени подход.\n");
            }

            let msg = match &view.screenshot_png {
                Some(png) => Message::user_with_image(prompt, png),
                None => Message::user(prompt),
            };
            let resp =
                match self
                    .llm
                    .complete(&[Message::system(ACT_SYSTEM), msg], "act", true, 0.2)
                {
                    Ok(r) => r,
                    Err(e) => {
                        self.log(format!("LLM недоступна: {e}"));
                        std::thread::sleep(Duration::from_secs(3));
                        continue;
                    }
                };
            let decision: Decision =
                match extract_json(&resp.text).and_then(|v| Ok(serde_json::from_value(v)?)) {
                    Ok(d) => d,
                    Err(e) => {
                        self.log(format!("Модель вернула не то ({e}), переспрашиваю"));
                        continue;
                    }
                };
            let _ = self.tx.send(AgentEvent::Thought(decision.thought.clone()));

            let act_str = decision.next.short();
            // Куда агент пошёл — туда и смотрим на следующем шаге.
            match &decision.next {
                Action::AdbTap { .. }
                | Action::AdbSwipe { .. }
                | Action::AdbText { .. }
                | Action::AdbKey { .. }
                | Action::AdbOpenApp { .. }
                | Action::AdbShell { .. } => phone_mode = true,
                Action::ClickElement { .. }
                | Action::ClickXY { .. }
                | Action::OpenUrl { .. }
                | Action::LaunchApp { .. }
                | Action::FocusWindow { .. } => phone_mode = false,
                _ => {}
            }
            if act_str == last_action {
                same_action_count += 1;
            } else {
                same_action_count = 0;
                last_action = act_str.clone();
            }

            // --- ЗАЩИТА ОТ НЕОБРАТИМОГО ---
            if decision.next.is_destructive()
                && (decision.confidence < self.cfg.destructive_confidence
                    || self.cfg.confirm_destructive)
            {
                let q = format!(
                    "Подтверди необратимое действие: {}\n(уверенность модели {:.0}%)",
                    act_str,
                    decision.confidence * 100.0
                );
                let ans = self.ask_human(&q, false);
                if !ans.trim().to_lowercase().starts_with(['д', 'y']) {
                    self.log("Пользователь отменил действие");
                    failures.push("пользователь отменил необратимое действие".into());
                    continue;
                }
            }

            // --- ДЕЙСТВИЕ ---
            self.log(format!("Шаг {step}: {}", human_readable(&decision.next)));
            let result = self.exec.execute(&decision.next);

            // Побочный эффект remember обрабатываем здесь: у исполнителя нет
            // доступа к памяти, и это правильно — разделение ответственности.
            if let Action::Remember {
                title,
                body,
                importance,
            } = &decision.next
            {
                let m = self.mem_lock();
                m.remember(&shelf, "fact", title, body, body, *importance)?;
            }

            match result {
                Ok(Outcome::Ok(msg)) => {
                    if let Some(sig) = pending_lesson.take() {
                        let _ = self.mem_lock().mark_lesson_worked(&sig);
                    }
                    history.push(format!(
                        "{step}. {} → {}",
                        human_readable(&decision.next),
                        msg
                    ));
                    self.mem_lock()
                        .log_step(&task_id, step as i64, &act_str, &msg, true)?;
                    failures.clear();
                }
                Ok(Outcome::SubtaskDone(res)) => {
                    self.log(format!("Подзадача #{} готова: {res}", sub.id));
                    plan.complete_current(&res);
                    self.send_plan(&plan);
                    history.push(format!("{step}. подзадача #{} готова", sub.id));
                    failures.clear();
                }
                Ok(Outcome::Finished(report)) => {
                    self.log("Задача выполнена");
                    let m = self.mem_lock();
                    m.remember(
                        &shelf,
                        "outcome",
                        &format!("Задача: {task}"),
                        &report,
                        &report,
                        0.7,
                    )?;
                    // Короткая «шпаргалка» по месту: читается всегда, в отличие от
                    // отдельных заметок, которые достаются только по совпадению слов.
                    let digest = format!("{task} — получилось за {step} шагов: {report}");
                    let digest: String = digest.chars().take(400).collect();
                    m.set_shelf_digest(&shelf, &digest)?;
                    return Ok(report);
                }
                Ok(Outcome::NeedHuman { question, secret }) => {
                    let ans = self.ask_human(&question, secret);
                    history.push(format!("{step}. спросил человека: {question}"));
                    if !secret {
                        history.push(format!("   ответ: {ans}"));
                    } else {
                        // Секрет НЕ попадает ни в историю, ни в промпт, ни в логи.
                        // Он сразу печатается в поле на экране.
                        let _ = crate::platform::type_text(&ans);
                        history.push("   код введён (значение не сохраняется)".into());
                    }
                }
                Ok(Outcome::NeedTool { what, why, how }) => {
                    let _ = self.tx.send(AgentEvent::NeedTool {
                        what: what.clone(),
                        why: why.clone(),
                        how: how.clone(),
                    });
                    let ans = self.ask_human(
                        &format!("Нужен инструмент: {what}\nЗачем: {why}\nКак подключить: {how}\n\nНапиши 'готово', когда подключишь (или 'пропусти')"),
                        false,
                    );
                    history.push(format!("{step}. запрошен инструмент {what}: {ans}"));
                }
                Err(e) => {
                    let err = e.to_string();
                    self.log(format!("Ошибка: {err}"));
                    failures.push(err.clone());
                    self.mem_lock()
                        .log_step(&task_id, step as i64, &act_str, &err, false)?;

                    // --- РЕФЛЕКСИЯ ---
                    let screen = view.prompt.clone();
                    match reflection::analyze(
                        &self.llm,
                        &self.mem_lock(),
                        &ctx,
                        &sub.goal,
                        &act_str,
                        &err,
                        &screen,
                    ) {
                        Ok(lesson) => {
                            self.log(format!("Разбор: {} → {}", lesson.cause, lesson.fix));
                            pending_lesson = Some(lesson.signature.clone());
                            let hint = if lesson.retry_hint.is_empty() {
                                String::new()
                            } else {
                                format!(" Пробуй: {}", lesson.retry_hint)
                            };
                            history.push(format!(
                                "{step}. ОШИБКА: {err}. Вывод: {}.{hint}",
                                lesson.fix
                            ));
                        }
                        Err(e2) => self.log(format!("Не смог разобрать ошибку: {e2}")),
                    }

                    // Два провала подряд — план плохой, а не руки кривые.
                    if failures.len() >= 2 {
                        self.log("Перепланирую: текущий план не ведёт к цели");
                        if let Ok(p) = planner::replan(&self.llm, &plan, &screen, &failures) {
                            plan = p;
                            self.send_plan(&plan);
                            failures.clear();
                        }
                    }
                }
            }
        }
        Ok(format!(
            "Достигнут лимит {} шагов. {}",
            self.cfg.max_steps,
            self.finish_report(&plan, &history)
        ))
    }

    /// Восприятие телефона: uiautomator даёт элементы с координатами,
    /// скриншот подключаем только когда элементов мало или что-то сломалось
    /// (PNG с телефона — это ~1-2 МБ и лишняя секунда на каждый шаг).
    fn perceive_phone(&self, need_shot: bool) -> Result<View> {
        let adb = &self.exec.adb;
        if !adb.available() {
            anyhow::bail!("adb не найден или устройство не подключено");
        }
        let prompt = crate::android::android_screen_prompt(adb)?;
        let app = adb.current_app().unwrap_or_default();
        let pkg = app
            .split('/')
            .next()
            .and_then(|s| s.split_whitespace().last())
            .unwrap_or("android")
            .to_string();
        let thin = prompt.lines().count() < 8;
        let screenshot_png = if need_shot || thin {
            adb.screenshot_png().ok()
        } else {
            None
        };
        Ok(View {
            ctx: format!("app:{pkg}"),
            prompt,
            screenshot_png,
        })
    }

    /// Блокирующий запрос к человеку. Именно это закрывает 2FA и SMS-коды:
    /// агент не пытается их обойти, а честно ждёт человека.
    fn ask_human(&self, question: &str, secret: bool) -> String {
        let _ = self.tx.send(AgentEvent::Ask {
            question: question.to_string(),
            secret,
        });
        self.log(format!("Жду ответа: {question}"));
        loop {
            if self.stop.load(std::sync::atomic::Ordering::Relaxed) {
                return String::new();
            }
            match self.rx.recv_timeout(Duration::from_millis(300)) {
                Ok(AgentCommand::Answer(a)) => return a,
                Ok(AgentCommand::Stop) => {
                    self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
                    return String::new();
                }
                Ok(AgentCommand::Start(_)) => {} // новая задача во время ожидания игнорируется
                Err(_) => continue,
            }
        }
    }

    fn send_plan(&self, plan: &Plan) {
        let items = plan
            .subtasks
            .iter()
            .map(|s| (s.goal.clone(), s.done))
            .collect();
        let _ = self.tx.send(AgentEvent::Plan(items));
    }

    fn finish_report(&self, plan: &Plan, history: &[String]) -> String {
        let (done, total) = plan.progress();
        format!(
            "Готово {done}/{total} подзадач.\nЧто сделано:\n{}",
            history
                .iter()
                .rev()
                .take(15)
                .rev()
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        )
    }
}

/// Унифицированный «взгляд» — экран ПК или экран телефона.
/// Один тип позволяет держать цикл принятия решений общим для обеих сред.
struct View {
    ctx: String,
    prompt: String,
    screenshot_png: Option<Vec<u8>>,
}

/// Грубая эвристика первого шага. Дальше режим уточняется по действиям,
/// поэтому ошибка здесь стоит максимум одного лишнего шага.
fn looks_like_phone_task(task: &str) -> bool {
    let t = task.to_lowercase();
    [
        "телефон",
        "смартфон",
        "android",
        "андроид",
        "adb",
        "на телефоне",
        "capcut",
        "tiktok",
        "тикток",
    ]
    .iter()
    .any(|k| t.contains(k))
}

fn human_readable(a: &Action) -> String {
    match a {
        Action::ClickElement { name, .. } => format!("клик по «{name}»"),
        Action::ClickXY { x, y, .. } => format!("клик ({x},{y})"),
        Action::TypeText { text, .. } => format!("печатаю «{}»", truncate(text, 40)),
        Action::KeyCombo { combo } => format!("клавиши {combo}"),
        Action::Scroll { clicks, .. } => format!("скролл {clicks}"),
        Action::OpenUrl { url } => format!("открываю {url}"),
        Action::LaunchApp { path, .. } => format!("запускаю {path}"),
        Action::Wait { seconds, reason } => format!("жду {seconds}с ({reason})"),
        Action::AskUser { question, .. } => format!("спрашиваю: {question}"),
        Action::AdbOpenApp { package } => format!("телефон: открываю {package}"),
        Action::AdbTap { x, y } => format!("телефон: тап ({x},{y})"),
        other => other.short(),
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}
