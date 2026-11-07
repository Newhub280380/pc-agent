//! Доказательный контур: каталог операторов и планировщик без модели в цикле.
//!
//! Модель не решает, что делать и что «получилось». Для известных типов задач
//! маршрут фиксирован каталогом операторов (предусловия → эффекты), а Z3
//! доказывает порядок шагов. Исполнение и закрытие остаются за ядром: экран,
//! наблюдение, `check_plan`, `check_solution` и эндоскоп. Неизвестная задача
//! сюда не попадает и уходит в обычный LLM-планировщик.

use crate::agent::graph::{Node, NodeState, TaskGraph};
use std::collections::HashSet;
use z3::ast::{Ast, Bool, Int};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    InstagramPost,
    MetricsSnapshot,
    DirectInbox,
    RenderReel,
    DeployCrm,
}

impl TaskKind {
    fn name(self) -> &'static str {
        match self {
            Self::InstagramPost => "instagram_post",
            Self::MetricsSnapshot => "metrics_snapshot",
            Self::DirectInbox => "direct_inbox",
            Self::RenderReel => "render_reel",
            Self::DeployCrm => "deploy_crm",
        }
    }

    fn goal_fact(self) -> &'static str {
        match self {
            Self::InstagramPost => "goal:instagram_post",
            Self::MetricsSnapshot => "goal:metrics_snapshot",
            Self::DirectInbox => "goal:direct_inbox",
            Self::RenderReel => "goal:render_reel",
            Self::DeployCrm => "goal:deploy_crm",
        }
    }

    fn initial_facts(self) -> Vec<&'static str> {
        let base = vec!["env:pc", "env:browser"];
        match self {
            Self::InstagramPost | Self::MetricsSnapshot | Self::DirectInbox => {
                let mut facts = base;
                facts.push("env:instagram_account");
                facts
            }
            Self::RenderReel => {
                let mut facts = base;
                facts.push("env:storage");
                facts
            }
            Self::DeployCrm => {
                let mut facts = base;
                facts.push("env:git");
                facts.push("env:vercel");
                facts
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Operator {
    kind: TaskKind,
    id: &'static str,
    goal: &'static str,
    done_when: &'static str,
    requires: &'static [&'static str],
    produces: &'static [&'static str],
    rollback: &'static str,
    time_min: u32,
    cost: u32,
    risk: u32,
}

