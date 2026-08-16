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
10. subtask_done — только когда критерий «ГОТОВО КОГДА» реально виден на экране. Твой успех проверяется повторным наблюдением: необоснованный subtask_done будет отклонён.
11. finish — только когда достигнута вся цель и это видно на экране.

Отвечай СТРОГО JSON:
{"observation":"что вижу на экране (только факты оттуда)","thought":"рассуждение: цель, риски, почему этот шаг","confidence":0.0-1.0,"question":"уточняющий вопрос, если confidence<0.8, иначе пусто","next":{"action":"click_element","name":"Войти"}}

Доступные action: click_element{name,control_type}, click_xy{x,y,button,double}, type_text{text,press_enter},
key_combo{combo}, scroll{clicks,horizontal}, drag{x1,y1,x2,y2}, open_url{url}, launch_app{path,args},
focus_window{title_contains}, wait{seconds,reason}, remember{title,body,importance}, ask_user{question,secret},
adb_tap{x,y}, adb_swipe{x1,y1,x2,y2,ms}, adb_text{text}, adb_key{keycode}, adb_open_app{package}, adb_shell{cmd},
subtask_done{result}, finish{report}, need_tool{what,why,how_to_get}"#;

const VERIFY_SYSTEM: &str = r#"Ты — независимый проверяющий автономного агента. Тебе дают критерий готовности подзадачи и СВЕЖЕЕ описание экрана.
Твоя работа — не поверить агенту, а проверить по экрану.
Правила:
1. verified=true только если критерий ВИДЕН в описании экрана.
2. В поле evidence процитируй фрагмент описания экрана (дословно), который это подтверждает. Выдумывать цитату запрещено: она сверяется автоматически.
3. Если данных не хватает — verified=false и в why напиши, чего именно не видно.
Отвечай СТРОГО JSON: {"verified":true|false,"evidence":"дословная цитата с экрана","why":"кратко"}"#;

