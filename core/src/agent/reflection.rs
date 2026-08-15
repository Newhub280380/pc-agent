//! Самообучение: разбор ошибки → урок → фикс.
//!
//! Механика:
//!   1. Действие упало (или экран не изменился) → строим «подпись» ошибки:
//!      контекст + тип действия + класс ошибки. Подпись стабильна, поэтому
//!      одна и та же грабля не порождает 100 разных уроков.
//!   2. Спрашиваем модель: причина + конкретный фикс (следующее действие).
//!   3. Урок сохраняется в память и подмешивается в промпт ДО следующего
//!      действия в этом контексте. Сработал — success++, ранжирование растёт.
//!
//! Почему не «дообучение модели»: файнтюн на пользовательской машине нереален,
//! а RAG-подсказки дают 80% эффекта за 0 рублей. Это осознанный размен.

use crate::llm::{extract_json, LlmClient, Message};
use crate::memory::Memory;
use anyhow::Result;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Lesson {
    #[serde(default)]
    pub cause: String,
    #[serde(default)]
    pub fix: String,
    #[serde(default)]
    pub retry_hint: String,
    /// Заполняется нами, а не моделью: по ней ядро отмечает, что урок сработал.
    #[serde(default)]
    pub signature: String,
}

const SYSTEM: &str = r#"Ты — аналитик отказов автономного агента. Тебе дают: цель, действие, ошибку и состояние экрана.
Определи НАСТОЯЩУЮ причину (не пересказывай ошибку) и дай конкретный фикс — что сделать на следующем шаге.
Типичные причины: элемент ещё не загрузился; нужен скролл; фокус в другом окне; модальное окно/баннер кук перекрывает; нужен другой селектор; требуется авторизация или 2FA; сайт показал капчу.
Отвечай СТРОГО JSON: {"cause":"...","fix":"...","retry_hint":"..."}"#;

/// Стабильная подпись ошибки: по ней ищем, встречались ли уже.
pub fn signature(context: &str, action: &str, error: &str) -> String {
    let ctx: String = context.chars().take(40).collect();
    let act = action
        .split('"')
        .nth(3)
        .unwrap_or(action)
        .chars()
        .take(30)
        .collect::<String>();
    let err: String = error
        .chars()
        .filter(|c| !c.is_ascii_digit())
        .take(60)
        .collect();
    format!("{ctx}|{act}|{err}")
}

pub fn analyze(
    llm: &LlmClient,
    mem: &Memory,
    context: &str,
    goal: &str,
    action: &str,
    error: &str,
    screen: &str,
) -> Result<Lesson> {
    let user = format!(
        "ЦЕЛЬ: {goal}\nКОНТЕКСТ: {context}\nДЕЙСТВИЕ: {action}\nОШИБКА: {error}\n\nЭКРАН:\n{screen}"
    );
    let resp = llm.complete(
        &[Message::system(SYSTEM), Message::user(user)],
        "reflect",
        true,
        0.3,
    )?;
    let mut lesson: Lesson = serde_json::from_value(extract_json(&resp.text)?)?;

    let sig = signature(context, action, error);
    lesson.signature = sig.clone();
    mem.save_lesson(&sig, context, &lesson.cause, &lesson.fix)?;
    // Урок дублируется в обычную память полки: так он попадёт и в
    // планировщик следующей задачи на этом же сайте.
    mem.remember(
        &Memory::shelf_for(context),
        "lesson",
        &format!("Ошибка: {}", truncate(error, 60)),
        &format!("Причина: {}\nФикс: {}", lesson.cause, lesson.fix),
        &lesson.fix,
        0.8,
    )?;
    Ok(lesson)
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}
