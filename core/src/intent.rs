//! Слой намерения: прежде чем агент начнёт кликать, он обязан понять,
//! что именно человек хочет на самом деле — и не выходит из этого
//! понимания, пока намерение не раскрыто на всю глубину.
//!
//! Зачем отдельный слой, а не «ещё один промпт»:
//!   - до этого триаж отделял только разговор от задачи, а сама задача
//!     шла в планировщик как есть: «сделай отчёт» превращался в план
//!     догадок, и агент узнавал пробелы по ходу кликов;
//!   - здесь намерение — это КОНТРАКТ (Brief): цель, сущности, ограничения,
//!     критерии успеха, открытые вопросы. Контракт фиксируется до плана,
//!     виден планировщику и на каждом шаге, и модель не может молча его
//!     подменить;
//!   - «глубина» проверяется детерминированно (coverage в `decide`), а не
//!     «модель решила, что всё понятно»: блокирующий пробел — это вопрос
//!     человеку, а не догадка. Лимиты вопросов жёсткие, чтобы слой не
//!     превратился в анкету.
//!
//! Режимы (`PCAGENT_INTENT`):
//!   - `auto` (по умолчанию) — разговор закрывается ответом, пробелы
//!     выясняются вопросами, готовое намерение сразу идёт в план;
//!   - `confirm` — то же, но перед планом человек подтверждает понимание
//!     («Понял так: … Верно?»); поправка пересобирает намерение;
//!   - `off` — старый путь без слоя (триаж + план по сырой задаче).
//!
//! Цена: один вызов модели на задачу вместо старого триажа (он заменён),
//! плюс по одному вызову на каждый ответ человека. Ясная задача без
//! пробелов обходится теми же двумя вызовами, что и раньше.

use crate::llm::{extract_json, CompleteResponse, LlmClient, Message};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Сколько блокирующих вопросов максимум задаём по одной задаче. Дальше
/// оставшиеся пробелы честно фиксируются как допущения — агент не имеет
/// права крутиться в анкете вместо работы.
pub const MAX_QUESTIONS: u32 = 3;

/// Страховочный предел ходов диалога намерения (вопросы + подтверждения +
/// поправки). Реальные вопросы режет MAX_QUESTIONS, этот предел — от
/// зацикливания «поправка → пересборка → подтверждение».
pub const MAX_TURNS: u32 = 12;

/// Минимальный контракт «модели», нужный слою намерения. Через него тесты
/// подставляют сценарные ответы вместо живого роутера.
pub trait Chat {
    fn complete(
        &self,
        messages: &[Message],
        purpose: &str,
        json_mode: bool,
        temperature: f64,
    ) -> Result<CompleteResponse>;
}

impl Chat for LlmClient {
    fn complete(
        &self,
        messages: &[Message],
        purpose: &str,
        json_mode: bool,
        temperature: f64,
    ) -> Result<CompleteResponse> {
        LlmClient::complete(self, messages, purpose, json_mode, temperature)
    }
}

/// Режим слоя намерения.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Off,
    Auto,
    Confirm,
}

impl Mode {
    pub fn from_env() -> Self {
        match std::env::var("PCAGENT_INTENT")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "off" | "0" | "no" | "нет" => Mode::Off,
            "confirm" | "ask" | "спросить" => Mode::Confirm,
            _ => Mode::Auto,
        }
    }

    pub fn enabled(self) -> bool {
        !matches!(self, Mode::Off)
    }
}

/// Что делает сессия намерения после очередного сообщения.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// Это разговор: ответить человеку и завершить как chat, в задачу не идём.
    Chat(String),
    /// Задать человеку один вопрос и продолжить после ответа.
    Ask(String),
    /// Показать понимание и спросить подтверждение (режим Confirm).
    Confirm(String),
    /// Намерение раскрыто достаточно — можно строить план.
    Run,
}

/// Конкретное значение, без которого действие неоднозначно.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Entity {
    pub name: String,
    pub value: String,
    /// "user" — человек сказал, "assumed" — достроено из контекста.
    #[serde(default)]
    pub source: String,
}

/// Пробел в намерении. `blocking` — ошибка в догадке сделает действие
/// неверным или необратимым, либо сделает проверку успеха невозможной.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Unknown {
    pub question: String,
    #[serde(default)]
    pub why: String,
    #[serde(default)]
    pub blocking: bool,
}