const CATALOG: &[Operator] = &[
    // --- Instagram: публикация проверяется появлением поста, а не текстом отчёта.
    Operator {
        kind: TaskKind::InstagramPost,
        id: "ig_open",
        goal: "Открыть Instagram в браузере",
        done_when: "на экране видна кнопка «Новая публикация» или профиль аккаунта",
        requires: &[],
        produces: &["ig:session"],
        rollback: "закрыть вкладку Instagram",
        time_min: 2,
        cost: 1,
        risk: 5,
    },
    Operator {
        kind: TaskKind::InstagramPost,
        id: "ig_check_account",
        goal: "Проверить аккаунт beauty_art.kz",
        done_when: "на экране виден «beauty_art.kz»",
        requires: &["ig:session"],
        produces: &["ig:account_ok"],
        rollback: "",
        time_min: 1,
        cost: 1,
        risk: 5,
    },
    Operator {
        kind: TaskKind::InstagramPost,
        id: "ig_prepare_asset",
        goal: "Подготовить медиа и подпись публикации",
        done_when: "на экране виден готовый пост с медиа и подписью",
        requires: &["ig:account_ok"],
        produces: &["ig:asset_ready"],
        rollback: "удалить черновик публикации",
        time_min: 5,
        cost: 2,
        risk: 10,
    },
    Operator {
        kind: TaskKind::InstagramPost,
        id: "ig_open_composer",
        goal: "Открыть форму публикации",
        done_when: "на экране видна кнопка «Опубликовать»",
        requires: &["ig:asset_ready"],
        produces: &["ig:composer_open"],
        rollback: "закрыть форму публикации",
        time_min: 2,
        cost: 1,
        risk: 15,
    },
    Operator {
        kind: TaskKind::InstagramPost,
        id: "ig_fill_post",
        goal: "Перенести медиа и подпись в форму",
        done_when: "в форме видны выбранный медиафайл и подпись поста",
        requires: &["ig:composer_open"],
        produces: &["ig:post_ready"],
        rollback: "очистить форму публикации",
        time_min: 4,
        cost: 1,
        risk: 20,
    },
    Operator {
        kind: TaskKind::InstagramPost,
        id: "ig_request_approval",
        goal: "Показать человеку пост на утверждение",
        done_when: "на экране видно подтверждение человека «публиковать»",
        requires: &["ig:post_ready"],
        produces: &["ig:approval"],
        rollback: "снять публикацию с утверждения",
        time_min: 3,
        cost: 1,
        risk: 30,
    },
    Operator {
        kind: TaskKind::InstagramPost,
        id: "ig_publish",
        goal: "Опубликовать пост и проверить появление",
        done_when: "на экране видна надпись «Публикация опубликована» или пост виден в профиле",
        requires: &["ig:approval"],
        produces: &["ig:published", "goal:instagram_post"],
        rollback: "удалить опубликованный пост",
        time_min: 3,
        cost: 2,
        risk: 40,
    },
    // --- Метрики: закрываются сохранённым срезом чисел, не словами «посмотрел».
    Operator {
        kind: TaskKind::MetricsSnapshot,
        id: "metrics_open",
        goal: "Открыть статистику аккаунта",
        done_when: "на экране виден раздел «Статистика» или «Insights»",
        requires: &[],
        produces: &["metrics:open"],
        rollback: "вернуться в ленту Instagram",
        time_min: 2,
        cost: 1,
        risk: 5,
    },
    Operator {
        kind: TaskKind::MetricsSnapshot,
        id: "metrics_period",
        goal: "Выбрать период отчёта",
        done_when: "на экране выбран период «7 дней» или «28 дней»",
        requires: &["metrics:open"],
        produces: &["metrics:period"],
        rollback: "сбросить фильтр периода",
        time_min: 1,
        cost: 1,
        risk: 5,
    },
    Operator {
        kind: TaskKind::MetricsSnapshot,
        id: "metrics_reach",
        goal: "Снять охват и показы",
        done_when: "на экране видны числа охвата и показов за период",
        requires: &["metrics:period"],
        produces: &["metrics:reach"],
        rollback: "",
        time_min: 2,
        cost: 1,
        risk: 10,
    },
    Operator {
        kind: TaskKind::MetricsSnapshot,
        id: "metrics_engagement",
        goal: "Снять реакции, комментарии и сохранения",
        done_when: "на экране видны лайки, комментарии и сохранения",
        requires: &["metrics:period"],
        produces: &["metrics:engagement"],
        rollback: "",
        time_min: 2,
        cost: 1,
        risk: 10,
    },
    Operator {
        kind: TaskKind::MetricsSnapshot,
        id: "metrics_save",
        goal: "Сохранить срез метрик в отчёт",
        done_when: "на экране виден статус «Сохранено» или файл отчёта в загрузках",
        requires: &["metrics:reach", "metrics:engagement"],
        produces: &["goal:metrics_snapshot"],
        rollback: "удалить последний отчёт",
        time_min: 3,
        cost: 1,
        risk: 15,
    },
    // --- Direct: только чтение и черновики. Отправка живым людям не оператор.
    Operator {
        kind: TaskKind::DirectInbox,
        id: "direct_open",
        goal: "Открыть Direct Instagram",
        done_when: "на экране виден список диалогов Direct",
        requires: &[],
        produces: &["direct:open"],
        rollback: "закрыть Direct",
        time_min: 2,
        cost: 1,
        risk: 5,
    },
    Operator {
        kind: TaskKind::DirectInbox,
        id: "direct_capture",
        goal: "Снять новые диалоги и непрочитанные",
        done_when: "на экране виден список диалогов с отметками прочитано или непрочитано",
        requires: &["direct:open"],
        produces: &["direct:captured"],
        rollback: "",
        time_min: 3,
        cost: 1,
        risk: 15,
    },
    Operator {
        kind: TaskKind::DirectInbox,
        id: "direct_classify",
        goal: "Разложить диалоги по лидам и кодовым словам",
        done_when: "в таблице или CRM видны метки лида и черновики ответов",
        requires: &["direct:captured"],
        produces: &["direct:classified"],
        rollback: "откатить черновики ответов",
        time_min: 6,
        cost: 2,
        risk: 25,
    },
    Operator {
        kind: TaskKind::DirectInbox,
        id: "direct_update_crm",
        goal: "Обновить CRM по итогам Direct",
        done_when: "в CRM видно обновлённое число лидов из Direct",
        requires: &["direct:classified"],
        produces: &["goal:direct_inbox"],
        rollback: "откатить последнюю запись CRM",
        time_min: 4,
        cost: 2,
        risk: 30,
    },
    // --- Рендер: закрывается файлом/статусом, а не фразой «рендер идёт».
    Operator {
        kind: TaskKind::RenderReel,
        id: "render_inputs",
        goal: "Проверить исходники ролика",
        done_when: "на экране видны исходное видео, фото и субтитры",
        requires: &[],
        produces: &["render:inputs"],
        rollback: "вернуть исходники в исходное состояние",
        time_min: 2,
        cost: 1,
        risk: 10,
    },
    Operator {
        kind: TaskKind::RenderReel,
        id: "render_submit",
        goal: "Запустить рендер",
        done_when: "на экране виден статус «В рендере» или номер задания",
        requires: &["render:inputs"],
        produces: &["render:job"],
        rollback: "остановить задание рендера",
        time_min: 2,
        cost: 2,
        risk: 20,
    },
    Operator {
        kind: TaskKind::RenderReel,
        id: "render_wait",
        goal: "Дождаться готового файла",
        done_when: "на экране виден статус «Готово» или файл результата в папке",
        requires: &["render:job"],
        produces: &["render:output"],
        rollback: "удалить неполный файл результата",
        time_min: 20,
        cost: 3,
        risk: 30,
    },
    Operator {
        kind: TaskKind::RenderReel,
        id: "render_verify",
        goal: "Проверить длительность и разрешение ролика",
        done_when: "в отчёте видны длительность и разрешение готового ролика",
        requires: &["render:output"],
        produces: &["render:verified"],
        rollback: "",
        time_min: 2,
        cost: 1,
        risk: 20,
    },
    Operator {
        kind: TaskKind::RenderReel,
        id: "render_package",
        goal: "Упаковать ролик для публикации",
        done_when: "на экране виден архив ролика или ссылка на готовый файл",
        requires: &["render:verified"],
        produces: &["goal:render_reel"],
        rollback: "удалить архив ролика",
        time_min: 3,
        cost: 1,
        risk: 15,
    },
    // --- CRM deploy: прод поднимается только после проверок и смоука.
    Operator {
        kind: TaskKind::DeployCrm,
        id: "crm_repo",
        goal: "Проверить репозиторий CRM",
        done_when: "в выводе виден чистый git status и ветка main",
        requires: &[],
        produces: &["crm:repo_ok"],
        rollback: "вернуть незакоммиченные изменения",
        time_min: 2,
        cost: 1,
        risk: 10,
    },
    Operator {
        kind: TaskKind::DeployCrm,
        id: "crm_checks",
        goal: "Прогнать проверки перед деплоем",
        done_when: "на экране виден результат тестов «passed»",
        requires: &["crm:repo_ok"],
        produces: &["crm:checks"],
        rollback: "",
        time_min: 8,
        cost: 2,
        risk: 20,
    },
    Operator {
        kind: TaskKind::DeployCrm,
        id: "crm_preview",
        goal: "Поднять превью-деплой",
        done_when: "на экране виден URL превью или статус «Ready»",
        requires: &["crm:checks"],
        produces: &["crm:preview"],
        rollback: "остановить превью-деплой",
        time_min: 5,
        cost: 2,
        risk: 25,
    },
    Operator {
        kind: TaskKind::DeployCrm,
        id: "crm_smoke",
        goal: "Пройти смоук-тест CRM",
        done_when: "на экране открыт CRM и виден вход или дашборд",
        requires: &["crm:preview"],
        produces: &["crm:smoke"],
        rollback: "",
        time_min: 4,
        cost: 1,
        risk: 30,
    },
    Operator {
        kind: TaskKind::DeployCrm,
        id: "crm_prod",
        goal: "Выпустить продакшен CRM",
        done_when: "на экране виден продакшен-домен «beautyarts-crm.vercel.app»",
        requires: &["crm:smoke"],
        produces: &["goal:deploy_crm"],
        rollback: "откатить продакшен-деплой",
        time_min: 6,
        cost: 3,
        risk: 50,
    },
];

