//! Политика решений перед действием: когда вообще спрашивать модель JEV.
//!
//! Слои уже разделены: `jev_gate` доказывает шаги свидетелями, `logic` (Z3)
//! проверяет инварианты, `jev` копит локальную статистику классов действий,
//! `decisions` ходит в `typesafe/jev-1.13` по сети. Не хватало одного —
//! правила, которое решает, **нужен ли сетевой вызов на этом шаге**.
//!
//! До этого модуля вызов модели шёл на каждый шаг, до локальной статистики и
//! до grounding, хотя отказ модели на обратимом шаге ни на что не влиял:
//! агент писал «JEV сомневается, но шаг обратимый — делаю» и тратил сотни
//! миллисекунд (а при зависании сети — до восьми секунд) впустую.
//!
//! Правила:
//!
//!   * обратимый шаг с хорошей локальной статистикой решается локально —
//!     микросекунды, ноль сети;
//!   * необратимый шаг всегда идёт в модель: там ошибка дороже задержки;
//!   * локальное «нет» по классу действия — вето до сети (дешёвый отказ);
//!   * вероятность читается полосой 0.6 / 0.4, а не одним порогом: 0.59 и
//!     0.61 — один и тот же «неясно», поэтому решение не дрожит на границе;
//!   * «неясно» и «сеть упала» не пропускают необратимый шаг молча;
//!   * источник решения (`Local` / `Remote` / `Fallback` / `Disabled`) всегда
//!     назван — в журнале видно, кто именно разрешил шаг.
//!
//! Включение: `PCAGENT_JEV_MODE=tiered` (по умолчанию), `always` — прежнее
//! поведение (модель на каждом шаге), `off` — только локальная статистика.

use crate::decisions::Client;
use crate::jev::{Jev, JevVerdict};

/// Выше этой вероятности модель говорит «да».
pub const YES_AT: f64 = 0.6;
/// Ниже этой вероятности модель говорит «нет». Между — «неясно».
pub const NO_AT: f64 = 0.4;
/// Для необратимого шага «да» требует высокой уверенности: 0.6 — это ещё
/// «скорее да», а не «данные подтверждают». Такой шаг всё равно уходит на
/// подтверждение человеком, поэтому цена строгости — один шаг, а цена
/// ошибки — потраченные деньги или удалённые данные.
pub const IRREVERSIBLE_AT: f64 = 0.75;

/// Вопрос к модели. Формулировка — часть протокола: она требует опоры на
/// состояние, а не на «похоже, можно».
pub const QUESTION: &str =
    "Действие необратимо и меняет данные, деньги или доступ. Отвечай «да» только \
     если поля state явно это подтверждают; сомнение — «нет».";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Сеть не используется: только локальная статистика.
    Off,
    /// Модель только для необратимых шагов (по умолчанию).
    Tiered,
    /// Модель на каждом шаге — прежнее поведение.
    Always,
}

impl Mode {
    pub fn from_env() -> Self {
        match std::env::var("PCAGENT_JEV_MODE")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "off" | "0" | "no" => Mode::Off,
            "always" | "all" | "1" | "max" => Mode::Always,
            _ => Mode::Tiered,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Band {
    Yes,
    Unclear,
    No,
}

/// Квантование вероятности до сотых: 0.601 и 0.604 — один и тот же ответ,
/// поэтому повторный запрос не меняет решение из-за шума третьего знака.
pub fn quantize(p: f64) -> f64 {
    if !p.is_finite() {
        return f64::NAN;
    }
    (p * 100.0).round() / 100.0
}

pub fn band(p: f64) -> Band {
    band_for(p, false)
}

/// Полоса для конкретного действия: у необратимого шага планка «да» выше.
pub fn band_for(p: f64, destructive: bool) -> Band {
    let p = quantize(p);
    let yes_at = if destructive { IRREVERSIBLE_AT } else { YES_AT };
    if p.is_nan() {
        return Band::Unclear;
    }
    if p >= yes_at {
        Band::Yes
    } else if p <= NO_AT {
        Band::No
    } else {
        Band::Unclear
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Local,
    Remote,
    Fallback,
    Disabled,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Local => "локальная статистика",
            Source::Remote => "jev-1.13",
            Source::Fallback => "запасное правило",
            Source::Disabled => "проверка выключена",
        }
    }
}

/// Решение по одному действию. `allow = false` — вето на этот шаг.
#[derive(Clone, Debug, PartialEq)]
pub struct Outcome {
    pub allow: bool,
    pub source: Source,
    pub note: String,
}

#[derive(Clone, Copy, Debug)]
pub struct Policy {
    pub mode: Mode,
}

impl Default for Policy {
    fn default() -> Self {
        Self::standard()
    }
}

impl Policy {
    pub fn standard() -> Self {
        Self { mode: Mode::Tiered }
    }