/// Контракт намерения — то, что агент зафиксировал ДО плана.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Brief {
    #[serde(default)]
    pub goal: String,
    #[serde(default)]
    pub entities: Vec<Entity>,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub criteria: Vec<String>,
    #[serde(default)]
    pub unknowns: Vec<Unknown>,
    #[serde(default)]
    pub assumptions: Vec<String>,
    #[serde(default)]
    pub risks: Vec<String>,
}

enum Kind {
    Chat(String),
    Task(Brief),
}

const INTENT_SYSTEM: &str = r#"Ты — приёмщик намерений автономного агента, который управляет компьютером и телефоном.
Твоя работа — ДО того, как агент начнёт действовать, понять, чего человек хочет на самом деле, и не дать ему действовать по догадке.

На вход — сообщения человека и твои вопросы с его ответами. Реши:

1. {"kind":"chat","reply":"..."} — это разговор, а не поручение: приветствие, вопрос о тебе, благодарность, уточнение без задачи. Ответь коротко и по-человечески. Ничего не планируй.
2. {"kind":"task","brief":{...}} — это поручение. Заполни brief:

- goal — одна фраза: какой наблюдаемый результат нужен.
- entities — конкретные значения, без которых действие невозможно или неоднозначно: сайт, файл, имя, сумма, дата, номер. source="user" — человек сказал, source="assumed" — ты достроил из контекста.
- constraints — запреты и условия человека («не тратить деньги», «только посмотри», «до пятницы»).
- criteria — как проверить успех НАБЛЮДАЕМО (что будет видно на экране или в системе).
- unknowns — что ОБЯЗАТЕЛЬНО надо знать, чтобы не сделать не то. blocking=true, если ошибка в догадке приведёт к неверному или необратимому действию либо сделает проверку успеха невозможной. Сомневаешься — blocking=true. blocking=false — можно безопасно предположить, тогда продублируй в assumptions.
- assumptions — безопасные додумывания (время «сегодня», папка по умолчанию).
- risks — что может пойти не так.

Правила:
- Используй ВСЮ историю: ответы на вопросы и поправки — часть задачи. «нет, я имел в виду X» заменяет прежнюю цель.
- Местоимения и «это/тот/там» разрешай по истории, не оставляй их в goal.
- Не задавай лишних вопросов: unknowns — только то, без чего действие опасно или бессмысленно. Ясная задача («открой почту») — unknowns пуст.
- Один unknown — один короткий вопрос, на который отвечают словом или фразой.
- Не повторяй вопросы, на которые человек уже ответил.
- Отвечай СТРОГО JSON, без текста вне JSON."#;

/// Сессия намерения одной задачи: диалог, контракт и решения
/// «спросить / подтверждение / в работу».
pub struct Session<C: Chat> {
    chat: C,
    mode: Mode,
    history: Vec<Message>,
    brief: Option<Brief>,
    asks: u32,
    confirmed: bool,
    awaiting_confirm: bool,
    /// Вопрос (или сводка подтверждения), заданный человеку последним —
    /// его кладём в историю перед ответом, иначе модель не видит контекст.
    last_question: Option<String>,
    last_user: String,
    finished: bool,
}

impl<C: Chat> Session<C> {
    pub fn new(chat: C, mode: Mode) -> Self {
        Self {
            chat,
            mode,
            history: vec![],
            brief: None,
            asks: 0,
            confirmed: false,
            awaiting_confirm: false,
            last_question: None,
            last_user: String::new(),
            finished: false,
        }
    }

    /// Контракт для промптов планировщика и шагов. Пусто, пока намерение
    /// не зафиксировано.
    pub fn brief_text(&self) -> String {
        self.render(true)
    }

    /// Сводка для человека (без служебной шапки контракта).
    pub fn summary_text(&self) -> String {
        self.render(false)
    }

