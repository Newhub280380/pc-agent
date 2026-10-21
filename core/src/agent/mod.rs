//! Ядро: ReAct-цикл Thought → Action → Observation поверх графа задач.
//!
//! Инварианты, которые здесь защищены (каждый — из реального опыта того,
//! как ломаются агенты):
//!   - агент никогда не «думает» без свежего наблюдения экрана;
//!   - Observation берётся ИЗ ЭКРАНА после действия, а не из слов модели;
//!   - каждое действие проходит self-verify: без подтверждения шаг не успешен;
//!   - утверждения модели проверяются на «заземление» (grounding.rs): выдуманные
//!     пути, ссылки и кнопки отсекаются ДО выполнения;
//!   - уверенность ниже 0.8 — вместо действия уточняющий вопрос человеку;
//!   - у каждого узла графа есть критерий готовности, иначе цикл бесконечен;
//!   - провал узла ведёт к откату выполненного и перепланированию, а не к
//!     слепому продолжению;
//!   - необратимые действия требуют человека;
//!   - лимит шагов и таймаут — жёсткие, чтобы не сжечь бюджет API за ночь.

pub mod action;
pub mod executor;
pub mod graph;
pub mod grounding;
pub mod perception;
pub mod planner;
pub mod react;
pub mod reflection;
pub mod verify;

use crate::android::Adb;
use crate::llm::{extract_json, LlmClient, Message};
use crate::memory::Memory;
use crate::retry::{retry, Backoff};
use action::{Action, Decision};
use anyhow::Result;
use executor::{Executor, Outcome};
use graph::TaskGraph;
use grounding::Evidence;
use react::{Gate, Step, Trace};
use std::collections::HashMap;
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
    /// Процесс умер или завис: продолжить задачу с последнего целого чекпоинта.
    Resume {
        task_id: String,
        task: String,
    },
}

/// Итог задачи. `closed` истинно только после доказательства Z3,
/// а не потому что модель прислала отчёт.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskReport {
    pub text: String,
    pub closed: bool,
    /// Обычный ответ на разговор. Это не выполнение задачи на компьютере.
    pub chat: bool,
}

impl TaskReport {
    fn chat(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            closed: false,
            chat: true,
        }
    }

    fn closed(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            closed: true,
            chat: false,
        }
    }

    fn open(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            closed: false,
            chat: false,
        }
    }
}

pub struct AgentConfig {
    pub max_steps: u32,
    /// Политики для Z3-фильтра планов и действий.
    pub policies: crate::logic::Policies,
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
            policies: crate::logic::Policies::new(),
            destructive_confidence: 0.85,
            confirm_destructive: true,
            ocr_lang: "ru-RU".into(),
        }
    }
}

/// Сколько шагов даём на один откат: если компенсирующее действие не удалось,
/// бесконечно ковырять его хуже, чем честно сказать «откатить не смог».
const MAX_ROLLBACK_STEPS: u32 = 8;

const ACT_SYSTEM: &str = r#"Ты — автономный агент, управляющий компьютером Windows и телефоном Android как живой человек.
Ты работаешь строго по циклу ReAct: Thought → Action → Observation. За один ответ — РОВНО ОДНО действие, не забегая вперёд.

Правила мышления (thought):
1. Сначала ответь себе: чего хочет пользователь, какие есть риски, какие шаги нужны и почему именно этот шаг сейчас.
2. Опирайся на «ПОСЛЕДНИЕ ШАГИ»: там твои прошлые Thought/Action/Observation. Не повторяй то, что уже не сработало.
3. observation заполняй ТОЛЬКО тем, что реально есть в описании экрана. Запрещено придумывать пути к файлам, имена кнопок, ссылки и вывод команд. Если чего-то нет — так и напиши «этого на экране нет».
4. confidence — честная оценка 0..1. Если ниже 0.8, НЕ действуй: верни action ask_user (или заполни поле question) и спроси у человека то, что мешает быть уверенным.
5. Предпочитай click_element (по имени элемента) вместо click_xy — это надёжнее.
6. Если элемента нет — проверь: не нужно ли закрыть баннер/модалку, проскроллить, подождать загрузку, сменить окно.
7. Если нужен код из SMS/приложения, 2FA или решение человека — action ask_user.
8. Если не хватает инструмента или доступа (нет ADB, нет приложения, нет аккаунта) — action need_tool с конкретной просьбой. Не отказывайся от задачи.
9. Важные факты о сайте/приложении сохраняй через action remember.
10. subtask_done — только когда критерий «ГОТОВО КОГДА» буквально виден на экране (фраза в кавычках или число из сравнения). Слова отчёта узел не закрывают.
11. finish задачу не завершает. Она закроется сама, когда каждый нужный узел подтверждён проверкой экрана. Отчёт «готово» без этого будет отклонён.

Отвечай СТРОГО JSON:
{"observation":"что вижу на экране (только факты оттуда)","thought":"рассуждение: цель, риски, почему этот шаг","confidence":0.0-1.0,"question":"уточняющий вопрос, если confidence<0.8, иначе пусто","next":{"action":"click_element","name":"Войти"}}

Доступные action: click_element{name,control_type}, click_xy{x,y,button,double}, type_text{text,press_enter},
key_combo{combo}, scroll{clicks,horizontal}, drag{x1,y1,x2,y2}, open_url{url}, launch_app{path,args},
focus_window{title_contains}, wait{seconds,reason}, remember{title,body,importance}, ask_user{question,secret},
adb_tap{x,y}, adb_swipe{x1,y1,x2,y2,ms}, adb_text{text}, adb_key{keycode}, adb_open_app{package}, adb_shell{cmd},
subtask_done{result}, finish{report}, need_tool{what,why,how_to_get}"#;

/// Что агент делает прямо сейчас: узел графа или откат ранее выполненного узла.
enum Work {
    Node {
        id: String,
        goal: String,
        done_when: String,
    },
    Rollback {
        id: String,
        instruction: String,
    },
}

