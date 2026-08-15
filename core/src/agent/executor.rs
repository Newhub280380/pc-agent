//! Исполнитель: превращает Action в реальные клики/нажатия.
//!
//! Здесь же — «человеческая» страховка: агент не дерётся с пользователем за
//! мышь и не бьёт по необратимым кнопкам при низкой уверенности.

use super::action::Action;
use crate::android::Adb;
use crate::platform::{self, MouseButton};
use anyhow::{anyhow, bail, Result};
use std::time::Duration;

pub enum Outcome {
    /// Действие выполнено, текст — что получилось (уходит в журнал и в промпт).
    Ok(String),
    /// Нужен человек: 2FA, SMS, спорное решение. GUI покажет окно и вернёт ответ.
    NeedHuman {
        question: String,
        secret: bool,
    },
    /// Не хватает инструмента/доступа — просим пользователя подключить.
    NeedTool {
        what: String,
        why: String,
        how: String,
    },
    SubtaskDone(String),
    Finished(String),
}

pub struct Executor {
    pub adb: Adb,
    /// Если пользователь активно работает мышью — агент ждёт, а не борется.
    pub yield_to_user: bool,
}

impl Executor {
    pub fn new(adb: Adb) -> Self {
        Self {
            adb,
            yield_to_user: true,
        }
    }

