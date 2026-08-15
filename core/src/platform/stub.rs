//! Заглушки для не-Windows сборок.
//!
//! Зачем они вообще: чтобы логику ядра (планировщик, память, роутер) можно
//! было собирать и тестировать в CI на Linux, где нет WinAPI. В рантайме
//! под Windows этот файл не компилируется.

// Часть функций тут не вызывается ядром на Linux — они существуют для
// паритета API с Windows-реализацией, чтобы код ядра компилировался обеими.
#![allow(dead_code)]

use super::{MouseButton, Screenshot, UiElement};
use anyhow::{bail, Result};

const MSG: &str = "нативный слой доступен только под Windows (сейчас stub-сборка)";

pub fn init() -> Result<()> {
    log::warn!("{MSG}");
    Ok(())
}
pub fn shutdown() {}

pub fn capture_screen() -> Result<Screenshot> {
    bail!(MSG)
}
pub fn ocr(_png: &[u8], _lang: &str) -> Result<Vec<UiElement>> {
    bail!(MSG)
}
pub fn ui_tree(_hwnd: u64) -> Result<Vec<UiElement>> {
    bail!(MSG)
}
pub fn find_template(_png: &[u8], _min_score: f32) -> Result<Vec<(i32, i32, i32, i32, f32)>> {
    bail!(MSG)
}
pub fn mouse_move(_x: i32, _y: i32) -> Result<()> {
    bail!(MSG)
}
pub fn click(_b: MouseButton, _double: bool) -> Result<()> {
    bail!(MSG)
}
pub fn click_at(_x: i32, _y: i32, _b: MouseButton, _double: bool) -> Result<()> {
    bail!(MSG)
}
pub fn drag(_x1: i32, _y1: i32, _x2: i32, _y2: i32) -> Result<()> {
    bail!(MSG)
}
pub fn scroll(_clicks: i32, _horizontal: bool) -> Result<()> {
    bail!(MSG)
}
pub fn type_text(_text: &str) -> Result<()> {
    bail!(MSG)
}
pub fn key_combo(_combo: &str) -> Result<()> {
    bail!(MSG)
}
pub fn foreground_window() -> Result<(u64, String)> {
    bail!(MSG)
}
pub fn focus_window(_hwnd: u64) -> Result<()> {
    bail!(MSG)
}
pub fn find_window(_title_substr: &str) -> Result<(u64, String)> {
    bail!(MSG)
}
pub fn clipboard_get() -> Result<String> {
    bail!(MSG)
}
pub fn clipboard_set(_s: &str) -> Result<()> {
    bail!(MSG)
}
pub fn screen_size() -> Result<(i32, i32)> {
    Ok((1920, 1080))
}
pub fn user_idle_seconds() -> i32 {
    9999
}
