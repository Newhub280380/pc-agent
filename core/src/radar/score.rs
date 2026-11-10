//! Intent Scoring: отделяет «есть такой бизнес» от «здесь готовы к сделке».
//!
//! Считаем два независимых сигнала:
//!   * relevance — попадание в нишу (ключевые слова);
//!   * intent — маркеры сделки («цена», «доставка», «каспи», «запись»).
//!
//! Лид без ключевого слова не попадает в выдачу вообще, а HOT получает тот,
//! у кого маркеров не меньше порога И есть канал связи.

use super::config::RadarConfig;
use super::source::Lead;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Intent {
    Hot,
    Warm,
    Cold,
}

impl Intent {
    pub fn as_str(&self) -> &'static str {
        match self {
            Intent::Hot => "HOT",
            Intent::Warm => "WARM",
            Intent::Cold => "COLD",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoredLead {
    #[serde(flatten)]
    pub lead: Lead,
    pub niche: String,
    pub intent: Intent,
    pub score: f64,
    /// Что именно сработало — чтобы менеджер видел причину, а не «магию».
    pub matched: Vec<String>,
}

fn haystack(l: &Lead) -> String {
    format!("{} {} {}", l.name, l.text_source, l.website).to_lowercase()
}

/// Возвращает None, если лид не относится к нише: пайплайн такие выбрасывает.
pub fn score(lead: &Lead, cfg: &RadarConfig) -> Option<ScoredLead> {
    let hay = haystack(lead);
    let mut matched: Vec<String> = cfg
        .keywords
        .iter()
        .filter(|k| hay.contains(&k.to_lowercase()))
        .cloned()
        .collect();
    if matched.is_empty() {
        return None;
    }
    let markers: Vec<String> = cfg
        .markers
        .iter()
        .filter(|m| hay.contains(&m.to_lowercase()))
        .cloned()
        .collect();
    let reachable = !lead.phone.is_empty() || !lead.website.is_empty();
    let intent = if markers.len() as u32 >= cfg.hot_threshold && reachable {
        Intent::Hot
    } else if reachable {
        Intent::Warm
    } else {
        Intent::Cold
    };
    // Вес: ниша важна, но решает наличие канала связи — лид без телефона и
    // сайта нельзя обработать, каким бы релевантным он ни был.
    let score = (matched.len() as f64 * 1.0 + markers.len() as f64 * 1.5)
        * if reachable { 1.0 } else { 0.3 };
    matched.extend(markers);
    Some(ScoredLead {
        lead: lead.clone(),
        niche: cfg.niche.clone(),
        intent,
        score: (score * 100.0).round() / 100.0,
        matched,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::radar::config::{Geo, RadarConfig, SourceCfg};

    fn cfg() -> RadarConfig {
        RadarConfig {
            niche: "косметика".into(),
            keywords: vec!["косметолог".into(), "филлер".into()],
            markers: vec!["цена".into(), "каспи".into(), "запись".into()],
            geo: Geo {
                lat: 43.2,
                lon: 76.9,
                radius_m: 3000,
                city: "Алматы".into(),
            },
            sources: vec![SourceCfg::Fixture { path: "x".into() }],
            hot_threshold: 2,
        }
    }

    fn lead(name: &str, text: &str, phone: &str) -> Lead {
        Lead {
            name: name.into(),
            phone: phone.into(),
            website: String::new(),
            text_source: text.into(),
            lat: 43.2,
            lon: 76.9,
            source: "fixture".into(),
            timestamp: "now".into(),
        }
    }

    #[test]
    fn off_niche_lead_is_dropped() {
        assert!(score(&lead("Шаурма", "fast_food", "+77070000000"), &cfg()).is_none());
    }

    #[test]
    fn markers_and_channel_make_it_hot() {
        let s = score(
            &lead("Косметолог Алия", "цена запись каспи", "+77070000000"),
            &cfg(),
        )
        .unwrap();
        assert_eq!(s.intent, Intent::Hot);
        assert!(s.score > 4.0, "{}", s.score);
        assert!(s.matched.iter().any(|m| m == "каспи"));
    }

    #[test]
    fn no_contact_channel_means_cold_even_with_markers() {
        let s = score(&lead("Филлеры Астана", "цена каспи", ""), &cfg()).unwrap();
        assert_eq!(s.intent, Intent::Cold);
    }

    #[test]
    fn single_marker_stays_warm() {
        let s = score(&lead("Косметолог", "цена", "+77070000000"), &cfg()).unwrap();
        assert_eq!(s.intent, Intent::Warm);
    }
}
