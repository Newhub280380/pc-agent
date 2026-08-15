//! GUI на egui: одно окно — поле задачи, Старт/Стоп, живой лог, план.
//!
//! Почему egui, а не WinUI/Qt/Electron:
//!   - линкуется прямо в .exe, нулевая установка (требование «1 файл»);
//!   - не тянет WebView2 (который на чистой Windows может отсутствовать);
//!   - отрисовка в immediate mode — идеально для лога, который капает
//!     из фонового потока.
//! Минус: не нативный вид элементов. Для внутреннего инструмента приемлемо.

use crate::agent::{AgentCommand, AgentEvent};
use eframe::egui;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

pub struct AppState {
    pub task: String,
    pub log: Vec<String>,
    pub thought: String,
    pub plan: Vec<(String, bool)>,
    pub running: bool,
    pub pending_question: Option<(String, bool)>,
    pub answer: String,
    pub status: String,
    pub tx: Sender<AgentCommand>,
    pub rx: Receiver<AgentEvent>,
    pub stop: Arc<AtomicBool>,
    pub start_requested: Arc<AtomicBool>,
    pub providers: String,
}

impl AppState {
    fn drain_events(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                AgentEvent::Log(s) => {
                    self.log.push(format!("{}  {}", now(), s));
                    // Лог в памяти ограничен: за длинную задачу иначе набежит
                    // десятки тысяч строк и окно начнёт тормозить.
                    if self.log.len() > 2000 {
                        self.log.drain(..500);
                    }
                }
                AgentEvent::Thought(t) => self.thought = t,
                AgentEvent::Plan(p) => self.plan = p,
                AgentEvent::Ask { question, secret } => {
                    self.pending_question = Some((question, secret));
                    self.answer.clear();
                }
                AgentEvent::NeedTool { what, why, how } => {
                    self.log.push(format!(
                        "{}  НУЖЕН ИНСТРУМЕНТ: {what} — {why}. Как: {how}",
                        now()
                    ));
                }
                AgentEvent::Done(r) => {
                    self.running = false;
                    self.status = "готово".into();
                    self.log.push(format!("{}  ✔ {r}", now()));
                }
                AgentEvent::Failed(e) => {
                    self.running = false;
                    self.status = "ошибка".into();
                    self.log.push(format!("{}  ✖ {e}", now()));
                }
                AgentEvent::Idle => {
                    self.running = false;
                    self.status = "ожидание".into();
                }
            }
        }
    }
}

impl eframe::App for AppState {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();
        // Перерисовка по таймеру: лог приходит из другого потока, а egui
        // сам по себе не знает, что данные изменились.
        ctx.request_repaint_after(std::time::Duration::from_millis(200));

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("PC Agent");
                ui.separator();
                ui.label(format!("статус: {}", self.status));
                ui.separator();
                ui.label(&self.providers);
            });
        });

        egui::SidePanel::right("plan")
            .min_width(280.0)
            .show(ctx, |ui| {
                ui.heading("План");
                if self.plan.is_empty() {
                    ui.label("— пусто —");
                }
                for (goal, done) in &self.plan {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(if *done { "✔" } else { "•" });
                        ui.label(goal);
                    });
                }
                ui.separator();
                ui.heading("Мысль");
                ui.label(&self.thought);
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.label("Задача (обычным языком):");
            ui.add_enabled(
                !self.running,
                egui::TextEdit::multiline(&mut self.task)
                    .desired_rows(3)
                    .desired_width(f32::INFINITY)
                    .hint_text(
                        "Например: Настрой рекламу в FB на 5000 тенге, ЦА мужчины 25-40 Алматы",
                    ),
            );

            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!self.running, egui::Button::new("▶ Старт"))
                    .clicked()
                    && !self.task.trim().is_empty()
                {
                    self.running = true;
                    self.status = "работаю".into();
                    self.log.clear();
                    self.stop.store(false, Ordering::Relaxed);
                    let _ = self.tx.send(AgentCommand::Start(self.task.clone()));
                    self.start_requested.store(true, Ordering::Relaxed);
                }
                if ui
                    .add_enabled(self.running, egui::Button::new("■ Стоп"))
                    .clicked()
                {
                    self.stop.store(true, Ordering::Relaxed);
                    let _ = self.tx.send(AgentCommand::Stop);
                    self.status = "останавливаюсь".into();
                }
            });

            ui.separator();
            ui.label("Что делаю:");
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for line in &self.log {
                        ui.label(line);
                    }
                });
        });

        // Модалка «нужен человек»: 2FA, SMS-код, подтверждение.
        if let Some((question, secret)) = self.pending_question.clone() {
            egui::Window::new("Нужен ты")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(&question);
                    let edit = egui::TextEdit::singleline(&mut self.answer).password(secret);
                    let resp = ui.add(edit);
                    resp.request_focus();
                    let submit = ui.button("Отправить").clicked()
                        || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                    if submit {
                        let _ = self.tx.send(AgentCommand::Answer(self.answer.clone()));
                        self.answer.clear();
                        self.pending_question = None;
                    }
                });
        }
    }
}

fn now() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

/// Экран первого запуска: одно окно, одно согласие, дальше агент работает.
/// Никаких email-подтверждений и трёхэтапных идентификаций.
pub fn consent_dialog(text: &str) -> bool {
    let accepted = Arc::new(AtomicBool::new(false));
    let flag = accepted.clone();
    let body = text.to_string();
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([620.0, 420.0]),
        ..Default::default()
    };
    let _ = eframe::run_simple_native("PC Agent — доступ", opts, move |ctx, _| {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Разреши агенту работать на этом компьютере");
            ui.add_space(8.0);
            ui.label(&body);
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui.button("Разрешить и запустить").clicked() {
                    flag.store(true, Ordering::Relaxed);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                if ui.button("Выход").clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        });
    });
    accepted.load(Ordering::Relaxed)
}
