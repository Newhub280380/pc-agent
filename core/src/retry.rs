//! Повторы с экспоненциальной задержкой и предсказуемым потолком.
//!
//! Зачем отдельный модуль: повторять нужно ВСЁ, что зависит от внешнего мира —
//! вызов LLM (провайдер отвечает 429/5xx), adb (устройство «просыпается»),
//! чтение экрана (окно перерисовывается). Разбросанные по коду `sleep(3)`
//! невозможно ни настроить, ни протестировать.
//!
//! Почему без джиттера: агент один на машине, стада запросов нет, а
//! детерминированная задержка проверяется в тестах и предсказуема в логах.

use anyhow::Result;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Backoff {
    pub attempts: u32,
    pub base: Duration,
    pub max: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            attempts: 3,
            base: Duration::from_millis(500),
            max: Duration::from_secs(8),
        }
    }
}

impl Backoff {
    /// Задержка перед попыткой `i` (нумерация с 0): base * 2^i, но не больше max.
    pub fn delay(&self, i: u32) -> Duration {
        let mult = 1u64.checked_shl(i.min(16)).unwrap_or(u64::MAX);
        let ms = (self.base.as_millis() as u64).saturating_mul(mult);
        Duration::from_millis(ms).min(self.max)
    }

    /// Быстрый вариант для операций, которые дешево повторить.
    pub fn quick() -> Self {
        Self {
            attempts: 3,
            base: Duration::from_millis(150),
            max: Duration::from_secs(2),
        }
    }
}

/// Повторяет операцию, пока не выйдут попытки. Ошибку последней попытки
/// возвращает как есть — с контекстом, сколько раз пробовали.
pub fn retry<T, F>(b: &Backoff, what: &str, mut op: F) -> Result<T>
where
    F: FnMut(u32) -> Result<T>,
{
    let attempts = b.attempts.max(1);
    let mut last: Option<anyhow::Error> = None;
    for i in 0..attempts {
        if i > 0 {
            let d = b.delay(i - 1);
            log::debug!("{what}: повтор {}/{attempts} через {:?}", i + 1, d);
            std::thread::sleep(d);
        }
        match op(i) {
            Ok(v) => return Ok(v),
            Err(e) => {
                log::warn!("{what}: попытка {} не удалась: {e}", i + 1);
                last = Some(e);
            }
        }
    }
    Err(match last {
        Some(e) => e.context(format!("{what}: не вышло за {attempts} попыток")),
        None => anyhow::anyhow!("{what}: не выполнено"),
    })
}

/// Выполнить в отдельном потоке и не ждать дольше таймаута.
///
/// Нужно для нативных вызовов (UI Automation, OCR, adb), которые не умеют
/// отменяться: висящий COM-вызов иначе останавливает агента навсегда.
/// Осознанная плата: поток остаётся висеть, пока вызов не вернётся сам —
/// убить его принудительно нельзя, не рискуя состоянием процесса.
pub fn with_timeout<T, F>(timeout: Duration, what: &str, op: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    let name = what.to_string();
    std::thread::Builder::new()
        .name(format!("timeout:{name}"))
        .spawn(move || {
            let _ = tx.send(op());
        })?;
    match rx.recv_timeout(timeout) {
        Ok(r) => r,
        Err(_) => anyhow::bail!("{what}: нет ответа за {:?}", timeout),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn delay_grows_exponentially_and_is_capped() {
        let b = Backoff {
            attempts: 5,
            base: Duration::from_millis(100),
            max: Duration::from_millis(500),
        };
        assert_eq!(b.delay(0), Duration::from_millis(100));
        assert_eq!(b.delay(1), Duration::from_millis(200));
        assert_eq!(b.delay(2), Duration::from_millis(400));
        assert_eq!(b.delay(3), Duration::from_millis(500));
        // Большой показатель не должен переполнять умножение.
        assert_eq!(b.delay(64), Duration::from_millis(500));
    }

    #[test]
    fn succeeds_on_second_attempt() {
        let b = Backoff {
            attempts: 3,
            base: Duration::from_millis(1),
            max: Duration::from_millis(1),
        };
        let calls = AtomicU32::new(0);
        let got = retry(&b, "тест", |_| {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                anyhow::bail!("первый раз всегда падает");
            }
            Ok(42)
        })
        .expect("вторая попытка должна пройти");
        assert_eq!(got, 42);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn gives_up_and_keeps_error() {
        let b = Backoff {
            attempts: 2,
            base: Duration::from_millis(1),
            max: Duration::from_millis(1),
        };
        let e = retry(&b, "тест", |_| -> Result<()> {
            anyhow::bail!("телефон не отвечает")
        })
        .expect_err("должно сдаться");
        let msg = format!("{e:#}");
        assert!(msg.contains("2 попыток"), "{msg}");
        assert!(msg.contains("телефон не отвечает"), "{msg}");
    }

    #[test]
    fn timeout_returns_error_not_hang() {
        let e = with_timeout(
            Duration::from_millis(100),
            "долгая операция",
            || {
                std::thread::sleep(Duration::from_secs(5));
                Ok(())
            },
        )
        .expect_err("должен быть таймаут");
        assert!(e.to_string().contains("нет ответа"));
    }
}
