//! Безопасные обёртки над C ABI из native/include/agent_native.h.
//!
//! Правило этого файла: наружу не утекает ни один сырой указатель.
//! Всё, что вернула C++-сторона, немедленно копируется в Rust-типы и
//! освобождается через an_free/an_free_image.

use super::{MouseButton, Screenshot, UiElement};
use anyhow::{anyhow, Result};
use std::ffi::{c_char, c_void, CStr, CString};

#[repr(C)]
struct AnImage {
    data: *mut u8,
    width: i32,
    height: i32,
    stride: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct AnRect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    score: f32,
}

extern "C" {
    fn an_init() -> i32;
    fn an_shutdown();
    fn an_free(p: *mut c_void);
    fn an_last_error() -> *const c_char;
    fn an_capture_screen(out: *mut AnImage) -> i32;
    fn an_free_image(img: *mut AnImage);
    fn an_encode_png(img: *const AnImage, buf: *mut *mut u8, len: *mut i32) -> i32;
    fn an_ocr(img: *const AnImage, lang: *const c_char, out: *mut *mut c_char) -> i32;
    fn an_find_template(
        hay: *const AnImage,
        png: *const u8,
        png_len: i32,
        min_score: f32,
        out: *mut AnRect,
        max_out: i32,
        found: *mut i32,
    ) -> i32;
    fn an_ui_tree(hwnd: u64, max_depth: i32, out: *mut *mut c_char) -> i32;
    fn an_mouse_move_human(x: i32, y: i32, duration_ms: i32) -> i32;
    fn an_mouse_click(button: i32, double_click: i32) -> i32;
    fn an_mouse_drag(x1: i32, y1: i32, x2: i32, y2: i32, duration_ms: i32) -> i32;
    fn an_scroll(clicks: i32, horizontal: i32) -> i32;
    fn an_type_text_human(text: *const c_char, wpm: i32) -> i32;
    fn an_key_combo(combo: *const c_char) -> i32;
    fn an_foreground_window(hwnd: *mut u64, title: *mut *mut c_char) -> i32;
    fn an_focus_window(hwnd: u64) -> i32;
    fn an_find_window(title_substr: *const c_char, hwnd: *mut u64, title: *mut *mut c_char) -> i32;
    fn an_clipboard_get(out: *mut *mut c_char) -> i32;
    fn an_clipboard_set(s: *const c_char) -> i32;
    fn an_screen_size(w: *mut i32, h: *mut i32) -> i32;
    fn an_user_idle_seconds() -> i32;
}