impl Work {
    fn goal(&self) -> &str {
        match self {
            Work::Node { goal, .. } => goal,
            Work::Rollback { instruction, .. } => instruction,
        }
    }
}

pub struct Agent {
    llm: LlmClient,
    mem: Arc<Mutex<Memory>>,
    exec: Executor,
    cfg: AgentConfig,
    tx: Sender<AgentEvent>,
    rx: Receiver<AgentCommand>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    /// JEV: вероятностный да/нет-фильтр на статистике шагов.
    /// Если OpenRouter недоступен, остаётся только он.
    jev: crate::jev::Jev,
    /// Быстрый выбор ветки через OpenRouter. Ключ наружу не пишется.
    decisions: crate::decisions::Client,
    optimizer: crate::optimize::Kind,
    checkpoints: crate::checkpoint::Store,
    hang: Duration,
    jev_warned: bool,
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
            jev: crate::jev::Jev::new(),
            decisions: crate::decisions::Client::from_env(),
            optimizer: crate::optimize::Kind::from_env(),
            checkpoints: crate::checkpoint::Store::open(crate::checkpoint::default_dir())
                .or_else(|_| {
                    crate::checkpoint::Store::open(std::env::temp_dir().join("pcagent-checkpoints"))
                })
                .expect("каталог чекпоинтов"),
            hang: hang_limit(),
            jev_warned: false,
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
    /// Обёртка watchdog: любой выход из задачи (успех, ошибка, паника) помечает
    /// её статус в БД. Жёсткая смерть процесса оставляет 'running' — и это
    /// единственный случай, который следующий запуск видит как «брошено».
    pub fn run_task(&mut self, task: &str) -> Result<TaskReport> {
        let task_id = format!("t{}", chrono::Utc::now().timestamp_millis());
        if let Ok(m) = self.mem.lock() {
            let _ = m.task_start(&task_id, task);
        }
        let result = self.run_task_inner(task, &task_id, false);
        self.finish_task(&task_id, result)
    }

    /// Продолжение после падения или зависания. Новый id не заводим:
    /// в базе задача всё ещё `running`, снимок лежит под старым id.
    pub fn resume_task(&mut self, task_id: &str, task: &str) -> Result<TaskReport> {
        self.log(format!("Watchdog: продолжаю задачу {task_id}"));
        let result = self.run_task_inner(task, task_id, true);
        self.finish_task(task_id, result)
    }

    fn finish_task(&mut self, task_id: &str, result: Result<TaskReport>) -> Result<TaskReport> {
        if let Ok(m) = self.mem.lock() {
            // Остановка человеком — не «успех». Закрытие пишем только когда
            // Z3 принял решение; отчёт модели такого статуса не даёт.
            let status = if self.stop.load(std::sync::atomic::Ordering::Relaxed) {
                "interrupted"
            } else {
                match &result {
                    Ok(r) if r.chat => "chat",
                    Ok(r) if r.closed => "done",
                    Ok(_) => "incomplete",
                    Err(_) => "failed",
                }
            };
            let _ = m.task_finish(task_id, status);
        }
        result
    }

