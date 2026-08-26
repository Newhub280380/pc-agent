//! Планировщик: превращает задачу человека в TaskGraph (DAG подзадач) и
//! умеет перепланировать на ходу.
//!
//! Требование: «Зарегистрируйся на сайте» агент сам раскладывает на
//! «найти форму → заполнить → пройти капчу → подтвердить почту», а
//! «поставь мне Fable 5» — на «проверить диск → скачать → установить → запустить»
//! с явными зависимостями и откатом.
//!
//! Почему план строится ОДИН раз и потом правится, а не заново на каждом шаге:
//!   - стабильность: иначе агент «забывает» цель и ходит по кругу;
//!   - деньги: перепланирование — самый дорогой вызов (длинный контекст).
//!
//! Перепланирование запускается по триггерам: узел исчерпал попытки, экран
//! радикально изменился, или подзадача оказалась неактуальной.

use super::graph::TaskGraph;
use crate::llm::{extract_json, LlmClient, Message};
use crate::retry::{retry, Backoff};
use anyhow::{Context, Result};

const PLANNER_SYSTEM: &str = r#"Ты — планировщик автономного агента, который управляет компьютером (Windows или Ubuntu) и телефоном Android как человек.
Построй ГРАФ задач (DAG) из 3-8 узлов. Каждый узел — наблюдаемый результат на экране, а не абстракция.

Требования к графу:
1. deps — список id узлов, без которых этот узел бессмысленен. Не выстраивай всё в одну цепочку, если шаги независимы.
2. done_when — как ПО ЭКРАНУ понять, что узел выполнен. Без наблюдаемого критерия узел невалиден.
3. rollback — как отменить узел, если дальше всё сломается (удалить скачанное, закрыть окно, вернуть настройку). Пусто, если узел ничего не менял.
4. Учитывай реальность: авторизация, 2FA, капчи, всплывающие окна, подтверждение почты, медленная загрузка.
5. Если нужен доступ, которого может не быть (аккаунт, приложение, ADB, платёжка, место на диске) — отдельный узел проверки БЕЗ зависимостей.

Пример: "поставь мне Fable 5"
{"goal":"установить Fable 5","nodes":[
 {"id":"disk","goal":"проверить свободное место","done_when":"в проводнике видно свободно > 50 ГБ","deps":[],"rollback":""},
 {"id":"dl","goal":"скачать установщик","done_when":"в загрузках виден файл установщика","deps":["disk"],"rollback":"удалить скачанный файл"},
 {"id":"inst","goal":"установить игру","done_when":"установщик показал 'Готово' и появился ярлык","deps":["dl"],"rollback":"удалить игру через программы и компоненты"},
 {"id":"run","goal":"запустить игру","done_when":"открылось окно игры","deps":["inst"],"rollback":""}],
 "risks":["может не хватить места","установщик может требовать права администратора"]}

Отвечай СТРОГО JSON того же формата."#;

const TRIAGE_SYSTEM: &str = r#"Ты — приёмщик заданий автономного агента, который управляет компьютером.
Реши, что тебе написали: задание на действия с компьютером или просто разговор (приветствие, вопрос о тебе, благодарность, уточнение).
Разговор не превращай в задание и ничего не выдумывай: если человек написал «привет, работаешь?», ответь ему словами.
Отвечай СТРОГО JSON: {"kind":"chat","reply":"короткий ответ человеку"} или {"kind":"task"} (reply не нужен)."#;

/// Ответ человеку, если он не давал задания. `None` — это задание, строим граф.
///
/// Без этого шага модель принимает «привет, работаешь?» за цель и планирует
/// случайные действия на чужом компьютере.
pub fn chat_reply(llm: &LlmClient, task: &str) -> Option<String> {
    let resp = llm
        .complete(
            &[
                Message::system(TRIAGE_SYSTEM),
                Message::user(format!("СООБЩЕНИЕ: {task}")),
            ],
            "triage",
            true,
            0.0,
        )
        .ok()?;
    parse_triage(&resp.text)
}