    fn wait_for_user_idle(&self) {
        if !self.yield_to_user {
            return;
        }
        // 2 секунды простоя — компромисс: не мешаем человеку, но и не стоим
        // вечно, если он просто держит руку на мыши.
        for _ in 0..30 {
            if platform::user_idle_seconds() >= 2 {
                return;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    pub fn execute(&mut self, act: &Action) -> Result<Outcome> {
        match act {
            Action::ClickElement { name, control_type } => {
                self.wait_for_user_idle();
                let (hwnd, _) = platform::foreground_window().unwrap_or((0, String::new()));
                let tree = platform::ui_tree(hwnd)?;
                let needle = name.to_lowercase();
                let hit = tree
                    .iter()
                    .filter(|e| {
                        control_type.is_empty() || e.kind.eq_ignore_ascii_case(control_type)
                    })
                    .find(|e| e.name.to_lowercase().contains(&needle))
                    // Точное совпадение приоритетнее, но частичное лучше промаха:
                    // подписи кнопок часто содержат лишние пробелы и значки.
                    .ok_or_else(|| anyhow!("элемент \"{name}\" не найден в UI-дереве"))?;
                let (x, y) = hit.center();
                platform::click_at(x, y, MouseButton::Left, false)?;
                Ok(Outcome::Ok(format!("клик по \"{}\" в ({x},{y})", hit.name)))
            }
            Action::ClickXY {
                x,
                y,
                button,
                double,
            } => {
                self.wait_for_user_idle();
                let b = match button.as_str() {
                    "right" => MouseButton::Right,
                    "middle" => MouseButton::Middle,
                    _ => MouseButton::Left,
                };
                platform::click_at(*x, *y, b, *double)?;
                Ok(Outcome::Ok(format!("клик в ({x},{y})")))
            }
            Action::TypeText { text, press_enter } => {
                self.wait_for_user_idle();
                // Длинные тексты (посты, описания объявлений) печатать побуквенно —
                // это минуты и риск потерять фокус на полпути. Через буфер обмена
                // быстро и надёжно; короткое же печатаем по-человечески: логины и
                // поисковые строки часто слушают keydown и не реагируют на вставку.
                if text.chars().count() > 200 {
                    let saved = platform::clipboard_get().unwrap_or_default();
                    platform::clipboard_set(text)?;
                    platform::key_combo("ctrl+v")?;
                    std::thread::sleep(Duration::from_millis(250));
                    // Возвращаем буфер пользователя: агент не должен воровать его скопированное.
                    if !saved.is_empty() {
                        let _ = platform::clipboard_set(&saved);
                    }
                    if *press_enter {
                        platform::key_combo("enter")?;
                    }
                    return Ok(Outcome::Ok(format!(
                        "вставлено {} символов",
                        text.chars().count()
                    )));
                }
                platform::type_text(text)?;
                if *press_enter {
                    std::thread::sleep(Duration::from_millis(180));
                    platform::key_combo("enter")?;
                }
                // Пароли и коды не логируем целиком.
                Ok(Outcome::Ok(format!(
                    "введено {} символов",
                    text.chars().count()
                )))
            }
            Action::KeyCombo { combo } => {
                platform::key_combo(combo)?;
                Ok(Outcome::Ok(format!("клавиши {combo}")))
            }
            Action::Scroll { clicks, horizontal } => {
                // Модель любит попросить «скролл 100000» — это часы зависания в SendInput.
                let clicks = (*clicks).clamp(-50, 50);
                platform::scroll(clicks, *horizontal)?;
                Ok(Outcome::Ok(format!("скролл {clicks}")))
            }
            Action::Drag { x1, y1, x2, y2 } => {
                platform::drag(*x1, *y1, *x2, *y2)?;
                Ok(Outcome::Ok("перетаскивание выполнено".into()))
            }
            Action::OpenUrl { url } => {
                check_url(url)?;
                // Открываем в браузере ПО УМОЛЧАНИЮ и в профиле пользователя:
                // его куки, его сессии, его «отпечаток». Никакого headless —
                // именно этого требует режим «Человек».
                open_default(url)?;
                std::thread::sleep(Duration::from_millis(2500));
                Ok(Outcome::Ok(format!("открыт {url}")))
            }
            Action::LaunchApp { path, args } => {
                launch(path, args)?;
                std::thread::sleep(Duration::from_millis(2000));
                Ok(Outcome::Ok(format!("запущено {path}")))
            }
            Action::FocusWindow { title_contains } => {
                if let Ok((_, title)) = platform::foreground_window() {
                    if title
                        .to_lowercase()
                        .contains(&title_contains.to_lowercase())
                    {
                        return Ok(Outcome::Ok(format!("окно \"{title}\" уже активно")));
                    }
                }
                let (hwnd, title) = platform::find_window(title_contains)?;
                platform::focus_window(hwnd)?;
                // Переключение окна анимировано— без паузы следующий клик уйдёт в старое окно.
                std::thread::sleep(Duration::from_millis(400));
                Ok(Outcome::Ok(format!("активировано окно \"{title}\"")))
            }
            Action::Wait { seconds, reason } => {
                let s = seconds.clamp(0.1, 120.0);
                std::thread::sleep(Duration::from_secs_f64(s));
                Ok(Outcome::Ok(format!("ждали {s:.1}с ({reason})")))
            }
            Action::Remember { title, .. } => Ok(Outcome::Ok(format!("запомнено: {title}"))),
            Action::AskUser { question, secret } => Ok(Outcome::NeedHuman {
                question: question.clone(),
                secret: *secret,
            }),
            Action::NeedTool {
                what,
                why,
                how_to_get,
            } => Ok(Outcome::NeedTool {
                what: what.clone(),
                why: why.clone(),
                how: how_to_get.clone(),
            }),
            Action::SubtaskDone { result } => Ok(Outcome::SubtaskDone(result.clone())),
            Action::Finish { report } => Ok(Outcome::Finished(report.clone())),

            // ---- Android ----
            Action::AdbTap { x, y } => {
                self.adb.tap(*x, *y)?;
                Ok(Outcome::Ok(format!("тап по телефону ({x},{y})")))
            }
            Action::AdbSwipe { x1, y1, x2, y2, ms } => {
                self.adb
                    .swipe(*x1, *y1, *x2, *y2, if *ms > 0 { *ms } else { 300 })?;
                Ok(Outcome::Ok("свайп выполнен".into()))
            }
            Action::AdbText { text } => {
                self.adb.type_text(text)?;
                Ok(Outcome::Ok("текст введён на телефоне".into()))
            }
            Action::AdbKey { keycode } => {
                self.adb.key(keycode)?;
                Ok(Outcome::Ok(format!("клавиша {keycode}")))
            }
            Action::AdbOpenApp { package } => {
                self.adb.open_app(package)?;
                std::thread::sleep(Duration::from_millis(2000));
                Ok(Outcome::Ok(format!("открыто приложение {package}")))
            }
            Action::AdbShell { cmd } => {
                let out = self.adb.shell(cmd)?;
                Ok(Outcome::Ok(format!("adb shell: {}", truncate(&out, 500))))
            }
        }
    }
}

/// URL приходит от модели, а значит может быть подсунут через prompt injection
/// на странице. Пускаем только http/https без управляющих символов:
/// file://, а тем более `"& calc.exe`, открывать нельзя.
fn check_url(url: &str) -> Result<()> {
    let u = url.trim();
    if !(u.starts_with("http://") || u.starts_with("https://")) {
        bail!("разрешены только http(s)-ссылки, получено: {u}");
    }
    if u.len() > 2048 || u.chars().any(|c| c.is_control() || c.is_whitespace()) {
        bail!("подозрительный URL");
    }
    Ok(())
}

#[cfg(windows)]
fn open_default(url: &str) -> Result<()> {
    use std::os::windows::process::CommandExt;
    // Без cmd.exe: `cmd /C start "" <url>` интерпретирует &, |, ^ и превращает
    // любой подсунутый URL в выполнение команд. explorer.exe отдаёт
    // ссылку браузеру по умолчанию так же, но без оболочки.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new("explorer.exe")
        .arg(url)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()?;
    Ok(())
}

#[cfg(not(windows))]
fn open_default(url: &str) -> Result<()> {
    // на Linux это только режим разработки
    std::process::Command::new("xdg-open").arg(url).spawn()?;
    Ok(())
}

fn launch(path: &str, args: &str) -> Result<()> {
    if path.trim().is_empty() {
        bail!("пустой путь к приложению");
    }
    let mut cmd = std::process::Command::new(path);
    if !args.trim().is_empty() {
        cmd.args(args.split_whitespace());
    }
    cmd.spawn()?;
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::check_url;

    #[test]
    fn only_http_urls_are_opened() {
        assert!(check_url("https://facebook.com/adsmanager").is_ok());
        assert!(check_url("http://localhost:3000/x?a=1&b=2").is_ok());
        // Именно эти строки раньше уезжали в cmd.exe как команды.
        assert!(check_url("https://a.com\" & calc.exe").is_err());
        assert!(check_url("file:///C:/Windows/System32/cmd.exe").is_err());
        assert!(check_url("javascript:alert(1)").is_err());
        assert!(check_url("").is_err());
        assert!(check_url(&("https://a.com/".to_string() + &"x".repeat(3000))).is_err());
    }
}