    fn run_task_inner(&mut self, task: &str, task_id: &str, resume: bool) -> Result<TaskReport> {
        let started = Instant::now();
        self.log(format!("Задача: {task}"));

        let resumed = if resume {
            match self.checkpoints.load_latest(task_id)? {
                Some(snap) => {
                    self.log(format!(
                        "Снимок шага {} найден, живое состояние подменю им",
                        snap.step
                    ));
                    Some(snap)
                }
                None => {
                    return Ok(TaskReport::open(
                        "Чекпоинта нет, продолжать оборванную задачу не из чего",
                    ));
                }
            }
        } else {
            None
        };

        // 0. Человек мог просто поздороваться или спросить — тогда отвечаем
        // словами, а не двигаем мышью по случайному плану.
        if resumed.is_none() {
            if let Some(reply) = planner::chat_reply(&self.llm, task) {
                self.log(reply.clone());
                return Ok(TaskReport::chat(reply));
            }
        }

        // 1. Поднимаем прошлый опыт ДО планирования.
        let hints = if resumed.is_some() {
            String::new()
        } else {
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

        // 2. Граф задач. Если модель не смогла его собрать — одна цель,
        // критерий которой состоит из слов самой задачи, а не из выдуманной
        // фразы «цель видна выполненной».
        let mut graph = if let Some(snap) = &resumed {
            snap.graph.clone()
        } else {
            match planner::make_graph(&self.llm, task, &hints) {
                Ok(g) => g,
                Err(e) => {
                    self.log(format!(
                        "Планировщик не дал валидный граф ({e}), иду одной целью"
                    ));
                    let mut g = TaskGraph::linear(task, &[(task.to_string(), task.to_string())]);
                    if let Err(err) = g.validate() {
                        return Ok(TaskReport::open(format!(
                            "План не построен: {e}. Запасной план тоже не прошёл проверку: {err}"
                        )));
                    }
                    g
                }
            }
        };
        self.send_plan(&graph);
        self.log(format!("Граф из {} узлов", graph.nodes.len()));

        // Z3 отклоняет план целиком. Подменять его одной размытой целью
        // нельзя: так запрещённый или цикличный план всё равно исполнялся.
        if let crate::logic::Verdict::Violation(why) =
            crate::logic::check_plan(&graph, &self.cfg.policies)
        {
            self.log(format!("Z3 отклонил план: {why}"));
            return Ok(TaskReport::open(format!(
                "План отклонён проверкой Z3: {why}"
            )));
        }
        if resumed.is_none() {
            self.order_plan(&mut graph);
        }

        let mut criteria = resumed
            .as_ref()
            .map(|s| s.criteria.clone())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| crate::acceptance::decompose(task));
        if criteria.is_empty() {
            self.log("В задаче нет измеримого критерия приёмки: она останется незакрытой");
        } else {
            self.log(format!("Критерии приёмки: {}", criteria.join(" | ")));
        }

        let mut trace = Trace::default();
        if let Some(snap) = &resumed {
            trace.from_steps(snap.trace.clone());
        }
        let mut step: u32 = resumed.as_ref().map(|s| s.step).unwrap_or(0);
        let mut touched: Vec<std::path::PathBuf> = resumed
            .as_ref()
            .map(|s| s.touched.iter().map(std::path::PathBuf::from).collect())
            .unwrap_or_default();
        let mut observed: HashMap<String, bool> = resumed
            .as_ref()
            .map(|s| s.observed.iter().map(|(k, v)| (k.clone(), *v)).collect())
            .unwrap_or_default();
        let mut failures: Vec<String> = vec![];
        let mut rollback_queue: Vec<(String, String)> = vec![];
        let mut rollback_steps: u32 = 0;
        let mut need_replan = false;
        let mut pending_lesson: Option<String> = None;
        let mut phone_mode = looks_like_phone_task(task);
        // Экран на старте отката: закрываем откат только если он изменился.
        let mut rollback_origin: HashMap<String, Evidence> = HashMap::new();
        if phone_mode {
            self.log("Задача про телефон — смотрю экран Android");
        }

        let beat_path = self.checkpoints.root().join(format!("{task_id}.beat"));
        let restart = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let _watch = spawn_watch(
            beat_path.clone(),
            self.hang,
            restart.clone(),
            self.stop.clone(),
        );
        if resumed.is_none() {
            if let Err(e) = self.capture_step(
                task_id, task, step, &graph, &observed, &touched, &criteria, &trace,
            ) {
                self.log(format!("Стартовый чекпоинт не снят: {e}"));
            }
        }

        while step < self.cfg.max_steps {
            if restart.swap(false, std::sync::atomic::Ordering::Relaxed) {
                self.log("Watchdog: шаг завис, возвращаюсь к последнему чекпоинту");
                if self.checkpoints.load_latest(task_id)?.is_some() {
                    if let Err(e) = self.restore_state(
                        task_id,
                        &mut graph,
                        &mut observed,
                        &mut trace,
                        &mut touched,
                        &mut criteria,
                    ) {
                        return Ok(TaskReport::open(format!(
                            "Watchdog не смог откатиться к чекпоинту: {e}"
                        )));
                    }
                    continue;
                }
            }
            let _ = crate::watchdog::write_beat(
                &beat_path,
                &crate::watchdog::Beat {
                    task_id: task_id.to_string(),
                    step,
                    unix_ms: crate::watchdog::now_ms(),
                },
            );
            if self.stop.load(std::sync::atomic::Ordering::Relaxed) {
                return Ok(TaskReport::open("Остановлено пользователем"));
            }
            if started.elapsed() > Duration::from_secs(self.cfg.max_minutes * 60) {
                return Ok(TaskReport::open(format!(
                    "Стоп по таймауту {} мин. {}",
                    self.cfg.max_minutes,
                    self.finish_report(&graph, &trace)
                )));
            }

            // --- ЧТО ДЕЛАЕМ СЕЙЧАС ---
            // Откат имеет приоритет: пока система не приведена в согласованное
            // состояние, двигаться вперёд нельзя.
            let work = match rollback_queue.last().cloned() {
                Some((id, instruction)) => Work::Rollback { id, instruction },
                None => match self.choose_node(&graph, task) {
                    Some(n) => Work::Node {
                        id: n.id.clone(),
                        goal: n.goal.clone(),
                        done_when: n.done_when.clone(),
                    },
                    None => {
                        if need_replan {
                            need_replan = false;
                            let screen = trace.render();
                            match planner::replan(&self.llm, &graph, &screen, &failures) {
                                Ok(g) => {
                                    self.log("Перепланировал остаток графа");
                                    graph = g;
                                    retain_observed(&graph, &mut observed);
                                    self.send_plan(&graph);
                                    failures.clear();
                                    continue;
                                }
                                Err(e) => {
                                    self.log(format!("Перепланировать не смог: {e}"));
                                }
                            }
                        }
                        return Ok(
                            self.close_if_proven(&graph, &trace, &observed, &criteria, phone_mode)
                        );
                    }
                },
            };
            step += 1;
            if matches!(work, Work::Rollback { .. }) {
                rollback_steps += 1;
                if rollback_steps > MAX_ROLLBACK_STEPS {
                    if let Some((id, instr)) = rollback_queue.pop() {
                        self.log(format!("Откат «{instr}» не удался, оставляю как есть"));
                        graph.mark_rolled_back(&id);
                    }
                    rollback_steps = 0;
                    continue;
                }
            }

            // --- ВОСПРИЯТИЕ (Observation для предыдущего шага уже записан) ---
            let need_shot = !failures.is_empty();
            let view = match self.perceive(phone_mode, need_shot) {
                Ok(v) => v,
                Err(PerceiveError::PhoneGone(e)) => {
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
                Err(PerceiveError::ScreenGone(e)) => {
                    self.log(format!("Не вижу экран: {e}"));
                    std::thread::sleep(Duration::from_secs(1));
                    continue;
                }
            };
            let ctx = view.ctx.clone();
            let shelf = Memory::shelf_for(&ctx);
            let ev_before = Evidence::from_prompt(&view.prompt);
            if let Work::Rollback { id, .. } = &work {
                rollback_origin
                    .entry(id.clone())
                    .or_insert_with(|| ev_before.clone());
            }

            // --- ПАМЯТЬ: только нужная полка ---
            let (shelf_notes, lessons, digest) = {
                let m = self.mem_lock();
                let notes = m
                    .recall(&shelf, work.goal(), 6)?
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
            let mut prompt = match &work {
                Work::Node {
                    id,
                    goal,
                    done_when,
                } => format!(
                    "{}\n\nТЕКУЩИЙ УЗЕЛ {}: {}\nГОТОВО КОГДА: {}\n\n{}\n",
                    graph.to_prompt(),
                    id,
                    goal,
                    done_when,
                    view.prompt
                ),
                Work::Rollback { id, instruction } => format!(
                    "{}\n\nСЕЙЧАС ОТКАТ узла {}: {}\nДальше по плану идти нельзя, пока это не отменено. Когда откат виден выполненным — action subtask_done.\n\n{}\n",
                    graph.to_prompt(),
                    id,
                    instruction,
                    view.prompt
                ),
            };
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
            let tail = trace.render();
            if !tail.is_empty() {
                prompt.push_str(&format!("\n{tail}"));
            }
            if !failures.is_empty() {
                prompt.push_str(&format!(
                    "\nПРОБЛЕМЫ НА ЭТОМ УЗЛЕ:\n- {}\n",
                    failures.join("\n- ")
                ));
            }

            let msg = match &view.screenshot_png {
                Some(png) => Message::user_with_image(prompt, png),
                None => Message::user(prompt),
            };
            // Провайдер может ответить 429/5xx — это не повод терять шаг.
            // with_timeout сверху: зависший сокет не должен останавливать агента
            // навсегда, даже если HTTP-таймаут почему-то не сработал.
            let resp = match retry(&Backoff::default(), "llm act", |_| {
                let llm = self.llm.clone();
                let m = msg.clone();
                crate::retry::with_timeout(Duration::from_secs(200), "llm act", move || {
                    llm.complete(&[Message::system(ACT_SYSTEM), m], "act", true, 0.2)
                })
            }) {
                Ok(r) => r,
                Err(e) => {
                    // {e:#} — вся цепочка причин: без неё в логе оставалось
                    // одно «не вышло за 3 попыток» без слова о причине.
                    self.log(format!("LLM недоступна: {e:#}"));
                    std::thread::sleep(Duration::from_secs(3));
                    continue;
                }
            };
            let decision: Decision =
                match extract_json(&resp.text).and_then(|v| Ok(serde_json::from_value(v)?)) {
                    Ok(d) => d,
                    Err(e) => {
                        self.log(format!("Модель вернула не то ({e}), переспрашиваю"));
                        failures.push(format!("ответ модели не разобран: {e}"));
                        continue;
                    }
                };
            let _ = self.tx.send(AgentEvent::Thought(decision.thought.clone()));
            let act_str = decision.next.short();

            // --- ЛОГИКА: политика Z3 + вероятностный JEV ДО действия ---
            if let crate::logic::Verdict::Violation(why) =
                crate::logic::gate_action(&act_str, &self.cfg.policies)
            {
                self.log(format!("Шаг отклонён логикой: {why}"));
                failures.push(why);
                continue;
            }
            if self.decisions.enabled() {
                let state = serde_json::json!({
                    "action": act_str,
                    "goal": work.goal(),
                    "failures": failures,
                });
                match self.decisions.yes(&state, "Делать ли это действие сейчас?")
                {
                    Ok(false) => {
                        self.log("JEV: действие сейчас не подходит, пропускаю");
                        failures.push("jev: noul отказал в действии".into());
                        continue;
                    }
                    Ok(true) => {}
                    Err(e) => self.warn_jev(&e.to_string()),
                }
            }
            match self.jev.ask(&act_str) {
                crate::jev::JevVerdict::No => {
                    self.log(format!(
                        "JEV: класс действия «{}» стабильно проваливается — пропускаю",
                        act_str
                    ));
                    failures.push("jev: плохая статистика класса действия".into());
                    continue;
                }
                _ => {}
            }

            // --- GROUNDING: отсекаем выдумки ДО действия ---
            let claims = format!("{} {}", decision.observation, decision.thought);
            let violations = grounding::check_claims(&claims, &ev_before, task, true);
            if !violations.is_empty() {
                let why = violations
                    .iter()
                    .map(|v| v.human())
                    .collect::<Vec<_>>()
                    .join("; ");
                self.log(format!("Отклонил шаг: не подтверждено наблюдением — {why}"));
                trace.push(Step {
                    n: step,
                    thought: decision.thought.clone(),
                    action: act_str.clone(),
                    observation: format!(
                        "ОТКЛОНЕНО: ты сослался на то, чего нет на экране ({why}). Смотри только на описание экрана."
                    ),
                    confidence: decision.confidence,
                    verified: Some(false),
                });
                failures.push(format!("выдуманные сущности: {why}"));
                continue;
            }

            // --- UNCERTAINTY: не уверен — спрашивай, а не жми ---
            let asks_human = matches!(
                decision.next,
                Action::AskUser { .. } | Action::NeedTool { .. }
            );
            if let Gate::Clarify(q) = react::confidence_gate(
                decision.confidence,
                asks_human,
                &decision.question,
                &human_readable(&decision.next),
            ) {
                self.log(format!(
                    "Уверенность {:.0}% — уточняю у человека",
                    decision.confidence.max(0.0) * 100.0
                ));
                let ans = self.ask_human(&q, false);
                trace.push(Step {
                    n: step,
                    thought: decision.thought.clone(),
                    action: format!("уточняющий вопрос: {q}"),
                    observation: format!("человек ответил: {ans}"),
                    confidence: decision.confidence,
                    verified: None,
                });
                continue;
            }

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
            if trace.repeats_of(&act_str) >= 2 {
                failures.push(
                    "ты повторяешь одно и то же действие без результата — смени подход".into(),
                );
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

            // --- ЧЕКПОИНТ ДО ШАГА ---
            // Нет целого снимка — нет действия. Иначе откатывать будет некуда.
            for path in paths_from(&decision.next) {
                if !touched.iter().any(|have| have == &path) {
                    touched.push(path);
                }
            }
            if let Err(e) = self.capture_step(
                task_id, task, step, &graph, &observed, &touched, &criteria, &trace,
            ) {
                self.log(format!("Чекпоинт не снят, шаг не делаю: {e}"));
                failures.push(format!("чекпоинт: {e}"));
                continue;
            }

            // --- ДЕЙСТВИЕ ---
            self.log(format!("Шаг {step}: {}", human_readable(&decision.next)));
            let expectation = verify::expectation_of(&decision.next);
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
                    // --- SELF-VERIFY: смотрим тем же инструментом ---
                    let (verdict, mut observation) =
                        self.observe_and_verify(phone_mode, &expectation, &ev_before, &msg);
                    if verdict.is_refuted() {
                        let why =
                            format!("{}: {}", human_readable(&decision.next), verdict.human());
                        failures.push(why.clone());
                        if let Err(e) = self.restore_state(
                            task_id,
                            &mut graph,
                            &mut observed,
                            &mut trace,
                            &mut touched,
                            &mut criteria,
                        ) {
                            return Ok(TaskReport::open(format!(
                                "Шаг не подтверждён ({why}), полный откат не удался: {e}"
                            )));
                        }
                    } else {
                        if let Some(sig) = pending_lesson.take() {
                            let _ = self.mem_lock().mark_lesson_worked(&sig);
                        }
                        failures.clear();
                    }
                    observation.push_str(&format!(" | {}", verdict.human()));
                    trace.push(Step {
                        n: step,
                        thought: decision.thought.clone(),
                        action: act_str.clone(),
                        observation,
                        confidence: decision.confidence,
                        verified: Some(verdict.is_confirmed()),
                    });
                    self.jev.observe(&act_str, !verdict.is_refuted());
                    self.mem_lock().log_step(
                        &task_id,
                        step as i64,
                        &act_str,
                        &msg,
                        !verdict.is_refuted(),
                    )?;
                }
                Ok(Outcome::SubtaskDone(res)) => {
                    match &work {
                        Work::Rollback { id, instruction } => {
                            let proven = self.rollback_visible(
                                phone_mode,
                                instruction,
                                rollback_origin.get(id),
                            );
                            if proven {
                                self.log(format!("Откат виден на экране: {instruction}"));
                                graph.mark_rolled_back(id);
                                observed.remove(id);
                                rollback_queue.pop();
                                rollback_steps = 0;
                                need_replan = true;
                            } else {
                                self.log(format!(
                                    "Не закрываю откат {id}: экран его не подтвердил"
                                ));
                                failures.push("откат не подтверждён наблюдением экрана".into());
                            }
                            trace.push(Step {
                                n: step,
                                thought: decision.thought.clone(),
                                action: act_str.clone(),
                                observation: if proven {
                                    format!("откат узла {id} подтверждён экраном")
                                } else {
                                    format!(
                                        "откат узла {id} НЕ подтверждён. Модель написала: {res}"
                                    )
                                },
                                confidence: decision.confidence,
                                verified: Some(proven),
                            });
                        }
                        Work::Node { id, done_when, .. } => {
                            // Слова subtask_done не закрывают узел. Сначала
                            // свежий экран, потом Z3: критерий наблюдён и
                            // зависимости уже выполнены.
                            let (met, why) = self.criterion_on_screen(phone_mode, done_when);
                            let verdict = crate::logic::allow_node_done(&graph, id, met);
                            let ok = verdict == crate::logic::Verdict::Ok;
                            if ok {
                                self.log(format!("Узел {id} закрыт проверкой Z3"));
                                graph.mark_done(id, &why);
                                observed.insert(id.clone(), true);
                                self.send_plan(&graph);
                                failures.clear();
                            } else {
                                let msg = match verdict {
                                    crate::logic::Verdict::Violation(v) if met => v,
                                    _ => why.clone(),
                                };
                                self.log(format!("Не закрываю {id}: {msg}"));
                                failures.push(format!("критерий готовности не подтверждён: {msg}"));
                            }
                            trace.push(Step {
                                n: step,
                                thought: decision.thought.clone(),
                                action: act_str.clone(),
                                observation: if ok {
                                    format!("узел {id} подтверждён экраном: {why}")
                                } else {
                                    format!("узел {id} НЕ подтверждён. Модель написала: {res}")
                                },
                                confidence: decision.confidence,
                                verified: Some(ok),
                            });
                        }
                    }
                }
                Ok(Outcome::Finished(report)) => {
                    // Отчёт не является сертификатом. Если решение уже доказано,
                    // наружу уходит отчёт по узлам, а не текст модели.
                    match crate::logic::check_solution(&graph, &observed) {
                        crate::logic::Verdict::Ok => {
                            self.log(
                                "Задача закрыта: Z3 подтвердил решение, текст finish не использован как доказательство",
                            );
                            let proven = self
                                .close_if_proven(&graph, &trace, &observed, &criteria, phone_mode);
                            let m = self.mem_lock();
                            let note = crate::text::truncate(&proven.text, 400);
                            m.remember(
                                &shelf,
                                "outcome",
                                &format!("Задача: {task}"),
                                &note,
                                &note,
                                0.7,
                            )?;
                            m.set_shelf_digest(&shelf, &note)?;
                            drop(m);
                            return Ok(proven);
                        }
                        crate::logic::Verdict::Violation(why) => {
                            let claim = grounding::report_is_grounded(&report)
                                .err()
                                .unwrap_or_else(|| "finish без доказанного результата".into());
                            self.log(format!("Отклонил finish: {claim}. {why}"));
                            failures.push(format!(
                                "{claim}. {why}. Сделай шаг и добейся, чтобы критерий был виден на экране"
                            ));
                            trace.push(Step {
                                n: step,
                                thought: decision.thought.clone(),
                                action: act_str.clone(),
                                observation: format!("finish отклонён: {why}"),
                                confidence: decision.confidence,
                                verified: Some(false),
                            });
                            continue;
                        }
                    }
                }
                Ok(Outcome::NeedHuman { question, secret }) => {
                    let ans = self.ask_human(&question, secret);
                    // Секрет НЕ попадает ни в трассу, ни в промпт, ни в логи.
                    // Он сразу печатается в поле на экране.
                    let observation = if secret {
                        let _ = crate::platform::type_text(&ans);
                        "код введён (значение не сохраняется)".to_string()
                    } else {
                        format!("человек ответил: {ans}")
                    };
                    trace.push(Step {
                        n: step,
                        thought: decision.thought.clone(),
                        action: format!("спросил человека: {question}"),
                        observation,
                        confidence: decision.confidence,
                        verified: None,
                    });
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
                    trace.push(Step {
                        n: step,
                        thought: decision.thought.clone(),
                        action: format!("запросил инструмент {what}"),
                        observation: format!("человек ответил: {ans}"),
                        confidence: decision.confidence,
                        verified: None,
                    });
                }
                Err(e) => {
                    let err = e.to_string();
                    self.log(format!("Ошибка: {err}"));
                    // Сначала целиком назад, и только потом разбор. Иначе
                    // заметка об ошибке пишется в уже испорченную память.
                    if let Err(re) = self.restore_state(
                        task_id,
                        &mut graph,
                        &mut observed,
                        &mut trace,
                        &mut touched,
                        &mut criteria,
                    ) {
                        return Ok(TaskReport::open(format!(
                            "Сбой «{err}». Полный откат к чекпоинту не удался, дальше состояние не меняю: {re}"
                        )));
                    }
                    failures.push(err.clone());
                    self.mem_lock()
                        .log_step(task_id, step as i64, &act_str, &err, false)?;
                    trace.push(Step {
                        n: step,
                        thought: decision.thought.clone(),
                        action: act_str.clone(),
                        observation: format!("ошибка: {err}"),
                        confidence: decision.confidence,
                        verified: Some(false),
                    });
                    let cause = crate::failure::analyze(&trace.render(), &err);
                    self.log(format!("Причина сбоя: {}", cause.summary));

                    let screen = view.prompt.clone();
                    match reflection::analyze(
                        &self.llm,
                        &self.mem_lock(),
                        &ctx,
                        work.goal(),
                        &act_str,
                        &err,
                        &screen,
                    ) {
                        Ok(lesson) => {
                            self.log(format!("Разбор: {} → {}", lesson.cause, lesson.fix));
                            pending_lesson = Some(lesson.signature.clone());
                            failures.push(format!("вывод из ошибки: {}", lesson.fix));
                            if !lesson.retry_hint.is_empty() {
                                failures.push(format!("пробуй: {}", lesson.retry_hint));
                            }
                        }
                        Err(e2) => self.log(format!("Не смог разобрать ошибку: {e2}")),
                    }

                    if let Work::Node { id, .. } = &work {
                        if graph.mark_attempt_failed(id, &cause.summary) {
                            self.log(format!(
                                "Узел {id} исчерпал попытки. Состояние уже на чекпоинте, перепланирую остаток"
                            ));
                            match crate::failure::replan_remaining(&graph, &cause) {
                                Ok(g) => {
                                    self.accept_replan(&mut graph, &mut observed, g, &mut failures);
                                }
                                Err(e2) => {
                                    self.log(format!("Локальный переплан не вышел: {e2}"));
                                    if let Ok(g) =
                                        planner::replan(&self.llm, &graph, &screen, &failures)
                                    {
                                        self.accept_replan(
                                            &mut graph,
                                            &mut observed,
                                            g,
                                            &mut failures,
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(TaskReport::open(format!(
            "Достигнут лимит {} шагов. {}",
            self.cfg.max_steps,
            self.finish_report(&graph, &trace)
        )))
    }

    /// Свежий экран против критерия. Вторая половина ответа — короткая
    /// выжимка того, что именно увидели, чтобы положить её в узел.
    fn criterion_on_screen(&self, phone_mode: bool, done_when: &str) -> (bool, String) {
        let view = match self.perceive(phone_mode, false) {
            Ok(v) => v,
            Err(PerceiveError::PhoneGone(e)) | Err(PerceiveError::ScreenGone(e)) => {
                return (false, format!("экран не читается: {e}"));
            }
        };
        let ev = Evidence::from_prompt(&view.prompt);
        let met = verify::criterion_observed(done_when, &ev);
        let seen = crate::text::truncate(&view.prompt, 160);
        if met {
            (true, format!("критерий «{done_when}» виден: {seen}"))
        } else {
            (false, format!("на экране нет критерия «{done_when}»"))
        }
    }

    /// Откат закрывается по экрану. Если в инструкции есть кавычки или
    /// сравнение — они тоже обязаны выполниться. Иначе достаточно того,
    /// что экран изменился с момента начала отката: «удалить файл» часто
    /// видно как отсутствие, а не как новая надпись.
    fn rollback_visible(
        &self,
        phone_mode: bool,
        instruction: &str,
        origin: Option<&Evidence>,
    ) -> bool {
        let view = match self.perceive(phone_mode, false) {
            Ok(v) => v,
            Err(_) => return false,
        };
        let after = Evidence::from_prompt(&view.prompt);
        let before = origin.cloned().unwrap_or_default();
        let changed =
            verify::verify(&verify::Expectation::ScreenChanged, &before, &after).is_confirmed();
        if verify::has_checkable_literal(instruction) {
            changed && verify::criterion_observed(instruction, &after)
        } else {
            changed
        }
    }

    fn close_if_proven(
        &self,
        graph: &TaskGraph,
        trace: &Trace,
        observed: &HashMap<String, bool>,
        criteria: &[String],
        phone_mode: bool,
    ) -> TaskReport {
        let evidence = match self.perceive(phone_mode, false) {
            Ok(view) => Evidence::from_prompt(&view.prompt),
            Err(_) => Evidence::default(),
        };
        let unmet = crate::acceptance::unmet(criteria, &evidence);
        if !unmet.is_empty() {
            return TaskReport::open(format!(
                "Задача не закрыта: {}\n\n{}",
                unmet.join("; "),
                self.finish_report(graph, trace)
            ));
        }
        match crate::logic::check_solution(graph, observed) {
            crate::logic::Verdict::Ok => TaskReport::closed(self.finish_report(graph, trace)),
            crate::logic::Verdict::Violation(why) => TaskReport::open(format!(
                "Задача не закрыта: {why}\n\n{}",
                self.finish_report(graph, trace)
            )),
        }
    }

    fn order_plan(&self, graph: &mut TaskGraph) {
        let draft = crate::optimize::problem_from_graph(
            graph,
            crate::optimize::Limits {
                max_time: 1,
                max_cost: 1,
                max_risk: 1,
            },
        );
        let problem =
            crate::optimize::problem_from_graph(graph, crate::optimize::loose_limits(&draft));
        match crate::optimize::solve(self.optimizer, &problem) {
            Ok(sol) => {
                let mut ordered = graph.clone();
                crate::optimize::apply_solution(&mut ordered, &sol);
                match crate::logic::check_plan(&ordered, &self.cfg.policies) {
                    crate::logic::Verdict::Ok => {
                        self.log(format!(
                            "Порядок шагов выбрал {}: {}",
                            sol.solver,
                            sol.order.join(" → ")
                        ));
                        *graph = ordered;
                    }
                    crate::logic::Verdict::Violation(why) => {
                        self.log(format!(
                            "Z3 отклонил порядок оптимизатора ({why}), оставляю исходный план"
                        ));
                    }
                }
            }
            Err(e) => self.log(format!("Оптимизатор порядка недоступен: {e}")),
        }
    }

    fn accept_replan(
        &self,
        graph: &mut TaskGraph,
        observed: &mut HashMap<String, bool>,
        next: TaskGraph,
        failures: &mut Vec<String>,
    ) {
        match crate::logic::check_plan(&next, &self.cfg.policies) {
            crate::logic::Verdict::Ok => {
                self.log("Остаток плана заново прошёл Z3");
                *graph = next;
                retain_observed(graph, observed);
                self.send_plan(graph);
                failures.clear();
            }
            crate::logic::Verdict::Violation(why) => {
                self.log(format!("Z3 отклонил перепланированный остаток: {why}"));
            }
        }
    }

    fn choose_node(&mut self, graph: &TaskGraph, task: &str) -> Option<graph::Node> {
        let ready: Vec<(String, String)> = graph
            .ready()
            .iter()
            .map(|n| (n.id.clone(), n.goal.clone()))
            .collect();
        if ready.len() > 1 && self.decisions.enabled() {
            let state = serde_json::json!({ "task": task });
            match self
                .decisions
                .choice(&state, "Какой оставшийся узел делать сейчас?", &ready)
            {
                Ok(choice) => {
                    if let Some(n) = graph.nodes.iter().find(|n| n.id == choice.id) {
                        return Some(n.clone());
                    }
                }
                Err(e) => self.warn_jev(&e.to_string()),
            }
        }
        graph.next().cloned()
    }

    fn warn_jev(&mut self, text: &str) {
        if self.jev_warned {
            return;
        }
        self.jev_warned = true;
        self.log(format!(
            "JEV OpenRouter недоступен, решаю локальной статистикой: {text}"
        ));
    }

    fn capture_step(
        &self,
        task_id: &str,
        task: &str,
        step: u32,
        graph: &TaskGraph,
        observed: &HashMap<String, bool>,
        touched: &[std::path::PathBuf],
        criteria: &[String],
        trace: &Trace,
    ) -> Result<()> {
        let files = crate::checkpoint::read_files(touched)?;
        let (window_id, window_title) =
            crate::platform::foreground_window().unwrap_or((0, String::new()));
        let snap = crate::checkpoint::Snapshot {
            task_id: task_id.to_string(),
            task: task.to_string(),
            step,
            graph: graph.clone(),
            observed: observed.iter().map(|(k, v)| (k.clone(), *v)).collect(),
            window_id,
            window_title,
            files,
            criteria: criteria.to_vec(),
            trace: trace.to_steps(),
            touched: touched.iter().map(|p| p.display().to_string()).collect(),
        };
        let mem = self.mem_lock();
        self.checkpoints.save(&snap, &mem)?;
        Ok(())
    }

    fn restore_state(
        &mut self,
        task_id: &str,
        graph: &mut TaskGraph,
        observed: &mut HashMap<String, bool>,
        trace: &mut Trace,
        touched: &mut Vec<std::path::PathBuf>,
        criteria: &mut Vec<String>,
    ) -> Result<()> {
        let mut mem = self.mem_lock();
        let snap = self
            .checkpoints
            .restore_latest(task_id, &mut mem, &mut |id, _title| {
                if id == 0 {
                    return Ok(());
                }
                crate::platform::focus_window(id).map_err(|e| e.to_string())
            })?;
        drop(mem);
        *graph = snap.graph;
        *observed = snap.observed.into_iter().collect();
        trace.from_steps(snap.trace);
        *touched = snap.touched.iter().map(std::path::PathBuf::from).collect();
        if !snap.criteria.is_empty() {
            *criteria = snap.criteria;
        }
        Ok(())
    }

    /// Восприятие ПК или телефона одним вызовом: цикл выше не должен знать,
    /// где именно сейчас «руки» агента.
    fn perceive(
        &self,
        phone_mode: bool,
        need_shot: bool,
    ) -> std::result::Result<View, PerceiveError> {
        if phone_mode {
            return self
                .perceive_phone(need_shot)
                .map_err(|e| PerceiveError::PhoneGone(e.to_string()));
        }
        match perception::perceive(need_shot, &self.cfg.ocr_lang) {
            Ok(o) => Ok(View {
                ctx: o.memory_context(),
                prompt: o.to_prompt(),
                screenshot_png: o.screenshot_png,
            }),
            Err(e) => Err(PerceiveError::ScreenGone(e.to_string())),
        }
    }

    /// Observation + self-verify: смотрим на экран ТЕМ ЖЕ инструментом, что и
    /// до действия, и сверяем с ожиданием. Скриншот не запрашиваем — проверка
    /// не должна стоить дороже самого действия.
    fn observe_and_verify(
        &self,
        phone_mode: bool,
        expectation: &verify::Expectation,
        before: &Evidence,
        tool_output: &str,
    ) -> (verify::Verdict, String) {
        match self.perceive(phone_mode, false) {
            Ok(v) => {
                let mut after = Evidence::from_prompt(&v.prompt);
                // Вывод инструмента — тоже законное наблюдение (adb shell и т.п.).
                after.add_tool_output(tool_output);
                let verdict = verify::verify(expectation, before, &after);
                let head: String = v
                    .prompt
                    .lines()
                    .take(6)
                    .collect::<Vec<_>>()
                    .join(" / ")
                    .chars()
                    .take(300)
                    .collect();
                (verdict, format!("{tool_output}; экран: {head}"))
            }
            Err(e) => (
                verify::Verdict::Unknown(match e {
                    PerceiveError::PhoneGone(s) | PerceiveError::ScreenGone(s) => s,
                }),
                tool_output.to_string(),
            ),
        }
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
                Ok(AgentCommand::Start(_)) | Ok(AgentCommand::Resume { .. }) => {}
                Err(_) => continue,
            }
        }
    }

    fn send_plan(&self, graph: &TaskGraph) {
        let items = graph
            .nodes
            .iter()
            .map(|n| (n.goal.clone(), n.state == graph::NodeState::Done))
            .collect();
        let _ = self.tx.send(AgentEvent::Plan(items));
    }

    /// Отчёт строится из подтверждённых шагов, а не из обещаний модели.
    fn finish_report(&self, graph: &TaskGraph, trace: &Trace) -> String {
        let (done, total) = graph.progress();
        let failed = graph.failed();
        let mut s = format!(
            "Закрыто {done}/{total} узлов графа. Шагов в памяти: {}, из них подтверждённых: {}.\n",
            trace.len(),
            trace.verified_count()
        );
        if !failed.is_empty() {
            s.push_str("Не получилось:\n");
            for n in failed {
                s.push_str(&format!("- {}: {}\n", n.goal, n.note));
            }
        }
        s.push_str(&trace.render());
        s
    }
}

/// Почему не удалось увидеть мир. Разделение важно: пропавший телефон лечится
/// просьбой к человеку, а сорванное чтение экрана — просто повтором.
enum PerceiveError {
    PhoneGone(String),
    ScreenGone(String),
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
        Action::TypeText { text, .. } => format!("печатаю {} символов", text.chars().count()),
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

fn hang_limit() -> Duration {
    let secs = std::env::var("PCAGENT_HANG_SEC")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(30);
    Duration::from_secs(secs.max(5))
}

fn paths_from(action: &Action) -> Vec<std::path::PathBuf> {
    match action {
        Action::LaunchApp { path, args } => {
            let mut out = crate::checkpoint::paths_of_action(path);
            for tok in args.split_whitespace() {
                out.extend(crate::checkpoint::paths_of_action(tok));
            }
            out
        }
        _ => Vec::new(),
    }
}

struct WatchStop {
    flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Drop for WatchStop {
    fn drop(&mut self) {
        self.flag.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn spawn_watch(
    beat_path: std::path::PathBuf,
    hang: Duration,
    restart: std::sync::Arc<std::sync::atomic::AtomicBool>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> WatchStop {
    let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_watch = flag.clone();
    let handle = std::thread::spawn(move || {
        while !stop_watch.load(std::sync::atomic::Ordering::Relaxed)
            && !stop.load(std::sync::atomic::Ordering::Relaxed)
        {
            std::thread::sleep(Duration::from_secs(1));
            if let Some(beat) = crate::watchdog::read_beat(&beat_path) {
                if crate::watchdog::assess(true, Some(&beat), crate::watchdog::now_ms(), hang)
                    == crate::watchdog::Watch::Hung
                {
                    restart.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
    });
    WatchStop {
        flag,
        handle: Some(handle),
    }
}

fn retain_observed(graph: &TaskGraph, observed: &mut HashMap<String, bool>) {
    observed.retain(|id, ok| {
        *ok && graph
            .nodes
            .iter()
            .any(|n| n.id == *id && n.state == graph::NodeState::Done)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phone_tasks_are_detected_but_pc_tasks_are_not() {
        assert!(looks_like_phone_task("Открой CapCut на телефоне"));
        assert!(looks_like_phone_task("зайди в тикток"));
        assert!(!looks_like_phone_task("настрой рекламу в Ads Manager"));
    }

    /// Сквозная проверка связки «провал узла → откат → отчёт» без сети:
    /// именно эта цепочка спасает систему от полусделанной установки.
    #[test]
    fn failed_node_produces_reverse_rollback_and_honest_report() {
        let mut g = TaskGraph::linear(
            "установить игру",
            &[
                ("скачать".into(), "файл в загрузках".into()),
                ("установить".into(), "ярлык на столе".into()),
            ],
        );
        g.validate().expect("граф валиден");
        g.nodes[0].rollback = "удалить скачанный файл".into();
        g.mark_done("n1", "файл виден");
        // Второй узел исчерпывает попытки и становится терминально провальным.
        let mut terminal = false;
        for _ in 0..graph::MAX_NODE_ATTEMPTS {
            terminal = g.mark_attempt_failed("n2", "установщик не запустился");
        }
        assert!(
            terminal,
            "после лимита попыток узел должен упасть терминально"
        );
        let plan = g.rollback_plan();
        assert_eq!(
            plan.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(),
            vec!["n1".to_string()],
            "откат идёт по выполненным узлам в обратном порядке"
        );

        // Отчёт не имеет права выглядеть успешным: нет ни одного проверенного шага.
        assert!(grounding::report_is_grounded("Всё готово, игра установлена").is_err());
        let observed = std::collections::HashMap::from([("n1".to_string(), true)]);
        assert!(
            matches!(
                crate::logic::check_solution(&g, &observed),
                crate::logic::Verdict::Violation(_)
            ),
            "проваленный граф нельзя закрыть отчётом"
        );
    }
}
