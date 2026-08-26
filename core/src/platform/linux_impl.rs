//! Linux-реализация «рук и глаз» — через штатные утилиты рабочего стола.
//!
//! Почему внешние программы, а не крейты: на Linux нет одного API рабочего
//! стола. X11 и Wayland — разные миры, а готовые крейты (enigo, xcap) тянут
//! xlib/pipewire в сборку и всё равно не работают под Wayland. Утилиты
//! xdotool/scrot/grim ставятся одной командой apt и делают ровно то же, зато
//! бинарь остаётся переносимым, а в ошибке видно, чего именно не хватает.
//!
//! Ограничения честно: UI Automation на Linux нет, поэтому дерево элементов
//! пустое и агент смотрит на экран (скриншот + OCR через tesseract), а поиск
//! по шаблону недоступен — он живёт в C++-слое под Windows.

// Часть функций существует для паритета с Windows-реализацией: ядро зовёт их
// не на каждой платформе, но API слоя обязан быть одинаковым.
#![allow(dead_code)]

use super::{MouseButton, Screenshot, TemplateHit, UiElement};
use anyhow::{anyhow, bail, Context, Result};
use std::path::PathBuf;
use std::process::Command;

/// Тип сессии: под Wayland X11-утилиты либо ничего не видят, либо молча
/// ничего не делают, поэтому набор инструментов выбирается заранее.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Session {
    X11,
    Wayland,
}

fn session() -> Session {
    if std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland")
        || (std::env::var_os("WAYLAND_DISPLAY").is_some() && std::env::var_os("DISPLAY").is_none())
    {
        Session::Wayland
    } else {
        Session::X11
    }
}

