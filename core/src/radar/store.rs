//! Хранилище лидов: SQLite (тот же bundled, что у памяти агента) + экспорт JSON.
//!
//! Дедупликация по ЛЮБОМУ из опознавательных признаков сразу: телефон,
//! сайт, «имя+координаты». Один признак не годится: OSM часто знает только
//! сайт, 2ГИС — только телефон, и один и тот же бизнес попадёт в рассылку
//! дважды. Поэтому признаки живут в отдельной таблице `lead_keys`: совпал хоть
//! один — это тот же лид, а новые признаки дописываются к нему.

use super::score::ScoredLead;
use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::path::Path;

pub struct Store {
    conn: Connection,
}

/// Все признаки одного бизнеса. Совпадение любого считаем тождеством.
pub fn dedup_keys(l: &ScoredLead) -> Vec<String> {
    let mut keys = vec![format!(
        "geo:{}@{:.4},{:.4}",
        l.lead.name.to_lowercase().trim(),
        l.lead.lat,
        l.lead.lon
    )];
    if !l.lead.phone.is_empty() {
        keys.push(format!("tel:{}", l.lead.phone));
    }
    if !l.lead.website.is_empty() {
        keys.push(format!("web:{}", normalize_site(&l.lead.website)));
    }
    keys
}

/// `HTTPS://Salon.KZ/prices?x=1` и `salon.kz` — один и тот же бизнес.
fn normalize_site(raw: &str) -> String {
    let s = raw.trim().to_lowercase();
    let s = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(&s);
    let s = s.strip_prefix("www.").unwrap_or(s);
    s.split(['/', '?', '#'])
        .next()
        .unwrap_or(s)
        .trim_end_matches('.')
        .to_string()
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let conn = Connection::open(path).context("не удалось открыть базу лидов")?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS leads(
                id INTEGER PRIMARY KEY,
                niche TEXT NOT NULL,
                name TEXT NOT NULL,
                phone TEXT NOT NULL,
                website TEXT NOT NULL,
                text_source TEXT NOT NULL,
                lat REAL NOT NULL,
                lon REAL NOT NULL,
                source TEXT NOT NULL,
                intent TEXT NOT NULL,
                score REAL NOT NULL,
                matched TEXT NOT NULL,
                first_seen TEXT NOT NULL,
                last_seen TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS lead_keys(
                niche TEXT NOT NULL,
                key TEXT NOT NULL,
                lead_id INTEGER NOT NULL REFERENCES leads(id) ON DELETE CASCADE,
                PRIMARY KEY(niche, key)
            );
            CREATE INDEX IF NOT EXISTS leads_intent ON leads(niche, intent, score DESC);",
        )?;
        Ok(Self { conn })
    }

    /// Возвращает (новых, обновлённых). Повторный прогон не плодит дубли, но
    /// обновляет score: бизнес мог добавить «каспи» в описание.
    pub fn upsert_all(&mut self, leads: &[ScoredLead]) -> Result<(usize, usize)> {
        let tx = self.conn.transaction()?;
        let mut fresh = 0;
        let mut updated = 0;
        for l in leads {
            let keys = dedup_keys(l);
            let matched = l.matched.join(",");
            let mut found: Option<i64> = None;
            for k in &keys {
                found = tx
                    .query_row(
                        "SELECT lead_id FROM lead_keys WHERE niche=?1 AND key=?2",
                        params![l.niche, k],
                        |r| r.get(0),
                    )
                    .ok();
                if found.is_some() {
                    break;
                }
            }
            let id = match found {
                Some(id) => {
                    // Пустые контакты не затирают уже известные: источники
                    // дополняют друг друга, а не конкурируют.
                    tx.execute(
                        "UPDATE leads SET
                           last_seen=?1,
                           phone=CASE WHEN phone='' THEN ?2 ELSE phone END,
                           website=CASE WHEN website='' THEN ?3 ELSE website END
                         WHERE id=?4",
                        params![l.lead.timestamp, l.lead.phone, l.lead.website, id],
                    )?;
                    // Скоринг перезаписываем только вверх: у второго источника
                    // может быть беднее текст, и HOT не должен деградировать.
                    tx.execute(
                        "UPDATE leads SET intent=?1, score=?2, matched=?3, text_source=?4
                         WHERE id=?5 AND ?2 >= score",
                        params![l.intent.as_str(), l.score, matched, l.lead.text_source, id],
                    )?;
                    updated += 1;
                    id
                }
                None => {
                    tx.execute(
                        "INSERT INTO leads(niche,name,phone,website,text_source,lat,lon,source,
                                           intent,score,matched,first_seen,last_seen)
                         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?12)",
                        params![
                            l.niche,
                            l.lead.name,
                            l.lead.phone,
                            l.lead.website,
                            l.lead.text_source,
                            l.lead.lat,
                            l.lead.lon,
                            l.lead.source,
                            l.intent.as_str(),
                            l.score,
                            matched,
                            l.lead.timestamp,
                        ],
                    )?;
                    fresh += 1;
                    tx.last_insert_rowid()
                }
            };
            for k in &keys {
                tx.execute(
                    "INSERT OR IGNORE INTO lead_keys(niche,key,lead_id) VALUES(?1,?2,?3)",
                    params![l.niche, k, id],
                )?;
            }
        }
        tx.commit()?;
        Ok((fresh, updated))
    }

    pub fn count(&self, niche: &str) -> Result<(i64, i64)> {
        let total: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM leads WHERE niche=?1",
            params![niche],
            |r| r.get(0),
        )?;
        let hot: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM leads WHERE niche=?1 AND intent='HOT'",
            params![niche],
            |r| r.get(0),
        )?;
        Ok((total, hot))
    }
}

