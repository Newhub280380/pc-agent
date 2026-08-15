//! Восприятие: превращает экран (или телефон) в компактное описание.
//!
//! Порядок источников выбран по цене и надёжности:
//!   1. UI Automation — точные элементы, 0 токенов на распознавание;
//!   2. OCR (встроенный в Windows) — текст там, где UIA пуст (Canvas, картинки);
//!   3. Скриншот в LLM — только если первые два дали мало сигнала ИЛИ
//!      модель явно запросила «посмотреть глазами».
//!
//! Зачем такая экономия: скриншот 2560x1440 в vision-модель — это ~1.5-2.5к
//! токенов и 1-3 секунды на КАЖДЫЙ шаг. На задаче в 40 шагов разница между
//! «всегда картинка» и «картинка по нужде» — это доллары и минуты.

use crate::platform::{self, UiElement};
use anyhow::Result;

pub struct Observation {
    pub window_title: String,
    /// Пригодится действиям, которым нужно окно, а не координаты.
    #[allow(dead_code)]
    pub hwnd: u64,
    pub elements: Vec<UiElement>,
    pub ocr_lines: Vec<String>,
    pub screenshot_png: Option<Vec<u8>>,
    pub screen_w: i32,
    pub screen_h: i32,
}

impl Observation {
    /// Текстовое представление экрана для промпта.
    /// Обрезаем до 120 элементов: дальше начинается шум, а не информация.
    pub fn to_prompt(&self) -> String {
        let mut s = format!(
            "ЭКРАН {}x{}. Активное окно: \"{}\"\n",
            self.screen_w, self.screen_h, self.window_title
        );
        if !self.elements.is_empty() {
            s.push_str("ЭЛЕМЕНТЫ UI (name | type | центр x,y | размер):\n");
            for e in self.elements.iter().take(120) {
                let (cx, cy) = e.center();
                s.push_str(&format!(
                    "- {} | {} | {},{} | {}x{}\n",
                    truncate(&e.name, 70),
                    e.kind,
                    cx,
                    cy,
                    e.w,
                    e.h
                ));
            }
        }
        if !self.ocr_lines.is_empty() {
            s.push_str("ТЕКСТ НА ЭКРАНЕ (OCR):\n");
            for l in self.ocr_lines.iter().take(60) {
                s.push_str(&format!("- {}\n", truncate(l, 100)));
            }
        }
        if self.elements.is_empty() && self.ocr_lines.is_empty() {
            s.push_str("(UI-дерево и OCR пусты — смотри на скриншот)\n");
        }
        s
    }

    /// Контекст для выбора полки памяти: заголовок окна обычно содержит
    /// домен или имя приложения.
    pub fn memory_context(&self) -> String {
        self.window_title.clone()
    }
}

/// Полный цикл восприятия ПК.
pub fn perceive(need_screenshot: bool, ocr_lang: &str) -> Result<Observation> {
    let (hwnd, title) = platform::foreground_window().unwrap_or((0, String::new()));
    let (sw, sh) = platform::screen_size().unwrap_or((0, 0));

    let elements = platform::ui_tree(hwnd).unwrap_or_default();

    // OCR запускаем, только если UIA дал мало: он стоит 50-200мс.
    let ocr_lines = if elements.len() < 8 {
        platform::ocr(&[], ocr_lang)
            .unwrap_or_default()
            .into_iter()
            .map(|e| e.name)
            .filter(|s| !s.trim().is_empty())
            .collect()
    } else {
        vec![]
    };

    let screenshot_png = if need_screenshot || (elements.len() < 8 && ocr_lines.is_empty()) {
        platform::capture_screen().ok().map(|s| s.png)
    } else {
        None
    };

    Ok(Observation {
        window_title: title,
        hwnd,
        elements,
        ocr_lines,
        screenshot_png,
        screen_w: sw,
        screen_h: sh,
    })
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}