fn stem_hit(tokens: &[&str], stems: &[&str]) -> bool {
    tokens.iter().copied().any(|tok| {
        stems
            .iter()
            .copied()
            .any(|stem| tok.starts_with(stem) || tok.contains(stem))
    })
}

fn exact_hit(tokens: &[&str], words: &[&str]) -> bool {
    tokens
        .iter()
        .copied()
        .any(|tok| words.iter().copied().any(|word| tok == word))
}

/// Классифицирует задачу в один из пяти доказанных маршрутов. Всё остальное —
/// `None`, и планировщик уходит к обычной LLM-схеме. Маршрут детерминированный,
/// поэтому сюда попадает только задача с явными признаками нужного домена.
pub fn classify(task: &str) -> Option<TaskKind> {
    let t = task.to_lowercase();
    let tokens: Vec<&str> = t
        .split(|c: char| !c.is_alphanumeric())
        .filter(|tok| !tok.is_empty())
        .collect();
    let instagram = stem_hit(&tokens, &["instagram", "инстаграм", "инста", "инсте"]);
    let direct = stem_hit(&tokens, &["директ", "личк"]) || exact_hit(&tokens, &["direct", "dm"]);
    let metrics = stem_hit(&tokens, &["метрик", "insight", "статистик", "охват"]);
    let account = stem_hit(&tokens, &["аккаунт", "профил", "страниц"]);
    let render = stem_hit(
        &tokens,
        &["ролик", "видео", "рендер", "render", "swap", "лица"],
    ) || t.contains("face swap");
    let render_action = stem_hit(
        &tokens,
        &["рендер", "render", "swap", "замен", "лица", "face"],
    );
    let crm = exact_hit(&tokens, &["crm"]);
    let deploy = stem_hit(&tokens, &["депло", "deploy", "vercel"]);

    if direct {
        return Some(TaskKind::DirectInbox);
    }
    if metrics && (instagram || account) {
        return Some(TaskKind::MetricsSnapshot);
    }
    if render && render_action {
        return Some(TaskKind::RenderReel);
    }
    if crm && (deploy || stem_hit(&tokens, &["продакшн", "production"])) {
        return Some(TaskKind::DeployCrm);
    }
    if instagram
        && (exact_hit(&tokens, &["пост", "поста", "посты", "постов"])
            || stem_hit(&tokens, &["публикац", "publish", "рилс", "reel"]))
    {
        return Some(TaskKind::InstagramPost);
    }
    None
}