fn last_error() -> String {
    unsafe {
        let p = an_last_error();
        if p.is_null() {
            "неизвестная ошибка native-слоя".into()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}

fn check(rc: i32, what: &str) -> Result<()> {
    if rc == 0 {
        Ok(())
    } else {
        Err(anyhow!("{what} failed (rc={rc}): {}", last_error()))
    }
}

/// Забирает C-строку в Rust и освобождает исходный буфер.
unsafe fn take_string(p: *mut c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    let s = CStr::from_ptr(p).to_string_lossy().into_owned();
    an_free(p as *mut c_void);
    s
}

pub fn init() -> Result<()> {
    check(unsafe { an_init() }, "an_init")
}

pub fn shutdown() {
    unsafe { an_shutdown() }
}

/// RAII для кадра: гарантирует освобождение даже при раннем возврате ошибки.
struct Frame(AnImage);

impl Drop for Frame {
    fn drop(&mut self) {
        unsafe { an_free_image(&mut self.0) }
    }
}

fn grab() -> Result<Frame> {
    let mut img = AnImage {
        data: std::ptr::null_mut(),
        width: 0,
        height: 0,
        stride: 0,
    };
    check(unsafe { an_capture_screen(&mut img) }, "an_capture_screen")?;
    Ok(Frame(img))
}

pub fn capture_screen() -> Result<Screenshot> {
    let f = grab()?;
    let mut buf: *mut u8 = std::ptr::null_mut();
    let mut len: i32 = 0;
    check(
        unsafe { an_encode_png(&f.0, &mut buf, &mut len) },
        "an_encode_png",
    )?;
    let png = unsafe {
        let v = std::slice::from_raw_parts(buf, len as usize).to_vec();
        an_free(buf as *mut c_void);
        v
    };
    Ok(Screenshot {
        width: f.0.width,
        height: f.0.height,
        png,
    })
}

/// OCR текущего экрана. `_png` игнорируется: снимаем свежий кадр, чтобы не
/// гонять мегабайты обратно в C++ (кодирование/декодирование PNG дороже).
pub fn ocr(_png: &[u8], lang: &str) -> Result<Vec<UiElement>> {
    let f = grab()?;
    let clang = CString::new(lang)?;
    let mut out: *mut c_char = std::ptr::null_mut();
    check(unsafe { an_ocr(&f.0, clang.as_ptr(), &mut out) }, "an_ocr")?;
    let json = unsafe { take_string(out) };
    Ok(serde_json::from_str(&json).unwrap_or_default())
}

pub fn ui_tree(hwnd: u64) -> Result<Vec<UiElement>> {
    let mut out: *mut c_char = std::ptr::null_mut();
    check(unsafe { an_ui_tree(hwnd, 12, &mut out) }, "an_ui_tree")?;
    let json = unsafe { take_string(out) };
    Ok(serde_json::from_str(&json).unwrap_or_default())
}

pub fn find_template(png: &[u8], min_score: f32) -> Result<Vec<super::TemplateHit>> {
    let f = grab()?;
    let mut rects = [AnRect::default(); 16];
    let mut found = 0i32;
    check(
        unsafe {
            an_find_template(
                &f.0,
                png.as_ptr(),
                png.len() as i32,
                min_score,
                rects.as_mut_ptr(),
                rects.len() as i32,
                &mut found,
            )
        },
        "an_find_template",
    )?;
    Ok(rects[..found as usize]
        .iter()
        .map(|r| (r.x, r.y, r.w, r.h, r.score))
        .collect())
}

pub fn mouse_move(x: i32, y: i32) -> Result<()> {
    check(unsafe { an_mouse_move_human(x, y, 0) }, "mouse_move")
}

pub fn click(b: MouseButton, double: bool) -> Result<()> {
    check(unsafe { an_mouse_click(b.code(), double as i32) }, "click")
}

pub fn click_at(x: i32, y: i32, b: MouseButton, double: bool) -> Result<()> {
    mouse_move(x, y)?;
    click(b, double)
}

pub fn drag(x1: i32, y1: i32, x2: i32, y2: i32) -> Result<()> {
    check(unsafe { an_mouse_drag(x1, y1, x2, y2, 0) }, "drag")
}

pub fn scroll(clicks: i32, horizontal: bool) -> Result<()> {
    check(unsafe { an_scroll(clicks, horizontal as i32) }, "scroll")
}

pub fn type_text(text: &str) -> Result<()> {
    let c = CString::new(text)?;
    check(unsafe { an_type_text_human(c.as_ptr(), 240) }, "type_text")
}

pub fn key_combo(combo: &str) -> Result<()> {
    let c = CString::new(combo)?;
    check(unsafe { an_key_combo(c.as_ptr()) }, "key_combo")
}

pub fn foreground_window() -> Result<(u64, String)> {
    let mut hwnd = 0u64;
    let mut title: *mut c_char = std::ptr::null_mut();
    check(
        unsafe { an_foreground_window(&mut hwnd, &mut title) },
        "foreground_window",
    )?;
    Ok((hwnd, unsafe { take_string(title) }))
}

pub fn focus_window(hwnd: u64) -> Result<()> {
    check(unsafe { an_focus_window(hwnd) }, "focus_window")
}

pub fn find_window(title_substr: &str) -> Result<(u64, String)> {
    let c = CString::new(title_substr)?;
    let mut hwnd = 0u64;
    let mut title: *mut c_char = std::ptr::null_mut();
    check(
        unsafe { an_find_window(c.as_ptr(), &mut hwnd, &mut title) },
        "find_window",
    )?;
    Ok((hwnd, unsafe { take_string(title) }))
}

pub fn clipboard_get() -> Result<String> {
    let mut out: *mut c_char = std::ptr::null_mut();
    check(unsafe { an_clipboard_get(&mut out) }, "clipboard_get")?;
    Ok(unsafe { take_string(out) })
}

pub fn clipboard_set(s: &str) -> Result<()> {
    let c = CString::new(s)?;
    check(unsafe { an_clipboard_set(c.as_ptr()) }, "clipboard_set")
}

pub fn screen_size() -> Result<(i32, i32)> {
    let (mut w, mut h) = (0, 0);
    check(unsafe { an_screen_size(&mut w, &mut h) }, "screen_size")?;
    Ok((w, h))
}

pub fn user_idle_seconds() -> i32 {
    unsafe { an_user_idle_seconds() }
}
