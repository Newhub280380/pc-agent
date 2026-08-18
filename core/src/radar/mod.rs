//! RADAR — модуль поиска лидов по нише и ГЕО.
//!
//! Пайплайн: источники → фильтр по нише → Intent Scoring → дедуп → SQLite+JSON.
//! Ниша и ГЕО задаются файлом конфига (`--radar radar.json`), поэтому смена
//! «косметика → доставка» не требует пересборки.
//!
//! Границы по закону и по здравому смыслу (см. также `source.rs`):
//!   * собираются только контакты, опубликованные самим бизнесом как канал
//!     входящих обращений;
//!   * персональные номера из переписок/комментариев не собираются;
//!   * никакой проверки номера в банковских и мессенджер-сервисах: и то, и
//!     другое — обработка чужих персональных данных без согласия и прямой путь
//!     к блокировке номера и штрафу.
//!
//! Что делать с готовой базой легально: писать по бизнес-номеру как B2B-оффер
//! с возможностью отказа, либо гнать трафик на входящие (WhatsApp Business API).

pub mod config;
pub mod score;
pub mod source;
pub mod store;

use anyhow::Result;
use config::RadarConfig;
use score::ScoredLead;
use std::path::Path;

pub struct RadarReport {
    /// Сколько всего лидов в базе по этой нише и сколько из них HOT.
    pub in_db: (i64, i64),
    pub scanned: usize,
    pub kept: usize,
    pub hot: usize,
    pub fresh: usize,
    pub errors: Vec<String>,
    pub leads: Vec<ScoredLead>,
}

impl RadarReport {
    pub fn summary(&self) -> String {
        let mut s = format!(
            "просмотрено {}, в нишу попало {}, HOT {}, новых {}; всего в базе {} (HOT {})",
            self.scanned, self.kept, self.hot, self.fresh, self.in_db.0, self.in_db.1
        );
        for e in &self.errors {
            s.push_str(&format!("\n  источник упал: {e}"));
        }
        // Верхушку показываем сразу: без неё непонятно, что именно нашлось.
        let mut top: Vec<&ScoredLead> = self.leads.iter().collect();
        top.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for l in top.iter().take(5) {
            s.push_str(&format!(
                "\n  {} {} — {} [{}] {}",
                l.intent.as_str(),
                l.score,
                l.lead.name,
                l.matched.join("/"),
                if l.lead.phone.is_empty() {
                    l.lead.website.clone()
                } else {
                    l.lead.phone.clone()
                }
            ));
        }
        s
    }
}

/// Один прогон радара. Ошибка отдельного источника не роняет весь прогон:
/// 2ГИС может отдать 429, а OSM при этом работает.
pub fn run(
    cfg: &RadarConfig,
    db: &Path,
    api_key: Option<String>,
    out: Option<&Path>,
) -> Result<RadarReport> {
    cfg.validate()?;
    let mut raw = vec![];
    let mut errors = vec![];
    for sc in &cfg.sources {
        let src = source::build(sc, api_key.clone());
        match src.fetch(&cfg.keywords, &cfg.geo) {
            Ok(items) => {
                log::info!("radar: {} отдал {} записей", src.name(), items.len());
                raw.extend(items);
            }
            Err(e) => errors.push(format!("{}: {e:#}", src.name())),
        }
    }
    let scanned = raw.len();
    let leads: Vec<ScoredLead> = raw.iter().filter_map(|l| score::score(l, cfg)).collect();
    // Дедуп в памяти до записи: иначе транзакция делает лишние UPDATE.
    let mut seen = std::collections::HashSet::new();
    let leads: Vec<ScoredLead> = leads
        .into_iter()
        .filter(|l| seen.insert(store::dedup_key(l)))
        .collect();
    let hot = leads
        .iter()
        .filter(|l| l.intent == score::Intent::Hot)
        .count();

    let mut st = store::Store::open(db)?;
    let (fresh, _) = st.upsert_all(&leads)?;
    let in_db = st.count(&cfg.niche)?;
    if let Some(p) = out {
        store::export_json(p, &leads)?;
    }
    Ok(RadarReport {
        in_db,
        scanned,
        kept: leads.len(),
        hot,
        fresh,
        errors,
        leads,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("radar-run-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn end_to_end_on_fixture_keeps_only_niche_and_marks_hot() {
        let d = dir("e2e");
        let fixture = d.join("osm.json");
        std::fs::write(
            &fixture,
            serde_json::json!({"elements":[
                {"lat":43.2,"lon":76.9,"tags":{"name":"Косметолог Алия","shop":"beauty",
                    "description":"филлеры цена, запись, каспи","phone":"8 707 111 22 33"}},
                {"lat":43.21,"lon":76.91,"tags":{"name":"Шаурма №1","amenity":"fast_food","phone":"+7 707 999 88 77"}},
                {"lat":43.22,"lon":76.92,"tags":{"name":"Косметолог без связи","shop":"beauty",
                    "description":"филлеры"}}
            ]})
            .to_string(),
        )
        .unwrap();
        let cfg = RadarConfig {
            niche: "косметика".into(),
            keywords: vec!["филлеры".into(), "beauty".into()],
            markers: vec!["цена".into(), "каспи".into(), "запись".into()],
            geo: config::Geo {
                lat: 43.2,
                lon: 76.9,
                radius_m: 5000,
                city: "Алматы".into(),
            },
            sources: vec![config::SourceCfg::Fixture {
                path: fixture.display().to_string(),
            }],
            hot_threshold: 2,
        };
        let out = d.join("leads.json");
        std::fs::remove_file(d.join("leads.db")).ok();
        let rep = run(&cfg, &d.join("leads.db"), None, Some(&out)).unwrap();
        assert_eq!(rep.scanned, 3);
        assert_eq!(rep.kept, 2, "шаурма не должна попасть в нишу");
        assert_eq!(rep.hot, 1);
        assert_eq!(rep.fresh, 2);
        let json = std::fs::read_to_string(&out).unwrap();
        assert!(json.contains("+77071112233"));
        assert!(!json.contains("Шаурма"));
    }

    #[test]
    fn broken_source_does_not_kill_the_run() {
        let d = dir("broken");
        let cfg = RadarConfig {
            niche: "x".into(),
            keywords: vec!["beauty".into()],
            markers: vec![],
            geo: config::Geo {
                lat: 43.2,
                lon: 76.9,
                radius_m: 1000,
                city: String::new(),
            },
            sources: vec![
                config::SourceCfg::Fixture {
                    path: d.join("нет-такого-файла.json").display().to_string(),
                },
                config::SourceCfg::TwoGis {
                    url: "https://example.invalid".into(),
                },
            ],
            hot_threshold: 2,
        };
        let rep = run(&cfg, &d.join("leads2.db"), None, None).unwrap();
        assert_eq!(rep.kept, 0);
        assert_eq!(rep.errors.len(), 2, "{:?}", rep.errors);
        assert!(
            rep.errors.iter().any(|e| e.contains("TWOGIS_API_KEY")),
            "{:?}",
            rep.errors
        );
    }
}