/// Строит `TaskGraph` для известной задачи без вызова модели. `None` — задача
/// вне каталога, дальше работает обычный LLM-планировщик.
pub fn plan(task: &str) -> Option<TaskGraph> {
    let kind = classify(task)?;
    let ops: Vec<Operator> = CATALOG
        .iter()
        .copied()
        .filter(|op| op.kind == kind)
        .collect();
    let order = solve_order(&ops, &kind.initial_facts(), kind.goal_fact())?;
    let mut nodes = Vec::with_capacity(order.len());
    for (pos, &op_idx) in order.iter().enumerate() {
        let op = ops[op_idx];
        let mut deps = Vec::new();
        let mut seen = HashSet::new();
        for &req in op.requires {
            if kind.initial_facts().contains(&req) {
                continue;
            }
            for &prev_idx in &order[..pos] {
                let prev = ops[prev_idx];
                if prev.produces.contains(&req) && seen.insert(prev.id) {
                    deps.push(prev.id.to_string());
                }
            }
        }
        nodes.push(Node {
            id: op.id.to_string(),
            goal: op.goal.to_string(),
            done_when: op.done_when.to_string(),
            deps,
            rollback: op.rollback.to_string(),
            state: NodeState::Pending,
            note: String::new(),
            attempts: 0,
            rounds: 0,
            time_min: op.time_min,
            cost: op.cost,
            risk: op.risk,
            variants: Vec::new(),
            chosen: String::new(),
        });
    }
    let mut graph = TaskGraph {
        goal: task.trim().to_string(),
        nodes,
        risks: vec![format!("детерминированный маршрут {}", kind.name())],
        completed_order: Vec::new(),
    };
    graph.validate().ok()?;
    Some(graph)
}