fn parse_triage(raw: &str) -> Option<String> {
    let v = extract_json(raw).ok()?;
    if v.get("kind")?.as_str()? != "chat" {
        return None;
    }
    let reply = v.get("reply").and_then(|r| r.as_str()).unwrap_or("").trim();
    Some(if reply.is_empty() {
        "Я на связи. Напиши, что сделать на компьютере.".to_string()
    } else {
        reply.to_string()
    })
}

/// Построение графа. Ошибка формата не должна ронять задачу: повторяем запрос,
/// добавляя в промпт причину отказа. Если валидный граф так и не получен —
/// вызывающий (agent::run_task) переходит на линейный план из одной цели.
pub fn make_graph(llm: &LlmClient, task: &str, memory_hints: &str) -> Result<TaskGraph> {
    let mut user = format!("ЗАДАЧА ПОЛЬЗОВАТЕЛЯ: {task}\n");
    if !memory_hints.trim().is_empty() {
        // Прошлый опыт в планировщик — иначе агент каждый раз заново
        // «открывает», что на этом сайте есть баннер кук.
        user.push_str(&format!(
            "\nЧТО АГЕНТ УЖЕ ЗНАЕТ ПО ЭТОЙ ТЕМЕ:\n{memory_hints}\n"
        ));
    }

    retry(&Backoff::default(), "планирование", |attempt| {
        let mut u = user.clone();
        if attempt > 0 {
            u.push_str("\nПРЕДЫДУЩИЙ ОТВЕТ НЕ ПРОШЁЛ ВАЛИДАЦИЮ. Верни строго JSON с полями goal, nodes[id,goal,done_when,deps,rollback], risks. Циклов в deps быть не должно.\n");
        }
        let resp = llm.complete(
            &[Message::system(PLANNER_SYSTEM), Message::user(u)],
            "plan",
            true,
            0.2,
        )?;
        parse_graph(&resp.text, task)
    })
}

/// Разбор и валидация ответа модели. Вынесено отдельно, чтобы тестировать
/// без сети — именно здесь ловятся циклы, дубли и висячие зависимости.
pub fn parse_graph(raw: &str, task: &str) -> Result<TaskGraph> {
    let v = extract_json(raw).context("планировщик вернул не JSON")?;
    let mut g: TaskGraph = serde_json::from_value(v).context("не разобрал граф задач")?;
    if g.goal.trim().is_empty() {
        g.goal = task.to_string();
    }
    g.validate().map_err(anyhow::Error::msg)?;
    Ok(g)
}

const REPLAN_SYSTEM: &str = r#"Ты — планировщик автономного агента. Текущий граф задач не работает.
Проанализируй, что пошло не так, и выдай ИСПРАВЛЕННЫЙ граф ОСТАВШИХСЯ узлов (выполненное не повторяй).
Не ссылайся в deps на узлы, которых нет в твоём ответе.
Отвечай СТРОГО JSON: {"goal":"...","nodes":[{"id":"...","goal":"...","done_when":"...","deps":[],"rollback":"..."}],"risks":["..."]}"#;

/// Перепланирование. Выполненные узлы сохраняем: они история, а не мусор,
/// и их rollback ещё может понадобиться.
pub fn replan(
    llm: &LlmClient,
    graph: &TaskGraph,
    screen: &str,
    failures: &[String],
) -> Result<TaskGraph> {
    let user = format!(
        "{}\n\nТЕКУЩИЙ ЭКРАН:\n{}\n\nПОСЛЕДНИЕ ОШИБКИ:\n- {}",
        graph.to_prompt(),
        screen,
        failures.join("\n- ")
    );
    let resp = llm.complete(
        &[Message::system(REPLAN_SYSTEM), Message::user(user)],
        "replan",
        true,
        0.3,
    )?;
    let fresh = parse_graph(&resp.text, &graph.goal)?;
    Ok(merge(graph, fresh))
}