    fn render(&self, for_prompt: bool) -> String {
        let Some(b) = self.brief.as_ref() else {
            return String::new();
        };
        let mut s = String::new();
        if for_prompt {
            s.push_str("НАМЕРЕНИЕ СЕССИИ (контракт: цель, сущности, ограничения и критерии зафиксированы здесь; не подменяй их своими догадками)\n");
        }
        if !b.goal.trim().is_empty() {
            s.push_str(&format!("Цель: {}\n", b.goal.trim()));
        }
        if !b.entities.is_empty() {
            s.push_str("Сущности:\n");
            for e in &b.entities {
                let name = e.name.trim();
                let value = e.value.trim();
                if name.is_empty() || value.is_empty() {
                    continue;
                }
                let assumed = e.source.trim().eq_ignore_ascii_case("assumed");
                let mark = if assumed { " [додумано]" } else { "" };
                s.push_str(&format!("- {name}: {value}{mark}\n"));
            }
        }
        if !b.constraints.is_empty() {
            s.push_str(&format!("Ограничения: {}\n", join_nonempty(&b.constraints)));
        }
        if !b.criteria.is_empty() {
            s.push_str(&format!(
                "Критерии успеха: {}\n",
                join_nonempty(&b.criteria)
            ));
        }
        if !b.assumptions.is_empty() {
            s.push_str(&format!("Допущения: {}\n", join_nonempty(&b.assumptions)));
        }
        if !b.risks.is_empty() {
            s.push_str(&format!("Риски: {}\n", join_nonempty(&b.risks)));
        }
        s.trim_end().to_string()
    }

    /// Новое сообщение человека. Первый и единственный `push` на сессию —
    /// дальше идут `answer` на вопросы и `proceed` без человека.
    pub fn push(&mut self, text: &str) -> Result<Step> {
        anyhow::ensure!(!self.finished, "сессия намерения уже закрыта");
        self.last_user = text.to_string();
        self.history.push(Message::user(text));
        self.extract()
    }

    /// Ответ человека на вопрос или поправка к подтверждению.
    pub fn answer(&mut self, text: &str) -> Result<Step> {
        anyhow::ensure!(!self.finished, "сессия намерения уже закрыта");
        if self.awaiting_confirm {
            self.awaiting_confirm = false;
            self.last_user = text.to_string();
            if affirmative(text) {
                self.confirmed = true;
                self.finished = true;
                return Ok(Step::Run);
            }
            // Поправка: намерение пересобирается по всему диалогу, но
            // повторно подтверждать не просим — человек уже включился.
            self.confirmed = true;
        }
        self.remember_question();
        self.history.push(Message::user(text));
        self.extract()
    }

    /// Человека нет (headless, оборванный канал): оставшиеся блокирующие
    /// пробелы честно фиксируем как допущения и идём в работу, а не
    /// крутимся в вопросах, на которые никто не ответит.
    pub fn proceed(&mut self) -> Step {
        if let Some(b) = self.brief.as_mut() {
            let open: Vec<String> = b
                .unknowns
                .iter()
                .filter(|u| u.blocking)
                .map(|u| u.question.trim().to_string())
                .filter(|q| !q.is_empty())
                .collect();
            for q in open {
                b.assumptions
                    .push(format!("без ответа человека принято: {q}"));
            }
            b.unknowns.retain(|u| !u.blocking);
        }
        self.finished = true;
        Step::Run
    }

    fn remember_question(&mut self) {
        if let Some(q) = self.last_question.take() {
            if !q.trim().is_empty() {
                self.history.push(Message {
                    role: "assistant".into(),
                    text: q,
                    images: vec![],
                });
            }
        }
    }

    fn messages(&self) -> Vec<Message> {
        let mut v = Vec::with_capacity(self.history.len() + 1);
        v.push(Message::system(INTENT_SYSTEM));
        v.extend(self.history.iter().cloned());
        v
    }

    fn extract(&mut self) -> Result<Step> {
        // Один повтор на битый JSON: модель иногда отвечает прозой, и
        // из-за одного такого ответа задача не должна терять слой.
        let messages = self.messages();
        let mut last_err: Option<anyhow::Error> = None;
        for _ in 0..2 {
            let resp = self
                .chat
                .complete(&messages, "intent", true, 0.2)
                .context("намерение: модель не ответила")?;
            match parse_extraction(&resp.text, &self.last_user) {
                Ok(kind) => return self.finish_extract(kind),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("намерение: пустой ответ")))
    }

    fn finish_extract(&mut self, kind: Kind) -> Result<Step> {
        match kind {
            Kind::Chat(reply) => {
                if self.brief.is_some() {
                    // Сессия уже задача: «чат» посреди диалога — это реплика
                    // на вопрос, а не выход из задачи. Решаем по брифу,
                    // который уже зафиксирован.
                    return self.decide();
                }
                self.finished = true;
                Ok(Step::Chat(reply))
            }
            Kind::Task(brief) => {
                self.brief = Some(brief);
                self.decide()
            }
        }
    }

