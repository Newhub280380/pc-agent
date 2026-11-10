//! Координатор: единственная точка, где задача получает маршрут.
//!
//! Порядок жёсткий и не зависит от модели:
//!   1. доказанный маршрут из каталога операторов (`proof`);
//!   2. план из журнала — эта же задача уже закрывалась по доказательству;
//!   3. план модели — только в открытом режиме и с пометкой «не доказан».
//! В строгом режиме (`PCAGENT_ROUTE=strict`) третьего пункта нет: задача без
//! доказанного маршрута честно остаётся открытой, а не выполняется по
//! фантазии модели.
//!
//! Второе — контракт глубины. Если человек просит обойти сайт целиком,
//! список разделов собирает код с экрана, а не модель. Задача не закроется,
//! пока по каждому разделу не было подтверждённого перехода.

use crate::agent::graph::TaskGraph;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Open,
    Strict,
}

impl Mode {
    pub fn from_env() -> Self {
        match std::env::var("PCAGENT_ROUTE")
            .unwrap_or_default()
            .trim()
            .to_lowercase()
            .as_str()
        {
            "strict" | "proof" | "строго" | "строгий" => Mode::Strict,
            _ => Mode::Open,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Proof,
    Journal,
    Model,
}

impl Route {
    pub fn name(self) -> &'static str {
        match self {
            Route::Proof => "доказанный маршрут (каталог операторов + Z3)",
            Route::Journal => "журнал доказанных прогонов",
            Route::Model => "план модели (не доказан, проверяется Z3 и экраном)",
        }
    }
}

#[derive(Debug)]
pub enum Routed {
    Plan(TaskGraph, Route),
    /// Доказанного пути нет, разрешено спросить планировщик-модель.
    Model,
    Refused(String),
}

pub fn route(mode: Mode, proof: Option<TaskGraph>, journal: Option<TaskGraph>) -> Routed {
    if let Some(g) = proof {
        return Routed::Plan(g, Route::Proof);
    }
    if let Some(g) = journal {
        return Routed::Plan(g, Route::Journal);
    }
    match mode {
        Mode::Open => Routed::Model,
        Mode::Strict => Routed::Refused(
            "строгий режим: для этой задачи нет доказанного маршрута и нет прогона в журнале, \
             модель сама маршрут не выбирает"
                .into(),
        ),
    }
}

const DEPTH_MARKERS: &[&str] = &[
    "весь сайт",
    "всему сайту",
    "все страниц",
    "всем страниц",
    "каждую страниц",
    "каждой страниц",
    "все раздел",
    "всем раздел",
    "каждый раздел",
    "все ссылк",
    "до самого дна",
    "до дна",
    "до конца сайта",
    "полностью обойди",
    "обойди сайт",
    "whole site",
    "all pages",
    "every page",
    "crawl",
];

/// Просит ли задача обход до конца. Ложное «да» безопасно — задача лишь
/// строже закрывается; ложное «нет» — только явными словами, без догадок.
pub fn wants_depth(task: &str) -> bool {
    let t = task.to_lowercase();
    DEPTH_MARKERS.iter().any(|m| t.contains(m))
}

/// Сколько разделов держим в контракте. Больше — обход не доказуем за один
/// прогон, и задача честно остаётся открытой с перечнем непройденного.
pub const MAX_SECTIONS: usize = 40;

#[derive(Debug, Clone, Default)]
pub struct Coverage {
    /// Разделы стартовой страницы: нормализованное имя → как было на экране.
    sections: BTreeMap<String, String>,
    overflow: usize,
    /// Заголовки окон, которые реально были на экране.
    titles: BTreeSet<String>,
    rooted: bool,
}

fn norm(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn window_title(prompt: &str) -> Option<String> {
    for line in prompt.lines() {
        if let Some(idx) = line.find("Активное окно: \"") {
            let after = &line[idx + "Активное окно: \"".len()..];
            if let Some(end) = after.rfind('"') {
                return Some(norm(&after[..end]));
            }
        }
        if let Some(rest) = line.trim().strip_prefix("ЭКРАН ТЕЛЕФОНА. Активное: ")
        {
            return Some(norm(rest));
        }
    }
    None
}

fn links(prompt: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_elements = false;
    for raw in prompt.lines() {
        let line = raw.trim();
        if line.starts_with("ЭЛЕМЕНТЫ") {
            in_elements = true;
            continue;
        }
        if line.starts_with("ТЕКСТ НА ЭКРАНЕ") || line.starts_with("СОДЕРЖИМОЕ ПОЛЕЙ")
        {
            in_elements = false;
        }
        if !in_elements {
            continue;
        }
        let Some(rest) = line.strip_prefix("- ") else {
            continue;
        };
        let mut parts = rest.split(" | ");
        let name = parts.next().unwrap_or("").trim();
        let kind = parts.next().unwrap_or("").trim().to_lowercase();
        if kind == "link" && name.chars().count() >= 2 {
            out.push(name.to_string());
        }
    }
    out
}

impl Coverage {
    /// Первый экран со ссылками задаёт контракт; дальше только копим
    /// заголовки, чтобы новые ссылки глубже не раздували его бесконечно.
    pub fn observe(&mut self, prompt: &str) {
        if let Some(t) = window_title(prompt) {
            if !t.is_empty() {
                self.titles.insert(t);
            }
        }
        if self.rooted {
            return;
        }
        let found = links(prompt);
        if found.is_empty() {
            return;
        }
        self.rooted = true;
        for name in found {
            let key = norm(&name);
            if self.sections.contains_key(&key) {
                continue;
            }
            if self.sections.len() >= MAX_SECTIONS {
                self.overflow += 1;
                continue;
            }
            self.sections.insert(key, name);
        }
    }

    fn visited(&self, key: &str) -> bool {
        self.titles.iter().any(|t| t.contains(key))
    }

    pub fn unvisited(&self) -> Vec<String> {
        self.sections
            .iter()
            .filter(|(k, _)| !self.visited(k))
            .map(|(_, v)| v.clone())
            .collect()
    }

    /// Пустая строка — обход доказан. Иначе причина, почему закрывать нельзя.
    pub fn gap(&self) -> Option<String> {
        if !self.rooted {
            return Some(
                "контракт глубины: на экране не было ни одной ссылки, обход не начат".into(),
            );
        }
        let left = self.unvisited();
        if self.overflow > 0 {
            return Some(format!(
                "контракт глубины: разделов больше {MAX_SECTIONS} (ещё {}), полный обход за прогон не доказуем",
                self.overflow
            ));
        }
        if left.is_empty() {
            None
        } else {
            Some(format!(
                "контракт глубины: не пройдены разделы ({} из {}): {}",
                left.len(),
                self.sections.len(),
                left.join(", ")
            ))
        }
    }

    pub fn prompt_block(&self) -> String {
        if !self.rooted {
            return "\nКОНТРАКТ ГЛУБИНЫ: задача требует обойти все разделы. Открой стартовую страницу со ссылками.\n".into();
        }
        let left = self.unvisited();
        if left.is_empty() {
            return format!(
                "\nКОНТРАКТ ГЛУБИНЫ: все {} разделов пройдены (по заголовкам окон).\n",
                self.sections.len()
            );
        }
        format!(
            "\nКОНТРАКТ ГЛУБИНЫ: задача не закроется, пока не открыт каждый раздел. Осталось {} из {}: {}\n",
            left.len(),
            self.sections.len(),
            left.join(", ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(goal: &str) -> TaskGraph {
        let mut g = TaskGraph::linear(goal, &[("шаг".into(), "видно «Готово»".into())]);
        g.validate().unwrap();
        g
    }

    fn screen(title: &str, links: &[&str]) -> String {
        let mut s = format!("ЭКРАН 1920x1080. Активное окно: \"{title}\"\nЭЛЕМЕНТЫ UI (name | type | центр x,y | размер):\n");
        for l in links {
            s.push_str(&format!("- {l} | link | 10,10 | 50x20\n"));
        }
        s.push_str("- Поиск | edit | 10,10 | 50x20\n");
        s
    }

    #[test]
    fn proof_beats_journal_beats_model() {
        let r = route(Mode::Open, Some(graph("a")), Some(graph("b")));
        assert!(matches!(r, Routed::Plan(_, Route::Proof)));
        let r = route(Mode::Strict, None, Some(graph("b")));
        assert!(matches!(r, Routed::Plan(_, Route::Journal)));
        assert!(matches!(route(Mode::Open, None, None), Routed::Model));
    }

    #[test]
    fn strict_mode_never_lets_model_choose_the_route() {
        assert!(matches!(
            route(Mode::Strict, None, None),
            Routed::Refused(_)
        ));
    }

    #[test]
    fn depth_is_requested_only_by_explicit_words() {
        assert!(wants_depth("Обойди весь сайт beautyarts и проверь цены"));
        assert!(wants_depth("пройди все разделы до самого дна"));
        assert!(!wants_depth("открой сайт и найди контакты"));
    }

    #[test]
    fn coverage_closes_only_after_every_section_was_on_screen() {
        let mut c = Coverage::default();
        assert!(c.gap().is_some(), "без стартовой страницы обход не доказан");
        c.observe(&screen(
            "Главная — BeautyArts",
            &["Каталог", "Контакты", "О нас"],
        ));
        let gap = c.gap().unwrap();
        assert!(gap.contains("Каталог") && gap.contains("Контакты"));

        // Модель может заявить что угодно — считаются только заголовки окон.
        c.observe(&screen("Каталог — BeautyArts", &["Филлеры"]));
        c.observe(&screen("Контакты — BeautyArts", &[]));
        assert_eq!(c.unvisited(), vec!["О нас".to_string()]);
        assert!(c.prompt_block().contains("О нас"));

        c.observe(&screen("О нас — BeautyArts", &[]));
        assert!(c.gap().is_none());
    }

    #[test]
    fn too_many_sections_keep_the_task_open() {
        let names: Vec<String> = (0..MAX_SECTIONS + 3)
            .map(|i| format!("Раздел {i}"))
            .collect();
        let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        let mut c = Coverage::default();
        c.observe(&screen("Главная", &refs));
        assert!(c.gap().unwrap().contains("больше"));
    }

    #[test]
    fn ocr_text_is_not_a_link() {
        let mut c = Coverage::default();
        c.observe("ЭКРАН 1x1. Активное окно: \"x\"\nТЕКСТ НА ЭКРАНЕ (OCR):\n- Каталог | link\n");
        assert!(c.gap().unwrap().contains("не начат"));
    }
}

