//! Платформенный слой: единый безопасный API поверх C++ (Windows) или
//! заглушек (Linux/macOS — только для сборки и тестов логики).
//!
//! Зачем прослойка: весь unsafe-код живёт в одном месте. Остальное ядро
//! работает с обычными Rust-типами и не знает про HWND и CoTaskMemFree.

#[cfg(windows)]
mod windows_impl;
#[cfg(windows)]
pub use windows_impl::*;

#[cfg(not(windows))]
mod stub;
#[cfg(not(windows))]
pub use stub::*;

use serde::{Deserialize, Serialize};

/// Скриншот в памяти. PNG получаем лениво: кодирование стоит ~20-60мс,
/// а нужно оно только когда кадр реально уходит в LLM.
pub struct Screenshot {
    // Размеры нужны только при поиске по картинке и масштабировании координат.
    #[allow(dead_code)]
    pub width: i32,
    #[allow(dead_code)]
    pub height: i32,
    pub png: Vec<u8>,
}

/// Найденное вхождение шаблона: x, y, ширина, высота, score 0..1.
pub type TemplateHit = (i32, i32, i32, i32, f32);

/// Элемент интерфейса из UI Automation или OCR.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiElement {
    #[serde(default)]
    pub name: String,
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub id: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    #[serde(default)]
    pub cx: i32,
    #[serde(default)]
    pub cy: i32,
}

impl UiElement {
    pub fn center(&self) -> (i32, i32) {
        if self.cx != 0 || self.cy != 0 {
            (self.cx, self.cy)
        } else {
            (self.x + self.w / 2, self.y + self.h / 2)
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

impl MouseButton {
    /// Код для C-слоя (в stub-сборке не используется).
    #[allow(dead_code)]
    pub fn code(self) -> i32 {
        match self {
            MouseButton::Left => 0,
            MouseButton::Right => 1,
            MouseButton::Middle => 2,
        }
    }
}
