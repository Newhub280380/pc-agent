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
    InstagramResearch,
    MetricsSnapshot,
    DirectInbox,
    RenderReel,
    DeployCrm,
}

impl TaskKind {
    fn name(self) -> &'static str {
        match self {
            Self::InstagramPost => "instagram_post",
            Self::InstagramResearch => "instagram_research",
            Self::MetricsSnapshot => "metrics_snapshot",
            Self::DirectInbox => "direct_inbox",
            Self::RenderReel => "render_reel",
            Self::DeployCrm => "deploy_crm",
        }
    }

    fn goal_fact(self) -> &'static str {
        match self {
            Self::InstagramPost => "goal:instagram_post",
            Self::InstagramResearch => "goal:instagram_research",
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
            Self::InstagramResearch => {
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

    pub fn as_str(self) -> &'static str {
        self.name()
    }

    /// Внешние предпосылки, которые маршрут не может получить сам.
    pub fn needs(self) -> Vec<(&'static str, &'static str)> {
        let mut needs = vec![
            (
                "env:pc",
                "Доступен компьютер, на котором выполняется задача",
            ),
            (
                "env:browser",
                "Доступен браузер для работы с веб-интерфейсом",
            ),
        ];
        match self {
            Self::InstagramPost => {
                needs.extend([
                    (
                        "env:instagram_account",
                        "Доступна авторизованная сессия нужного аккаунта Instagram",
                    ),
                    ("env:media_file", "Предоставлен медиафайл для публикации"),
                ]);
            }
            Self::InstagramResearch => needs.push((
                "env:instagram_account",
                "Доступна авторизованная сессия исследуемого аккаунта Instagram",
            )),
            Self::MetricsSnapshot | Self::DirectInbox => needs.push((
                "env:instagram_account",
                "Доступна авторизованная сессия нужного аккаунта Instagram",
            )),
            Self::RenderReel => {
                needs.extend([
                    (
                        "env:storage",
                        "Доступно хранилище для исходников и результата",
                    ),
                    (
                        "env:media_file",
                        "Предоставлены исходные видео, фото и субтитры",
                    ),
                ]);
            }
            Self::DeployCrm => {
                needs.extend([
                    ("env:git", "Доступен Git и нужный репозиторий CRM"),
                    (
                        "env:vercel",
                        "Есть доступ к Vercel, включая авторизованный токен",
                    ),
                ]);
            }
        }
        needs
    }
}

