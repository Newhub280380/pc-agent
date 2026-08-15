//! Планировщик: разбивает задачу человека на подзадачи и умеет
//! перепланировать на ходу.
//!
//! Требование: «Зарегистрируйся на сайте» агент сам раскладывает на
//! «найти форму → заполнить → пройти капчу → подтвердить почту».
//!
//! Почему план строится ОДИН раз и потом правится, а не заново на каждом шаге:
//!   - стабильность: иначе агент «забывает» цель и ходит по кругу;
//!   - деньги: перепланирование — самый дорогой вызов (длинный контекст).
//!
//! Перепланирование запускается по триггерам: 2 провала подряд, экран
//! радикально изменился, или подзадача оказалась неактуальной.

use crate::llm::{extract_json, LlmClient, Message};
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subtask {
    pub id: u32,
    pub goal: String,
    /// Как понять, что подзадача выполнена. Без явного критерия агент либо
    /// зацикливается, либо объявляет успех раньше времени.
    pub done_when: String,
    #[serde(default)]
    pub done: bool,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Plan {
    pub goal: String,
    pub subtasks: Vec<Subtask>,
    #[serde(default)]
    pub risks: Vec<String>,
}

impl Plan {
    pub fn current(&self) -> Option<&Subtask> {
        self.subtasks.iter().find(|s| !s.done)
    }
    pub fn complete_current(&mut self, note: &str) {
        if let Some(s) = self.subtasks.iter_mut().find(|s| !s.done) {
            s.done = true;
            s.notes = note.to_string();
        }
    }
    pub fn progress(&self) -> (usize, usize) {
        (
            self.subtasks.iter().filter(|s| s.done).count(),
            self.subtasks.len(),
        )
    }
    pub fn to_prompt(&self) -> String {
        let mut s = format!("ЦЕЛЬ: {}\nПЛАН:\n", self.goal);
        for t in &self.subtasks {
            s.push_str(&format!(
                "{} [{}] {} (готово когда: {})\n",
                t.id,
                if t.done { "x" } else { " " },
                t.goal,
                t.done_when
            ));
        }
        s
    }
}

const PLANNER_SYSTEM: &str = r#"Ты — планировщик автономного агента, который управляет компьютером Windows и телефоном Android как человек.
Разбей задачу пользователя на 3-8 подзадач. Каждая подзадача — наблюдаемый результат на экране, а не абстракция.
Учитывай реальность: авторизация, 2FA, капчи, всплывающие окна, подтверждение почты, медленная загрузка.
Если для задачи нужен доступ, которого может не быть (аккаунт, приложение, ADB, платёжка) — добавь это отдельной подзадачей проверки.
Отвечай СТРОГО JSON:
{"goal":"...","subtasks":[{"id":1,"goal":"...","done_when":"..."}],"risks":["..."]}"#;

pub fn make_plan(llm: &LlmClient, task: &str, memory_hints: &str) -> Result<Plan> {
    let mut user = format!("ЗАДАЧА ПОЛЬЗОВАТЕЛЯ: {task}\n");
    if !memory_hints.trim().is_empty() {
        // Прошлый опыт в планировщик — иначе агент каждый раз заново
        // «открывает», что на этом сайте есть баннер кук.
        user.push_str(&format!(
            "\nЧТО АГЕНТ УЖЕ ЗНАЕТ ПО ЭТОЙ ТЕМЕ:\n{memory_hints}\n"
        ));
    }
    let resp = llm.complete(
        &[Message::system(PLANNER_SYSTEM), Message::user(user)],
        "plan",
        true,
        0.2,
    )?;
    let v = extract_json(&resp.text)?;
    let mut plan: Plan = serde_json::from_value(v)?;
    if plan.goal.is_empty() {
        plan.goal = task.to_string();
    }
    for (i, st) in plan.subtasks.iter_mut().enumerate() {
        st.id = i as u32 + 1;
    }
    Ok(plan)
}

const REPLAN_SYSTEM: &str = r#"Ты — планировщик автономного агента. Текущий план не работает.
Проанализируй, что пошло не так, и выдай ИСПРАВЛЕННЫЙ план оставшихся шагов (выполненное не повторяй).
Отвечай СТРОГО JSON того же формата: {"goal":"...","subtasks":[{"id":1,"goal":"...","done_when":"..."}],"risks":["..."]}"#;

pub fn replan(llm: &LlmClient, plan: &Plan, screen: &str, failures: &[String]) -> Result<Plan> {
    let user = format!(
        "{}\n\nТЕКУЩИЙ ЭКРАН:\n{}\n\nПОСЛЕДНИЕ ОШИБКИ:\n- {}",
        plan.to_prompt(),
        screen,
        failures.join("\n- ")
    );
    let resp = llm.complete(
        &[Message::system(REPLAN_SYSTEM), Message::user(user)],
        "replan",
        true,
        0.3,
    )?;
    let mut new_plan: Plan = serde_json::from_value(extract_json(&resp.text)?)?;
    // Сохраняем уже выполненные подзадачи: они — история, а не мусор.
    let done: Vec<Subtask> = plan.subtasks.iter().filter(|s| s.done).cloned().collect();
    let mut merged = done;
    for st in new_plan.subtasks.drain(..) {
        merged.push(st);
    }
    for (i, st) in merged.iter_mut().enumerate() {
        st.id = i as u32 + 1;
    }
    Ok(Plan {
        goal: plan.goal.clone(),
        subtasks: merged,
        risks: new_plan.risks,
    })
}