    /// Детерминированное покрытие: спрашиваем, только если есть блокирующий
    /// пробел и лимит вопросов не исчерпан. Модель решает, ЧТО спросить;
    /// код решает, НУЖНО ли спрашивать — «модель решила, что всё понятно»
    /// здесь не принимается.
    fn decide(&mut self) -> Result<Step> {
        let blocking = match self.brief.as_ref() {
            Some(b) => b
                .unknowns
                .iter()
                .filter(|u| u.blocking && !u.question.trim().is_empty())
                .count(),
            None => 0,
        };
        if blocking > 0 && self.asks < MAX_QUESTIONS {
            let u = self
                .brief
                .as_ref()
                .and_then(|b| {
                    b.unknowns
                        .iter()
                        .find(|u| u.blocking && !u.question.trim().is_empty())
                })
                .cloned()
                .unwrap();
            self.asks += 1;
            let q = if u.why.trim().is_empty() {
                u.question.trim().to_string()
            } else {
                format!("{} ({})", u.question.trim(), u.why.trim())
            };
            self.last_question = Some(q.clone());
            return Ok(Step::Ask(q));
        }
        // Лимит вопросов исчерпан: что не выяснили — фиксируем как
        // допущения, а не крутимся дальше.
        if blocking > 0 {
            if let Some(b) = self.brief.as_mut() {
                let open: Vec<String> = b
                    .unknowns
                    .iter()
                    .filter(|u| u.blocking)
                    .map(|u| u.question.trim().to_string())
                    .filter(|q| !q.is_empty())
                    .collect();
                for q in open {
                    b.assumptions
                        .push(format!("не уточнено у человека, принято по умолчанию: {q}"));
                }
                b.unknowns.retain(|u| !u.blocking);
            }
        }
        if matches!(self.mode, Mode::Confirm) && !self.confirmed {
            let summary = self.summary_text();
            self.awaiting_confirm = true;
            self.last_question = Some(summary.clone());
            return Ok(Step::Confirm(summary));
        }
        self.finished = true;
        Ok(Step::Run)
    }
}

fn join_nonempty(items: &[String]) -> String {
    items
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("; ")
}

/// «да» в любом оформлении, но не «давай не так»: первое слово целиком.
fn affirmative(text: &str) -> bool {
    let first = text.split_whitespace().next().unwrap_or("").to_lowercase();
    let word: String = first
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '+')
        .take(16)
        .collect();
    matches!(
        word.as_str(),
        "да" | "ок"
            | "ok"
            | "okay"
            | "yes"
            | "y"
            | "верно"
            | "точно"
            | "ага"
            | "угу"
            | "можно"
            | "подтверждаю"
            | "поехали"
            | "старт"
            | "+"
            | "д"
    )
}