    pub fn from_env() -> Self {
        Self {
            mode: Mode::from_env(),
        }
    }

    /// Нужен ли сетевой вызов на этом шаге.
    pub fn needs_remote(&self, destructive: bool) -> bool {
        match self.mode {
            Mode::Off => false,
            Mode::Always => true,
            Mode::Tiered => destructive,
        }
    }

    /// Решение «делать ли действие». `state` — только наблюдаемое: действие,
    /// цель, накопленные провалы. Свободный текст модели сюда не попадает,
    /// иначе решение опиралось бы на её же догадку.
    pub fn decide(
        &self,
        client: &mut Client,
        local: &Jev,
        action: &str,
        state: &serde_json::Value,
        destructive: bool,
    ) -> Outcome {
        if self.mode == Mode::Off {
            return Outcome {
                allow: true,
                source: Source::Disabled,
                note: "PCAGENT_JEV_MODE=off".into(),
            };
        }

        // Локальное «нет» по классу действия — вето до сети: оно бесплатно.
        if destructive && matches!(local.ask(action), JevVerdict::No) {
            return Outcome {
                allow: false,
                source: Source::Local,
                note: format!("класс действия «{action}» стабильно проваливается"),
            };
        }

        if !self.needs_remote(destructive) {
            return Outcome {
                allow: true,
                source: Source::Local,
                note: match local.confidence(action) {
                    Some(p) => format!("успех класса {:.0}%", p * 100.0),
                    None => "статистики нет, шаг обратимый".into(),
                },
            };
        }

        if !client.enabled() {
            // Ключа нет — это не сбой задачи: необратимый шаг всё равно идёт
            // на подтверждение человеком, поэтому здесь честно «не проверяли».
            return Outcome {
                allow: true,
                source: Source::Disabled,
                note: "OPENROUTER_API_KEY не задан: модель не спрашивали".into(),
            };
        }

        match client.noul(state, QUESTION) {
            Ok(p) => {
                let b = band_for(p, destructive);
                let allow = match b {
                    Band::Yes => true,
                    // Отказ на обратимом шаге не должен выбрасывать готовое
                    // решение модели; на необратимом — вето.
                    Band::No | Band::Unclear => !destructive,
                };
                let verdict = match b {
                    Band::Yes => "да",
                    Band::No => "нет",
                    Band::Unclear => "неясно",
                };
                Outcome {
                    allow,
                    source: Source::Remote,
                    note: format!("{verdict} ({p:.2})"),
                }
            }
            Err(e) => {
                let local_ok = matches!(local.ask(action), JevVerdict::Yes);
                Outcome {
                    allow: !destructive || local_ok,
                    source: Source::Fallback,
                    note: format!("jev недоступен: {e}"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn counting_client(reply: Result<f64, String>) -> (Client, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let inner = calls.clone();
        let client = Client::with_transport(
            "http://jev.local/decisions",
            Some("test-key".into()),
            Box::new(move |_url, _bearer, _body| {
                inner.fetch_add(1, Ordering::SeqCst);
                match &reply {
                    Ok(p) => Ok(json!({"answers": {"q": {"type": "noul", "noul": p}}})),
                    Err(e) => Err(e.clone()),
                }
            }),
        );
        (client, calls)
    }

    fn state() -> serde_json::Value {
        json!({"action": "клик «Купить»", "goal": "оформить заказ", "failures": []})
    }

    #[test]
    fn probability_is_read_as_a_band_not_a_single_threshold() {
        assert_eq!(band(0.61), Band::Yes);
        assert_eq!(band(0.59), Band::Unclear);
        assert_eq!(band(0.45), Band::Unclear);
        assert_eq!(band(0.39), Band::No);
        // Третьего знака нет: шум модели не двигает решение.
        assert_eq!(quantize(0.601), 0.6);
        assert_eq!(quantize(0.604), 0.6);
        assert_eq!(band(f64::NAN), Band::Unclear);
    }

    #[test]
    fn reversible_step_never_touches_the_network() {
        let policy = Policy::standard();
        let (mut client, calls) = counting_client(Ok(0.99));
        let jev = Jev::new();
        let out = policy.decide(&mut client, &jev, "клик по вкладке", &state(), false);
        assert!(out.allow);
        assert_eq!(out.source, Source::Local);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn always_mode_keeps_one_call_per_step() {
        let policy = Policy { mode: Mode::Always };
        let (mut client, calls) = counting_client(Ok(0.9));
        let jev = Jev::new();
        let out = policy.decide(&mut client, &jev, "клик по вкладке", &state(), false);
        assert!(out.allow);
        assert_eq!(out.source, Source::Remote);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn irreversible_step_is_vetoed_on_no_and_on_unclear() {
        let policy = Policy::standard();
        let jev = Jev::new();

        let (mut client, calls) = counting_client(Ok(0.2));
        let out = policy.decide(&mut client, &jev, "клик «Купить»", &state(), true);
        assert!(!out.allow);
        assert_eq!(out.source, Source::Remote);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let (mut client, _) = counting_client(Ok(0.5));
        let out = policy.decide(&mut client, &jev, "клик «Купить»", &state(), true);
        assert!(!out.allow, "полоса «неясно» необратимый шаг не пропускает");
    }

    /// 0.68 — «скорее да» для модели, но не подтверждение для денег: такой шаг
    /// уходит на подтверждение человеком, а обратимый с той же вероятностью идёт.
    #[test]
    fn irreversible_step_needs_more_than_a_weak_yes() {
        let policy = Policy::standard();
        let jev = Jev::new();

        let (mut client, _) = counting_client(Ok(0.68));
        assert!(
            !policy
                .decide(&mut client, &jev, "клик «Оплатить»", &state(), true)
                .allow
        );

        let (mut client, _) = counting_client(Ok(0.68));
        assert!(
            policy
                .decide(&mut client, &jev, "клик по вкладке", &state(), false)
                .allow
        );

        let (mut client, _) = counting_client(Ok(0.76));
        assert!(
            policy
                .decide(&mut client, &jev, "клик «Оплатить»", &state(), true)
                .allow
        );
    }

    #[test]
    fn third_decimal_does_not_change_the_verdict() {
        assert_eq!(band_for(0.601, true), band_for(0.604, true));
        assert_eq!(band_for(0.596, true), band_for(0.604, true));
        assert_eq!(band_for(0.601, false), Band::Yes);
        assert_eq!(band_for(0.74, true), Band::Unclear);
        assert_eq!(band_for(0.751, true), Band::Yes);
    }

    #[test]
    fn irreversible_step_goes_through_on_yes() {
        let policy = Policy::standard();
        let (mut client, _) = counting_client(Ok(0.93));
        let jev = Jev::new();
        let out = policy.decide(&mut client, &jev, "клик «Купить»", &state(), true);
        assert!(out.allow);
        assert_eq!(out.source, Source::Remote);
    }

    #[test]
    fn bad_local_history_vetoes_before_paying_for_network() {
        let policy = Policy::standard();
        let (mut client, calls) = counting_client(Ok(0.99));
        let mut jev = Jev::new();
        for _ in 0..3 {
            jev.observe("клик «Купить»", false);
        }
        let out = policy.decide(&mut client, &jev, "клик «Купить»", &state(), true);
        assert!(!out.allow);
        assert_eq!(out.source, Source::Local);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn network_failure_does_not_silently_approve_irreversible_step() {
        let policy = Policy::standard();
        let (mut client, _) = counting_client(Err("сеть недоступна".into()));
        let jev = Jev::new();
        let out = policy.decide(&mut client, &jev, "клик «Купить»", &state(), true);
        assert!(!out.allow);
        assert_eq!(out.source, Source::Fallback);
        assert!(out.note.contains("сеть недоступна"));
    }

    #[test]
    fn missing_key_is_reported_not_hidden() {
        let policy = Policy::standard();
        let mut client = Client::with_transport(
            "http://jev.local/decisions",
            None,
            Box::new(|_u, _b, _v| Err("не должно вызываться".into())),
        );
        let jev = Jev::new();
        let out = policy.decide(&mut client, &jev, "клик «Купить»", &state(), true);
        assert!(out.allow);
        assert_eq!(out.source, Source::Disabled);
        assert!(out.note.contains("OPENROUTER_API_KEY"));
    }

    #[test]
    fn off_mode_is_local_only() {
        let policy = Policy { mode: Mode::Off };
        let (mut client, calls) = counting_client(Ok(0.0));
        let jev = Jev::new();
        let out = policy.decide(&mut client, &jev, "клик «Купить»", &state(), true);
        assert!(out.allow);
        assert_eq!(out.source, Source::Disabled);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