#[derive(Debug)]
pub enum Routing {
    Route {
        kind: TaskKind,
        graph: TaskGraph,
    },
    Missing {
        kind: TaskKind,
        missing: Vec<String>,
    },
    Unknown,
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
    Operator {
        kind: TaskKind::InstagramResearch,
        id: "research_open_profile",
        goal: "Открыть профиль Instagram для исследования",
        done_when: "на экране виден профиль исследуемого аккаунта Instagram",
        requires: &[],
        produces: &["research:profile"],
        rollback: "закрыть профиль Instagram",
        time_min: 2,
        cost: 1,
        risk: 5,
    },
    Operator {
        kind: TaskKind::InstagramResearch,
        id: "research_list_reels",
        goal: "Открыть список последних роликов профиля",
        done_when: "на экране виден список запрошенных последних роликов",
        requires: &["research:profile"],
        produces: &["research:reels", "goal:instagram_research"],
        rollback: "вернуться в профиль Instagram",
        time_min: 2,
        cost: 1,
        risk: 5,
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

/// Классифицирует задачу в один из известных доказанных маршрутов. Всё остальное —
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
    let research_action = stem_hit(&tokens, &["изуч", "анализ", "разбер", "исслед"]);
    let research_subject = instagram || stem_hit(&tokens, &["рилс", "reel"]);
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
    if research_action && research_subject {
        return Some(TaskKind::InstagramResearch);
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
    if kind == TaskKind::InstagramResearch {
        return None;
    }
    plan_kind(task, kind)
}

fn plan_kind(task: &str, kind: TaskKind) -> Option<TaskGraph> {
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

/// Строит маршрут только при наличии всех внешних предпосылок.
pub fn route(task: &str, have: &[String]) -> Routing {
    let Some(kind) = classify(task) else {
        return Routing::Unknown;
    };
    let missing: Vec<String> = kind
        .needs()
        .into_iter()
        .filter(|(fact, _)| !have.iter().any(|provided| provided == fact))
        .map(|(_, description)| description.to_string())
        .collect();
    if !missing.is_empty() {
        return Routing::Missing { kind, missing };
    }
    let graph = if kind == TaskKind::InstagramResearch {
        build_research_plan(task)
    } else {
        plan(task)
    };
    match graph {
        Some(graph) => Routing::Route { kind, graph },
        None => Routing::Unknown,
    }
}

fn build_research_plan(task: &str) -> Option<TaskGraph> {
    let mut graph = plan_kind(task, TaskKind::InstagramResearch)?;
    let reel_count = requested_reel_count(task);
    let list_index = graph
        .nodes
        .iter()
        .position(|node| node.id == "research_list_reels")?;
    graph.nodes[list_index].goal = format!("Показать последние ролики профиля (N={reel_count})");
    graph.nodes[list_index].done_when =
        format!("на экране виден список последних роликов профиля (N={reel_count})");

    let reel_ids: Vec<String> = (1..=reel_count).map(|i| format!("reel_{i}")).collect();
    let reels = reel_ids.iter().enumerate().map(|(i, id)| Node {
        id: id.clone(),
        goal: format!("Досмотреть ролик №{} до конца и сохранить расшифровку", i + 1),
        done_when: "«просмотрен целиком, включая последнюю минуту»; виден конец ролика: «таймкод равен длительности»; «расшифровка ролика сохранена»".to_string(),
        deps: Vec::new(),
        rollback: String::new(),
        state: NodeState::Pending,
        note: String::new(),
        attempts: 0,
        rounds: 0,
        time_min: 1,
        cost: 1,
        risk: 5,
        variants: Vec::new(),
        chosen: String::new(),
    });
    for (offset, reel) in reels.enumerate() {
        graph.nodes.insert(list_index + 1 + offset, reel);
    }
    graph.nodes.push(Node {
        id: "research_summary".to_string(),
        goal: format!("Составить выводы по последним роликам (N={reel_count})"),
        done_when: "в отчёте выполнено «у каждого вывода указан номер ролика и таймкод»"
            .to_string(),
        deps: reel_ids,
        rollback: String::new(),
        state: NodeState::Pending,
        note: String::new(),
        attempts: 0,
        rounds: 0,
        time_min: 3,
        cost: 1,
        risk: 5,
        variants: Vec::new(),
        chosen: String::new(),
    });
    graph.validate().ok()?;
    Some(graph)
}

fn requested_reel_count(task: &str) -> usize {
    let lowercase = task.to_lowercase();
    let tokens: Vec<&str> = lowercase
        .split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect();
    let count = tokens.iter().enumerate().find_map(|(index, token)| {
        if token.chars().all(|c| c.is_ascii_digit())
            && tokens.iter().skip(index + 1).take(5).any(|next| {
                ["ролик", "рилс", "reel", "видео", "публикац", "пост"]
                    .iter()
                    .any(|stem| next.starts_with(stem) || next.contains(stem))
            })
        {
            Some(token.parse::<usize>().unwrap_or(12))
        } else {
            None
        }
    });
    count.unwrap_or(10).clamp(1, 12)
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

    fn task_for(kind: TaskKind) -> &'static str {
        match kind {
            TaskKind::InstagramPost => "сделай пост в instagram",
            TaskKind::InstagramResearch => "исследуй аккаунт instagram",
            TaskKind::MetricsSnapshot => "сними метрики instagram",
            TaskKind::DirectInbox => "прочитай директ instagram",
            TaskKind::RenderReel => "отрендери ролик с заменой лица",
            TaskKind::DeployCrm => "задеплой crm на vercel",
        }
    }

    fn have_for(kind: TaskKind) -> Vec<String> {
        kind.needs()
            .into_iter()
            .map(|(fact, _)| fact.to_string())
            .collect()
    }

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
        assert_eq!(
            classify("изучи аккаунт Instagram и последние рилс"),
            Some(TaskKind::InstagramResearch)
        );
        assert_eq!(classify("проанализируй ролик"), None);
        assert_eq!(classify("проанализируй ролик с камеры"), None);
        assert_eq!(
            classify("проанализируй ролик в instagram"),
            Some(TaskKind::InstagramResearch)
        );
        assert_eq!(classify("исследуй новые методы"), None);
        assert_eq!(classify("разбери файл отчёта"), None);
    }

    #[test]
    fn routing_requires_every_declared_fact_for_each_kind() {
        for kind in [
            TaskKind::InstagramPost,
            TaskKind::InstagramResearch,
            TaskKind::MetricsSnapshot,
            TaskKind::DirectInbox,
            TaskKind::RenderReel,
            TaskKind::DeployCrm,
        ] {
            let have = have_for(kind);
            match route(task_for(kind), &have) {
                Routing::Route {
                    kind: actual,
                    mut graph,
                } => {
                    assert_eq!(actual, kind);
                    assert!(graph.validate().is_ok());
                }
                other => panic!("ожидался маршрут {kind:?}, получено {other:?}"),
            }

            match route(task_for(kind), &[]) {
                Routing::Missing {
                    kind: actual,
                    missing,
                } => {
                    assert_eq!(actual, kind);
                    assert_eq!(missing.len(), kind.needs().len());
                }
                other => panic!("ожидались недостающие предпосылки {kind:?}: {other:?}"),
            }
        }
        assert!(matches!(route("придумай слоган", &[]), Routing::Unknown));
    }

    #[test]
    fn research_route_has_capped_full_watch_steps_and_summary_dependencies() {
        let kind = TaskKind::InstagramResearch;
        let have = have_for(kind);
        let Routing::Route { mut graph, .. } = route("исследуй последние 20 reels", &have)
        else {
            panic!("ожидался маршрут исследования");
        };
        let reels: Vec<_> = graph
            .nodes
            .iter()
            .filter(|node| node.id.starts_with("reel_"))
            .collect();
        assert_eq!(reels.len(), 12);
        assert!(reels.iter().all(|node| {
            node.done_when.contains("включая последнюю минуту")
                && node.done_when.contains("таймкод равен длительности")
                && node.done_when.contains("расшифровка ролика сохранена")
        }));
        let summary = graph
            .nodes
            .iter()
            .find(|node| node.id == "research_summary")
            .unwrap();
        assert_eq!(summary.deps.len(), 12);
        assert!(reels.iter().all(|reel| summary.deps.contains(&reel.id)));
        assert!(summary
            .done_when
            .contains("у каждого вывода указан номер ролика и таймкод"));
        assert!(graph.validate().is_ok());
    }

    #[test]
    fn research_reel_count_defaults_to_ten_and_caps_at_twelve() {
        assert_eq!(requested_reel_count("исследуй reels Instagram"), 10);
        assert_eq!(requested_reel_count("исследуй 3 reels Instagram"), 3);
        assert_eq!(requested_reel_count("исследуй 99 reels Instagram"), 12);
        assert_eq!(
            requested_reel_count("изучи instagram beauty_art2024 и 5 последних роликов"),
            5
        );
        assert_eq!(requested_reel_count("изучи instagram user123"), 10);
        assert_eq!(
            requested_reel_count("исследуй 3 самых последних опубликованных reels Instagram"),
            3
        );
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
        assert!(plan("изучи аккаунт Instagram").is_none());
    }
}