fn parse_extraction(raw: &str, fallback_goal: &str) -> Result<Kind> {
    let v = extract_json(raw).context("намерение: не JSON")?;
    match v.get("kind").and_then(|k| k.as_str()).unwrap_or("") {
        "chat" => {
            let reply = v
                .get("reply")
                .and_then(|r| r.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            Ok(Kind::Chat(if reply.is_empty() {
                "На связи.".to_string()
            } else {
                reply
            }))
        }
        "task" => {
            // brief может отсутствовать или быть null — тогда контракт
            // пустой, а цель берётся из слов человека ниже.
            let mut brief: Brief = serde_json::from_value(
                v.get("brief")
                    .filter(|b| !b.is_null())
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({})),
            )
            .context("намерение: brief не разобран")?;
            if brief.goal.trim().is_empty() {
                brief.goal = fallback_goal.trim().chars().take(200).collect();
            }
            brief.unknowns.retain(|u| !u.question.trim().is_empty());
            Ok(Kind::Task(brief))
        }
        other => Err(anyhow::anyhow!("намерение: неизвестный kind «{other}»")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// Сценарные ответы вместо роутера: каждый `complete` отдаёт следующий
    /// заготовленный JSON и запоминает промпт — по промптам видно, что
    /// диалог и контракт доезжают до модели.
    #[derive(Default)]
    struct Stub {
        replies: Mutex<VecDeque<String>>,
        prompts: Mutex<Vec<String>>,
    }

    impl Stub {
        fn new(replies: &[&str]) -> Self {
            Self {
                replies: Mutex::new(replies.iter().map(|s| s.to_string()).collect()),
                prompts: Mutex::new(vec![]),
            }
        }

        fn prompt(&self, i: usize) -> String {
            self.prompts.lock().unwrap()[i].clone()
        }
    }

    impl Chat for Stub {
        fn complete(
            &self,
            messages: &[Message],
            _purpose: &str,
            _json_mode: bool,
            _temperature: f64,
        ) -> Result<CompleteResponse> {
            let mut text = String::new();
            for m in messages {
                text.push_str(&format!("[{}] {}\n", m.role, m.text));
            }
            self.prompts.lock().unwrap().push(text);
            let reply = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| r#"{"kind":"task","brief":{"goal":"фолбэк"}}"#.into());
            Ok(CompleteResponse {
                text: reply,
                ..Default::default()
            })
        }
    }

    fn task_json(goal: &str) -> String {
        format!(
            r#"{{"kind":"task","brief":{{"goal":"{goal}","entities":[],"constraints":[],"criteria":[],"unknowns":[],"assumptions":[],"risks":[]}}}}"#
        )
    }

    #[test]
    fn chat_is_answered_and_never_becomes_a_task() {
        let mut s = Session::new(
            Stub::new(&[r#"{"kind":"chat","reply":"Привет! Работает."}"#]),
            Mode::Auto,
        );
        match s.push("привет, работаешь?").unwrap() {
            Step::Chat(reply) => assert!(reply.contains("Привет")),
            other => panic!("разговор не должен идти в задачу: {other:?}"),
        }
        assert!(s.brief_text().is_empty());
    }

    #[test]
    fn clear_task_runs_without_questions() {
        let mut s = Session::new(Stub::new(&[&task_json("Открыть почту")]), Mode::Auto);
        assert_eq!(s.push("открой почту").unwrap(), Step::Run);
        assert!(s.brief_text().contains("Открыть почту"));
    }

    #[test]
    fn blocking_gap_asks_one_question_and_answer_flows_into_dialog() {
        let first = r#"{"kind":"task","brief":{"goal":"подготовить отчёт","entities":[],"constraints":[],"criteria":[],"unknowns":[{"question":"Какой файл отчёта?","why":"иначе сохраню не туда","blocking":true}],"assumptions":[],"risks":[]}}"#;
        let second = r#"{"kind":"task","brief":{"goal":"подготовить отчёт","entities":[{"name":"файл","value":"report.pdf","source":"user"}],"constraints":[],"criteria":[],"unknowns":[],"assumptions":[],"risks":[]}}"#;
        let stub = Stub::new(&[first, second]);
        let mut s = Session::new(stub, Mode::Auto);
        match s.push("подготовь отчёт").unwrap() {
            Step::Ask(q) => assert!(q.contains("Какой файл отчёта?")),
            other => panic!("блокирующий пробел обязан стать вопросом: {other:?}"),
        }
        assert_eq!(s.answer("report.pdf").unwrap(), Step::Run);
        // Ответ человека и вопрос агента доехали до модели в истории.
        let p1 = s.chat.prompt(1);
        assert!(p1.contains("report.pdf"), "{p1}");
        assert!(p1.contains("[assistant]"), "{p1}");
        assert!(s.brief_text().contains("report.pdf"));
    }

    #[test]
    fn non_blocking_gap_runs_without_asking() {
        let raw = r#"{"kind":"task","brief":{"goal":"посмотреть курс","entities":[],"constraints":[],"criteria":[],"unknowns":[{"question":"какая валюта?","blocking":false}],"assumptions":["валюта по умолчанию"],"risks":[]}}"#;
        let mut s = Session::new(Stub::new(&[raw]), Mode::Auto);
        assert_eq!(s.push("посмотри курс доллара").unwrap(), Step::Run);
    }

    #[test]
    fn questions_are_capped_and_rest_become_assumptions() {
        let raw = r#"{"kind":"task","brief":{"goal":"сделать заказ","entities":[],"constraints":[],"criteria":[],"unknowns":[{"question":"какой товар?","blocking":true}],"assumptions":[],"risks":[]}}"#;
        let stub = Stub {
            replies: Mutex::new((0..20).map(|_| raw.to_string()).collect()),
            prompts: Mutex::new(vec![]),
        };
        let mut s = Session::new(stub, Mode::Auto);
        let mut step = s.push("закажи").unwrap();
        let mut asks = 0;
        while let Step::Ask(_) = step {
            asks += 1;
            step = s.answer("ответ").unwrap();
        }
        assert_eq!(asks, MAX_QUESTIONS, "вопросов больше лимита быть не должно");
        assert!(matches!(step, Step::Run));
        assert!(
            s.brief_text().contains("не уточнено у человека"),
            "невыясненный пробел обязан стать допущением: {}",
            s.brief_text()
        );
    }

    #[test]
    fn confirm_mode_shows_brief_and_yes_runs() {
        let mut s = Session::new(Stub::new(&[&task_json("Открыть почту")]), Mode::Confirm);
        match s.push("открой почту").unwrap() {
            Step::Confirm(summary) => assert!(summary.contains("Открыть почту")),
            other => panic!("confirm-режим обязан показывать понимание: {other:?}"),
        }
        assert_eq!(s.answer("да").unwrap(), Step::Run);
    }

    #[test]
    fn correction_rebuilds_intent_without_second_confirm() {
        let first = task_json("Открыть почту");
        let second = task_json("Открыть сайт example.com");
        let mut s = Session::new(Stub::new(&[&first, &second]), Mode::Confirm);
        assert!(matches!(s.push("открой почту").unwrap(), Step::Confirm(_)));
        // «давай не так» — не подтверждение, а поправка.
        assert_eq!(
            s.answer("давай не так, открой сайт example.com").unwrap(),
            Step::Run
        );
        assert!(s.brief_text().contains("example.com"));
        // Повторного подтверждения после поправки нет — человек уже включился.
    }

    #[test]
    fn proceed_without_human_records_assumptions_and_runs() {
        let raw = r#"{"kind":"task","brief":{"goal":"отправить файл","entities":[],"constraints":[],"criteria":[],"unknowns":[{"question":"кому отправить?","blocking":true}],"assumptions":[],"risks":[]}}"#;
        let mut s = Session::new(Stub::new(&[raw]), Mode::Confirm);
        assert!(matches!(s.push("отправь файл").unwrap(), Step::Ask(_)));
        assert_eq!(s.proceed(), Step::Run);
        assert!(s.brief_text().contains("без ответа человека"));
    }

    #[test]
    fn affirmative_recognizes_yes_but_not_corrections() {
        for yes in [
            "да",
            "Да!",
            "ок",
            "OK",
            "yes",
            "верно",
            "точно",
            "ага",
            "поехали",
            "+",
        ] {
            assert!(affirmative(yes), "{yes} — это подтверждение");
        }
        for no in ["давай не так", "нет", "другой файл", ""] {
            assert!(!affirmative(no), "{no} — это не подтверждение");
        }
    }

    #[test]
    fn broken_extraction_retries_then_errors() {
        // Два запроса на битый JSON, третий — чтобы проверить, что постоянный
        // мусор от модели всё-таки ошибка, а не тихая пустая задача.
        let mut s = Session::new(
            Stub::new(&["не json", "ещё не json", "и снова не json", "мусор"]),
            Mode::Auto,
        );
        assert!(s.push("привет").is_err());
    }

    #[test]
    fn task_without_brief_falls_back_to_user_text() {
        // Модель забыла brief — цель берётся из слов человека, слой не падает.
        let mut s = Session::new(Stub::new(&[r#"{"kind":"task"}"#]), Mode::Auto);
        assert_eq!(s.push("открой почту").unwrap(), Step::Run);
        assert!(s.brief_text().contains("открой почту"));
    }

    #[test]
    fn mode_comes_from_env() {
        std::env::remove_var("PCAGENT_INTENT");
        assert_eq!(Mode::from_env(), Mode::Auto);
        std::env::set_var("PCAGENT_INTENT", "confirm");
        assert_eq!(Mode::from_env(), Mode::Confirm);
        std::env::set_var("PCAGENT_INTENT", "off");
        assert_eq!(Mode::from_env(), Mode::Off);
        std::env::remove_var("PCAGENT_INTENT");
    }
}