#[derive(Debug, serde::Deserialize)]
struct VerifyAnswer {
    #[serde(default)]
    verified: bool,
    #[serde(default)]
    evidence: String,
    #[serde(default)]
    why: String,
}

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

        // 2. Граф задач. Если модель не смогла его собрать — работаем по одной
        // цели: агент без плана хуже агента с плохим планом.
        let mut graph = match planner::make_graph(&self.llm, task, &hints) {
            Ok(g) => g,
            Err(e) => {
                self.log(format!(
                    "Планировщик не дал валидный граф ({e}), иду одной целью"
                ));
                let mut g = TaskGraph::linear(
                    task,
                    &[(
                        task.to_string(),
                        "цель задачи видна выполненной".to_string(),
                    )],
                );
                g.validate().map_err(anyhow::Error::msg)?;
                g
            }
        };
        self.send_plan(&graph);
        self.log(format!("Граф из {} узлов", graph.nodes.len()));

        let mut trace = Trace::default();
        let mut step: u32 = 0;
        let mut failures: Vec<String> = vec![];
        let mut rollback_queue: Vec<(String, String)> = vec![];
        let mut rollback_steps: u32 = 0;
        let mut need_replan = false;
        let mut pending_lesson: Option<String> = None;
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
                    "Стоп по таймауту {} мин. {}",
                    self.cfg.max_minutes,
                    self.finish_report(&graph, &trace)
                ));
            }

            // --- ЧТО ДЕЛАЕМ СЕЙЧАС ---
            // Откат имеет приоритет: пока система не приведена в согласованное
            // состояние, двигаться вперёд нельзя.
            let work = match rollback_queue.last().cloned() {
                Some((id, instruction)) => Work::Rollback { id, instruction },
                None => match graph.next() {
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
                                    self.send_plan(&graph);
                                    failures.clear();
                                    continue;
                                }
                                Err(e) => {
                                    self.log(format!("Перепланировать не смог: {e}"));
                                }
                            }
                        }
                        return Ok(self.finish_report(&graph, &trace));
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
                        failures.push(format!(
                            "{}: {}",
                            human_readable(&decision.next),
                            verdict.human()
                        ));
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
                            self.log(format!("Откат выполнен: {instruction}"));
                            graph.mark_rolled_back(id);
                            rollback_queue.pop();
                            rollback_steps = 0;
                            need_replan = true;
                            trace.push(Step {
                                n: step,
                                thought: decision.thought.clone(),
                                action: act_str.clone(),
                                observation: format!("откат узла {id} завершён: {res}"),
                                confidence: decision.confidence,
                                verified: Some(true),
                            });
                        }
                        Work::Node {
                            id,
                            goal,
                            done_when,
                        } => {
                            // Do → Verify → Report: закрываем узел только после
                            // независимой проверки критерия по свежему экрану.
                            let (ok, why) = self.verify_node(phone_mode, goal, done_when);
                            if ok {
                                self.log(format!("Узел {id} готов: {res}"));
                                graph.mark_done(id, &res);
                                self.send_plan(&graph);
                                failures.clear();
                            } else {
                                self.log(format!("Не закрываю {id}: {why}"));
                                failures.push(format!("критерий готовности не подтверждён: {why}"));
                            }
                            trace.push(Step {
                                n: step,
                                thought: decision.thought.clone(),
                                action: act_str.clone(),
                                observation: if ok {
                                    format!("узел {id} подтверждён проверкой: {why}")
                                } else {
                                    format!("узел {id} НЕ подтверждён: {why}")
                                },
                                confidence: decision.confidence,
                                verified: Some(ok),
                            });
                        }
                    }
                }
                Ok(Outcome::Finished(report)) => {
                    // Отчёт без единого подтверждённого шага — это галлюцинация
                    // успеха, самый дорогой вид ошибки.
                    if let Err(why) = grounding::report_is_grounded(&report, trace.verified_count())
                    {
                        self.log(format!("Отклонил finish: {why}"));
                        failures.push(format!("{why}: сделай проверяемый шаг и покажи результат"));
                        continue;
                    }
                    let left = graph
                        .nodes
                        .iter()
                        .filter(|n| n.state == graph::NodeState::Pending)
                        .count();
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
                    drop(m);
                    return Ok(if left > 0 {
                        format!("{report}\n\n(узлов графа не закрыто: {left} — проверь результат)")
                    } else {
                        report
                    });
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
                    failures.push(err.clone());
                    self.mem_lock()
                        .log_step(&task_id, step as i64, &act_str, &err, false)?;
                    trace.push(Step {
                        n: step,
                        thought: decision.thought.clone(),
                        action: act_str.clone(),
                        observation: format!("ошибка: {err}"),
                        confidence: decision.confidence,
                        verified: Some(false),
                    });

                    // --- РЕФЛЕКСИЯ ---
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

                    // Узел исчерпал попытки — откатываем сделанное и перепланируем.
                    if let Work::Node { id, .. } = &work {
                        if graph.mark_attempt_failed(id, &err) {
                            self.log(format!("Узел {id} провалился — откатываю выполненное"));
                            rollback_queue = graph.rollback_plan();
                            rollback_steps = 0;
                            need_replan = true;
                            self.send_plan(&graph);
                            if rollback_queue.is_empty() {
                                // Откатывать нечего — сразу к перепланированию.
                                let screen = view.prompt.clone();
                                if let Ok(g) =
                                    planner::replan(&self.llm, &graph, &screen, &failures)
                                {
                                    graph = g;
                                    self.send_plan(&graph);
                                    failures.clear();
                                    need_replan = false;
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(format!(
            "Достигнут лимит {} шагов. {}",
            self.cfg.max_steps,
            self.finish_report(&graph, &trace)
        ))
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

    /// Независимая проверка критерия готовности узла по свежему экрану.
    /// Цитату модели сверяем с наблюдением: сочинённое «подтверждение» не
    /// проходит (см. grounding.rs).
    fn verify_node(&self, phone_mode: bool, goal: &str, done_when: &str) -> (bool, String) {
        let view = match self.perceive(phone_mode, false) {
            Ok(v) => v,
            Err(PerceiveError::PhoneGone(e)) | Err(PerceiveError::ScreenGone(e)) => {
                return (false, format!("экран не читается: {e}"))
            }
        };
        let ev = Evidence::from_prompt(&view.prompt);
        let user = format!(
            "ПОДЗАДАЧА: {goal}\nКРИТЕРИЙ ГОТОВНОСТИ: {done_when}\n\nСВЕЖИЙ ЭКРАН:\n{}",
            view.prompt
        );
        let resp = match self.llm.complete(
            &[Message::system(VERIFY_SYSTEM), Message::user(user)],
            "verify",
            true,
            0.0,
        ) {
            Ok(r) => r,
            Err(e) => return (false, format!("проверяющий вызов не удался: {e}")),
        };
        let ans: VerifyAnswer = match extract_json(&resp.text)
            .and_then(|v| serde_json::from_value::<VerifyAnswer>(v).map_err(anyhow::Error::from))
        {
            Ok(a) => a,
            Err(e) => return (false, format!("проверяющий ответил не по формату: {e}")),
        };
        if !ans.verified {
            return (
                false,
                if ans.why.is_empty() {
                    "критерий не виден".into()
                } else {
                    ans.why
                },
            );
        }
        // Цитата обязательна и обязана существовать на экране.
        if ans.evidence.trim().is_empty() {
            return (false, "нет цитаты с экрана".into());
        }
        if !ev.mentions(&ans.evidence) {
            return (
                false,
                format!("цитата «{}» отсутствует на экране", short(&ans.evidence)),
            );
        }
        (true, ans.evidence)
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

fn short(s: &str) -> String {
    if s.chars().count() <= 60 {
        s.to_string()
    } else {
        s.chars().take(60).collect::<String>() + "…"
    }
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
        let trace = Trace::default();
        assert!(grounding::report_is_grounded(
            "Всё готово, игра установлена",
            trace.verified_count()
        )
        .is_err());
    }
}