/// JSON готов к дальнейшей автоматике: массив объектов, отсортированный по
/// score — сверху те, кому писать первым.
pub fn export_json(path: &Path, leads: &[ScoredLead]) -> Result<()> {
    let mut sorted = leads.to_vec();
    sorted.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    std::fs::write(path, serde_json::to_string_pretty(&sorted)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::radar::score::Intent;
    use crate::radar::source::Lead;

    fn scored(name: &str, phone: &str, intent: Intent) -> ScoredLead {
        ScoredLead {
            lead: Lead {
                name: name.into(),
                phone: phone.into(),
                website: String::new(),
                text_source: "косметолог цена".into(),
                lat: 43.2,
                lon: 76.9,
                source: "fixture".into(),
                timestamp: "2026-01-01T00:00:00Z".into(),
            },
            niche: "косметика".into(),
            intent,
            score: 3.5,
            matched: vec!["косметолог".into()],
        }
    }

    fn db(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("radar-db-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("leads.db");
        std::fs::remove_file(&p).ok();
        p
    }

    #[test]
    fn same_phone_is_not_duplicated_across_runs() {
        let mut s = Store::open(&db("dedup")).unwrap();
        let a = scored("Салон", "+77070000000", Intent::Warm);
        let mut b = scored("Салон (второй источник)", "+77070000000", Intent::Hot);
        b.lead.source = "2gis".into();
        b.lead.timestamp = "2026-02-02T00:00:00Z".into();
        let (fresh, _) = s.upsert_all(std::slice::from_ref(&a)).unwrap();
        assert_eq!(fresh, 1);
        s.upsert_all(&[b]).unwrap();
        let (total, hot) = s.count("косметика").unwrap();
        assert_eq!(total, 1, "один телефон — один лид");
        assert_eq!(hot, 1, "статус должен обновиться до HOT");
    }

    #[test]
    fn one_business_from_two_sources_merges_by_website_and_gains_phone() {
        let mut s = Store::open(&db("merge")).unwrap();
        // OSM знает сайт, 2ГИС — телефон и тот же сайт с www/путём.
        let mut osm = scored("Салон Алия", "", Intent::Warm);
        osm.lead.website = "salon.kz".into();
        let mut gis = scored("Салон Алия (2ГИС)", "+77071112233", Intent::Hot);
        gis.lead.website = "HTTPS://WWW.salon.kz/prices?utm=1".into();
        gis.lead.lat = 43.9; // карточки редко совпадают точкой
        s.upsert_all(&[osm]).unwrap();
        let (fresh, updated) = s.upsert_all(&[gis]).unwrap();
        assert_eq!((fresh, updated), (0, 1));
        assert_eq!(s.count("косметика").unwrap().0, 1, "один бизнес — один лид");
        let phone: String = s
            .conn
            .query_row("SELECT phone FROM leads", [], |r| r.get(0))
            .unwrap();
        assert_eq!(phone, "+77071112233", "телефон должен дописаться к лиду");
    }

    #[test]
    fn better_scored_rerun_wins_and_worse_does_not_downgrade() {
        let mut s = Store::open(&db("score")).unwrap();
        let mut warm = scored("Клиника", "+77070000009", Intent::Warm);
        warm.score = 2.0;
        let mut hot = scored("Клиника", "+77070000009", Intent::Hot);
        hot.score = 7.0;
        s.upsert_all(&[warm.clone()]).unwrap();
        s.upsert_all(&[hot]).unwrap();
        s.upsert_all(&[warm]).unwrap(); // бедный источник не должен ломать HOT
        assert_eq!(s.count("косметика").unwrap(), (1, 1));
    }

    #[test]
    fn site_normalization_ignores_scheme_www_and_path() {
        assert_eq!(
            normalize_site("HTTPS://WWW.Salon.kz/prices?x=1"),
            "salon.kz"
        );
        assert_eq!(normalize_site("http://salon.kz."), "salon.kz");
    }

    #[test]
    fn leads_without_phone_fall_back_to_name_and_geo() {
        let mut s = Store::open(&db("nophone")).unwrap();
        let a = scored("Аптека", "", Intent::Cold);
        let mut b = scored("Аптека", "", Intent::Cold);
        b.lead.lat = 43.9; // другая точка — другой бизнес
        s.upsert_all(&[a, b]).unwrap();
        assert_eq!(s.count("косметика").unwrap().0, 2);
    }

    #[test]
    fn json_export_is_sorted_by_score() {
        let dir = std::env::temp_dir().join(format!("radar-json-{}", std::process::id()));
        let p = dir.join("leads.json");
        let mut low = scored("Низкий", "+77070000001", Intent::Warm);
        low.score = 1.0;
        let mut high = scored("Высокий", "+77070000002", Intent::Hot);
        high.score = 9.0;
        export_json(&p, &[low, high]).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(
            text.find("Высокий").unwrap() < text.find("Низкий").unwrap(),
            "{text}"
        );
        // Плоский JSON: поля лида и скоринга на одном уровне — так его проще
        // отдать в рассылку без вложенности.
        assert!(text.contains("\"intent\": \"HOT\""), "{text}");
    }
}