/// Склейка старого и нового графа. id новых узлов префиксуем, чтобы они не
/// столкнулись с историческими, а их deps переписываем на новые имена.
pub fn merge(old: &TaskGraph, mut fresh: TaskGraph) -> TaskGraph {
    let gen = old.nodes.iter().filter(|n| n.id.starts_with("r")).count() + 1;
    let rename = |id: &str| format!("r{gen}_{id}");
    for n in &mut fresh.nodes {
        n.id = rename(&n.id);
        n.deps = n.deps.iter().map(|d| rename(d)).collect();
        n.state = super::graph::NodeState::Pending;
        n.attempts = 0;
    }
    let mut nodes: Vec<super::graph::Node> = old
        .nodes
        .iter()
        .filter(|n| n.state != super::graph::NodeState::Pending)
        .cloned()
        .collect();
    nodes.extend(fresh.nodes);
    let mut out = TaskGraph {
        goal: old.goal.clone(),
        nodes,
        risks: fresh.risks,
        completed_order: old.completed_order.clone(),
    };
    // Если склейка дала невалидный граф — оставляем только новые узлы:
    // потерять историю неприятно, но работать без плана нельзя.
    if out.validate().is_err() {
        out.nodes.retain(|n| n.id.starts_with(&format!("r{gen}_")));
        out.completed_order.clear();
        let _ = out.validate();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_talk_gets_answer_and_task_goes_to_planner() {
        assert_eq!(
            parse_triage(r#"{"kind":"chat","reply":"Да, работаю"}"#).as_deref(),
            Some("Да, работаю")
        );
        assert!(parse_triage(r#"{"kind":"chat","reply":"  "}"#).is_some());
        assert!(parse_triage(r#"{"kind":"task"}"#).is_none());
        assert!(parse_triage("не json").is_none());
    }

    #[test]
    fn parses_graph_with_fences_and_text() {
        let raw = r#"Вот план:
```json
{"goal":"установить игру","nodes":[
 {"id":"disk","goal":"проверить место","done_when":"видно свободно","deps":[]},
 {"id":"dl","goal":"скачать","done_when":"файл в загрузках","deps":["disk"],"rollback":"удалить файл"}],
 "risks":["мало места"]}
```"#;
        let g = parse_graph(raw, "поставь игру").expect("граф должен разобраться");
        assert_eq!(g.nodes.len(), 2);
        assert_eq!(g.next().map(|n| n.id.clone()), Some("disk".into()));
    }

    #[test]
    fn cyclic_plan_from_model_is_error_not_panic() {
        let raw = r#"{"goal":"g","nodes":[{"id":"a","goal":"a","deps":["b"]},{"id":"b","goal":"b","deps":["a"]}]}"#;
        assert!(parse_graph(raw, "t").is_err());
        assert!(parse_graph("вообще не json", "t").is_err());
        assert!(parse_graph(r#"{"goal":"g","nodes":[]}"#, "t").is_err());
    }

    #[test]
    fn replan_keeps_history_and_renames_new_nodes() {
        let mut old = TaskGraph::linear(
            "цель",
            &[
                ("шаг 1".into(), "видно 1".into()),
                ("шаг 2".into(), "".into()),
            ],
        );
        assert!(old.validate().is_ok());
        old.mark_done("n1", "готово");
        let fresh = parse_graph(
            r#"{"goal":"g","nodes":[{"id":"n1","goal":"иначе","done_when":"видно"}]}"#,
            "t",
        )
        .expect("парсинг");
        let mut merged = merge(&old, fresh);
        assert!(merged.validate().is_ok(), "склейка должна быть валидной");
        // История сохранена, новый узел не столкнулся с ней по id.
        assert!(merged.nodes.iter().any(|n| n.id == "n1"));
        assert!(merged.nodes.iter().any(|n| n.id == "r1_n1"));
        assert_eq!(merged.next().map(|n| n.id.clone()), Some("r1_n1".into()));
    }
}