fn solve_order(ops: &[Operator], initial: &[&str], goal: &str) -> Option<Vec<usize>> {
    if ops.is_empty() || ops.len() > 12 {
        return None;
    }
    if !ops.iter().any(|op| op.produces.contains(&goal)) {
        return None;
    }
    let initial_set: HashSet<&str> = initial.iter().copied().collect();
    crate::logic::with_solver(|ctx, solver| {
        let m = ops.len() as i64;
        let zero = Int::from_i64(ctx, 0);
        let limit = Int::from_i64(ctx, m);
        let pos: Vec<Int> = (0..ops.len())
            .map(|i| Int::new_const(ctx, format!("proof_pos_{i}")))
            .collect();
        for p in &pos {
            solver.assert(&p.ge(&zero));
            solver.assert(&p.lt(&limit));
        }
        for i in 0..pos.len() {
            for j in 0..i {
                solver.assert(&pos[i]._eq(&pos[j]).not());
            }
        }
        for (i, op) in ops.iter().enumerate() {
            for &req in op.requires {
                let mut choices: Vec<Bool> = Vec::new();
                if initial_set.contains(req) {
                    choices.push(Bool::from_bool(ctx, true));
                }
                for (j, producer) in ops.iter().enumerate() {
                    if j != i && producer.produces.contains(&req) {
                        choices.push(pos[j].lt(&pos[i]));
                    }
                }
                if choices.is_empty() {
                    return None;
                }
                let refs: Vec<&Bool> = choices.iter().collect();
                solver.assert(&Bool::or(ctx, &refs));
            }
        }
        if solver.check() != z3::SatResult::Sat {
            return None;
        }
        let model = solver.get_model()?;
        let mut keyed = Vec::with_capacity(ops.len());
        for (i, p) in pos.iter().enumerate() {
            let value = model.eval(p, true)?.as_i64()?;
            if value < 0 || value >= m {
                return None;
            }
            keyed.push((value, i));
        }
        keyed.sort_by_key(|(value, _)| *value);
        let order: Vec<usize> = keyed.into_iter().map(|(_, i)| i).collect();
        if order.len() != ops.len() {
            return None;
        }
        Some(order)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_known_tasks() {
        assert_eq!(
            classify("сделай пост в instagram"),
            Some(TaskKind::InstagramPost)
        );
        assert_eq!(
            classify("сними метрики instagram за неделю"),
            Some(TaskKind::MetricsSnapshot)
        );
        assert_eq!(
            classify("прочитай директ и разложи лиды"),
            Some(TaskKind::DirectInbox)
        );
        assert_eq!(
            classify("отрендери ролик с заменой лица"),
            Some(TaskKind::RenderReel)
        );
        assert_eq!(
            classify("задеплой crm на vercel"),
            Some(TaskKind::DeployCrm)
        );
        assert_eq!(classify("привет, как дела"), None);
        assert_eq!(classify("постоянно проверяй crm"), None);
        assert_eq!(classify("сделай пост"), None);
        assert_eq!(classify("сделай видео для сайта"), None);
    }

    #[test]
    fn instagram_plan_is_deterministic_and_valid() {
        let task = "сделай пост в instagram";
        let first = plan(task).expect("план Instagram должен строиться");
        let second = plan(task).expect("план Instagram должен строиться");
        let ids_first: Vec<_> = first.nodes.iter().map(|n| n.id.as_str()).collect();
        let ids_second: Vec<_> = second.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids_first, ids_second);
        assert!(ids_first.contains(&"ig_publish"));
        assert!(ids_first.contains(&"ig_open"));
        assert!(first.nodes.iter().all(|n| !n.done_when.trim().is_empty()));
    }

    #[test]
    fn metrics_plan_respects_dependencies() {
        let graph = plan("сними метрики instagram охвата и сохранений").expect("план метрик");
        let save = graph
            .nodes
            .iter()
            .find(|n| n.id == "metrics_save")
            .expect("узел metrics_save");
        assert!(save.deps.iter().any(|d| d == "metrics_reach"));
        assert!(save.deps.iter().any(|d| d == "metrics_engagement"));
        let reach_pos = graph
            .nodes
            .iter()
            .position(|n| n.id == "metrics_reach")
            .unwrap();
        let save_pos = graph
            .nodes
            .iter()
            .position(|n| n.id == "metrics_save")
            .unwrap();
        assert!(reach_pos < save_pos);
    }

    #[test]
    fn unknown_task_has_no_proof_plan() {
        assert!(plan("придумай слоган для новой коллекции").is_none());
    }
}