/// Путь до утилиты в PATH или None.
fn tool(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

/// Утилита или понятная ошибка с командой установки: человек за Ubuntu должен
/// уметь починить всё копированием одной строки.
fn need(name: &str, apt: &str) -> Result<PathBuf> {
    tool(name).ok_or_else(|| anyhow!("нет утилиты {name}. Установи: sudo apt install -y {apt}"))
}

fn run(prog: &PathBuf, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new(prog)
        .args(args)
        .output()
        .with_context(|| format!("не удалось запустить {}", prog.display()))?;
    if !out.status.success() {
        bail!(
            "{} {:?} завершилась с ошибкой: {}",
            prog.display(),
            args,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

fn xdotool() -> Result<PathBuf> {
    if session() == Session::Wayland {
        bail!(
            "сессия Wayland: управлять мышью и клавиатурой можно только через ydotool \
             (sudo apt install -y ydotool && sudo systemctl enable --now ydotool) \
             либо войти в систему, выбрав сессию «Xorg» на экране входа"
        );
    }
    need("xdotool", "xdotool")
}

fn xdo(args: &[&str]) -> Result<Vec<u8>> {
    let exe = xdotool()?;
    run(&exe, args)
}

pub fn init() -> Result<()> {
    let s = session();
    log::info!("платформа: Linux, сессия {s:?}");
    // Скриншот — единственная критичная вещь: без него агент слепой.
    let shooter = screenshot_tool();
    match &shooter {
        Ok((p, _)) => log::info!("снимки экрана: {}", p.display()),
        Err(e) => log::error!("{e}"),
    }
    if let Err(e) = xdotool() {
        log::warn!("управление вводом недоступно: {e}");
    }
    if tool("tesseract").is_none() {
        log::warn!("нет tesseract — текст на экране читает только LLM по скриншоту");
    }
    shooter.map(|_| ())
}

pub fn shutdown() {}

/// Чем снимать экран: порядок — от самого предсказуемого к «что нашлось».
/// Второй элемент кортежа — аргументы, где `{}` заменяется на файл.
fn screenshot_tool() -> Result<(PathBuf, Vec<&'static str>)> {
    let candidates: &[(&str, &[&str])] = if session() == Session::Wayland {
        &[("grim", &["{}"]), ("spectacle", &["-b", "-n", "-o", "{}"])]
    } else {
        &[
            ("scrot", &["-o", "-z", "{}"]),
            ("maim", &["{}"]),
            ("import", &["-window", "root", "{}"]),
            ("gnome-screenshot", &["-f", "{}"]),
        ]
    };
    for (name, args) in candidates {
        if let Some(p) = tool(name) {
            return Ok((p, args.to_vec()));
        }
    }
    if session() == Session::Wayland {
        bail!("нечем снять экран. Установи: sudo apt install -y grim");
    }
    bail!("нечем снять экран. Установи: sudo apt install -y scrot")
}

fn temp_png(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("pcagent-{tag}-{}.png", std::process::id()))
}

/// Размеры из заголовка PNG (IHDR идёт сразу за 8-байтной подписью).
fn png_size(png: &[u8]) -> (i32, i32) {
    if png.len() < 24 {
        return (0, 0);
    }
    let num = |b: &[u8]| i32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    (num(&png[16..20]), num(&png[20..24]))
}

pub fn capture_screen() -> Result<Screenshot> {
    let (exe, args) = screenshot_tool()?;
    let file = temp_png("shot");
    let path = file.to_string_lossy().to_string();
    let args: Vec<&str> = args
        .iter()
        .map(|a| if *a == "{}" { path.as_str() } else { a })
        .collect();
    let _ = std::fs::remove_file(&file);
    run(&exe, &args)?;
    let png =
        std::fs::read(&file).with_context(|| format!("{} не создал снимок", exe.display()))?;
    let _ = std::fs::remove_file(&file);
    let (width, height) = png_size(&png);
    Ok(Screenshot { width, height, png })
}

/// Коды языков Windows («ru-RU») в имена моделей tesseract («rus»).
fn tess_lang(lang: &str) -> String {
    let base = lang.split(['-', '_']).next().unwrap_or(lang).to_lowercase();
    match base.as_str() {
        "ru" => "rus+eng".into(),
        "en" => "eng".into(),
        "kk" => "kaz+rus+eng".into(),
        "" => "eng".into(),
        other => format!("{other}+eng"),
    }
}

/// OCR через tesseract: tsv даёт слова с координатами, из них собираются
/// строки — их и видит планировщик как «текст на экране».
pub fn ocr(png: &[u8], lang: &str) -> Result<Vec<UiElement>> {
    let exe = need("tesseract", "tesseract-ocr tesseract-ocr-rus")?;
    let owned = if png.is_empty() {
        capture_screen()?.png
    } else {
        png.to_vec()
    };
    let file = temp_png("ocr");
    std::fs::write(&file, &owned)?;
    let out = run(
        &exe,
        &[
            &file.to_string_lossy(),
            "stdout",
            "-l",
            &tess_lang(lang),
            "tsv",
        ],
    );
    let _ = std::fs::remove_file(&file);
    Ok(parse_tsv(&String::from_utf8_lossy(&out?)))
}

/// Слова tesseract → строки. Колонки tsv: level, page, block, par, line,
/// word, left, top, width, height, conf, text.
fn parse_tsv(tsv: &str) -> Vec<UiElement> {
    let mut lines: Vec<UiElement> = Vec::new();
    let mut key: Option<(u32, u32, u32)> = None;
    for row in tsv.lines().skip(1) {
        let c: Vec<&str> = row.split('\t').collect();
        if c.len() < 12 || c[0] != "5" {
            continue;
        }
        let num = |i: usize| c[i].parse::<i32>().unwrap_or(0);
        let conf: f32 = c[10].parse().unwrap_or(0.0);
        let text = c[11].trim();
        if conf < 40.0 || text.is_empty() {
            continue;
        }
        let (x, y, w, h) = (num(6), num(7), num(8), num(9));
        let id = (
            c[2].parse().unwrap_or(0),
            c[3].parse().unwrap_or(0),
            c[4].parse().unwrap_or(0),
        );
        match lines.last_mut() {
            Some(last) if key == Some(id) => {
                last.name.push(' ');
                last.name.push_str(text);
                last.w = (x + w - last.x).max(last.w);
                last.h = last.h.max(h);
            }
            _ => {
                key = Some(id);
                lines.push(UiElement {
                    name: text.to_string(),
                    kind: "text".into(),
                    id: String::new(),
                    x,
                    y,
                    w,
                    h,
                    cx: 0,
                    cy: 0,
                });
            }
        }
    }
    lines
}

pub fn ui_tree(_hwnd: u64) -> Result<Vec<UiElement>> {
    // AT-SPI на Linux выключен по умолчанию и в браузерах отдаёт дерево
    // неполностью, поэтому не притворяемся: агент работает по скриншоту.
    bail!("дерева UI-элементов на Linux нет — восприятие идёт по скриншоту и OCR")
}

pub fn find_template(_png: &[u8], _min_score: f32) -> Result<Vec<TemplateHit>> {
    bail!("поиск по шаблону доступен только под Windows (нужен C++-слой)")
}

pub fn mouse_move(x: i32, y: i32) -> Result<()> {
    xdo(&["mousemove", &x.to_string(), &y.to_string()]).map(|_| ())
}

fn button_code(b: MouseButton) -> &'static str {
    match b {
        MouseButton::Left => "1",
        MouseButton::Middle => "2",
        MouseButton::Right => "3",
    }
}

pub fn click(b: MouseButton, double: bool) -> Result<()> {
    let code = button_code(b);
    if double {
        xdo(&["click", "--repeat", "2", "--delay", "80", code]).map(|_| ())
    } else {
        xdo(&["click", code]).map(|_| ())
    }
}

pub fn click_at(x: i32, y: i32, b: MouseButton, double: bool) -> Result<()> {
    mouse_move(x, y)?;
    click(b, double)
}

pub fn drag(x1: i32, y1: i32, x2: i32, y2: i32) -> Result<()> {
    // Одной командой: между mousedown и mouseup нельзя терять фокус.
    xdo(&[
        "mousemove",
        &x1.to_string(),
        &y1.to_string(),
        "mousedown",
        "1",
        "mousemove",
        &x2.to_string(),
        &y2.to_string(),
        "mouseup",
        "1",
    ])
    .map(|_| ())
}

pub fn scroll(clicks: i32, horizontal: bool) -> Result<()> {
    if clicks == 0 {
        return Ok(());
    }
    let button = match (horizontal, clicks > 0) {
        (false, true) => "4",  // вверх
        (false, false) => "5", // вниз
        (true, true) => "7",   // вправо
        (true, false) => "6",  // влево
    };
    xdo(&[
        "click",
        "--repeat",
        &clicks.abs().to_string(),
        "--delay",
        "30",
        button,
    ])
    .map(|_| ())
}

pub fn type_text(text: &str) -> Result<()> {
    xdo(&["type", "--clearmodifiers", "--delay", "12", "--", text]).map(|_| ())
}

/// Имена клавиш из мира Windows в имена X11: агент и LLM говорят «win+r»
/// и «esc», xdotool ждёт «super+r» и «Escape».
fn x_combo(combo: &str) -> String {
    combo
        .split('+')
        .map(|part| {
            let p = part.trim();
            match p.to_lowercase().as_str() {
                "win" | "cmd" | "meta" => "super".to_string(),
                "esc" => "Escape".to_string(),
                "enter" | "return" => "Return".to_string(),
                "del" => "Delete".to_string(),
                "ins" => "Insert".to_string(),
                "pgup" => "Prior".to_string(),
                "pgdn" | "pgdown" => "Next".to_string(),
                "space" => "space".to_string(),
                "tab" => "Tab".to_string(),
                "backspace" | "bksp" => "BackSpace".to_string(),
                "up" | "down" | "left" | "right" | "home" | "end" => {
                    let mut c = p.chars();
                    match c.next() {
                        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                        None => String::new(),
                    }
                }
                _ => p.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

pub fn key_combo(combo: &str) -> Result<()> {
    xdo(&["key", "--clearmodifiers", &x_combo(combo)]).map(|_| ())
}

pub fn foreground_window() -> Result<(u64, String)> {
    // getactivewindow опирается на _NET_ACTIVE_WINDOW: его нет в лёгких
    // оконных менеджерах и в Xvfb, поэтому есть запасной путь через фокус ввода.
    let raw = match xdo(&["getactivewindow"]) {
        Ok(out) => out,
        Err(e) => xdo(&["getwindowfocus"]).map_err(|_| e)?,
    };
    let id = String::from_utf8_lossy(&raw).trim().to_string();
    let title = String::from_utf8_lossy(&xdo(&["getwindowname", &id])?)
        .trim()
        .to_string();
    Ok((id.parse().unwrap_or(0), title))
}

pub fn focus_window(hwnd: u64) -> Result<()> {
    xdo(&["windowactivate", "--sync", &hwnd.to_string()]).map(|_| ())
}

pub fn find_window(title_substr: &str) -> Result<(u64, String)> {
    let out = xdo(&["search", "--name", title_substr])?;
    let id = String::from_utf8_lossy(&out)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    if id.is_empty() {
        bail!("окно с «{title_substr}» в заголовке не найдено");
    }
    let title = String::from_utf8_lossy(&xdo(&["getwindowname", &id])?)
        .trim()
        .to_string();
    Ok((id.parse().unwrap_or(0), title))
}

pub fn clipboard_get() -> Result<String> {
    let (exe, args) = if session() == Session::Wayland {
        (need("wl-paste", "wl-clipboard")?, vec!["--no-newline"])
    } else {
        (
            need("xclip", "xclip")?,
            vec!["-selection", "clipboard", "-o"],
        )
    };
    Ok(String::from_utf8_lossy(&run(&exe, &args)?).to_string())
}

pub fn clipboard_set(s: &str) -> Result<()> {
    use std::io::Write;
    use std::process::Stdio;

    let (exe, args) = if session() == Session::Wayland {
        (need("wl-copy", "wl-clipboard")?, vec![])
    } else {
        (need("xclip", "xclip")?, vec!["-selection", "clipboard"])
    };
    let mut child = Command::new(&exe)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("не удалось запустить {}", exe.display()))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| anyhow!("{} не принял ввод", exe.display()))?
        .write_all(s.as_bytes())?;
    // wl-copy держит буфер, пока жив процесс, и на wait() зависал бы навсегда.
    if session() == Session::Wayland {
        return Ok(());
    }
    let st = child.wait()?;
    if !st.success() {
        bail!("{} вернул {st}", exe.display());
    }
    Ok(())
}

pub fn screen_size() -> Result<(i32, i32)> {
    if session() == Session::X11 {
        if let Ok(out) = xdo(&["getdisplaygeometry"]) {
            let text = String::from_utf8_lossy(&out);
            let mut it = text.split_whitespace();
            if let (Some(w), Some(h)) = (it.next(), it.next()) {
                if let (Ok(w), Ok(h)) = (w.parse(), h.parse()) {
                    return Ok((w, h));
                }
            }
        }
    }
    // Под Wayland геометрию спрашивать нечем — берём из снимка экрана.
    let s = capture_screen()?;
    Ok((s.width, s.height))
}

pub fn user_idle_seconds() -> i32 {
    // Без xprintidle считаем, что человек за компьютером: лучше лишний раз
    // спросить подтверждение, чем начать двигать мышью под его руками.
    match tool("xprintidle").map(|exe| run(&exe, &[])) {
        Some(Ok(out)) => String::from_utf8_lossy(&out)
            .trim()
            .parse::<i64>()
            .map(|ms| (ms / 1000) as i32)
            .unwrap_or(0),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_size_reads_ihdr() {
        let mut png = vec![0u8; 24];
        png[16..20].copy_from_slice(&1920i32.to_be_bytes());
        png[20..24].copy_from_slice(&1080i32.to_be_bytes());
        assert_eq!(png_size(&png), (1920, 1080));
        assert_eq!(png_size(&[0, 1, 2]), (0, 0));
    }

    #[test]
    fn windows_key_names_become_x11() {
        assert_eq!(x_combo("win+r"), "super+r");
        assert_eq!(x_combo("ctrl+shift+esc"), "ctrl+shift+Escape");
        assert_eq!(x_combo("alt+Tab"), "alt+Tab");
        assert_eq!(x_combo("ctrl+enter"), "ctrl+Return");
        assert_eq!(x_combo("pgdn"), "Next");
    }

    #[test]
    fn ocr_langs_map_to_tesseract_models() {
        assert_eq!(tess_lang("ru-RU"), "rus+eng");
        assert_eq!(tess_lang("en-US"), "eng");
        assert_eq!(tess_lang(""), "eng");
    }

    #[test]
    fn tsv_words_are_joined_into_lines() {
        let tsv = "level\tpage\tblock\tpar\tline\tword\tleft\ttop\twidth\theight\tconf\ttext\n\
             5\t1\t1\t1\t1\t1\t10\t20\t30\t12\t95\tПривет\n\
             5\t1\t1\t1\t1\t2\t45\t20\t20\t12\t90\tмир\n\
             5\t1\t2\t1\t1\t1\t10\t60\t40\t12\t12\tмусор\n\
             5\t1\t2\t1\t2\t1\t10\t80\t50\t14\t88\tOK\n";
        let els = parse_tsv(tsv);
        assert_eq!(els.len(), 2);
        assert_eq!(els[0].name, "Привет мир");
        assert_eq!(els[0].w, 55);
        assert_eq!(els[1].name, "OK");
    }
}
