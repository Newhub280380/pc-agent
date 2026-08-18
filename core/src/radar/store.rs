//! Хранилище лидов: SQLite (тот же bundled, что у памяти агента) + экспорт JSON.
//!
//! Дедупликация на уровне БД: `UNIQUE(niche, key)`, где key — нормализованный
//! телефон, а если его нет — сайт или «имя+координаты». Иначе один и тот же
//! бизнес из 2ГИС и OSM попадёт в рассылку дважды.

use super::score::ScoredLead;
use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::path::Path;

pub struct Store {
    conn: Connection,
}

pub fn dedup_key(l: &ScoredLead) -> String {
    if !l.lead.phone.is_empty() {
        return l.lead.phone.clone();
    }
    if !l.lead.website.is_empty() {
        return l.lead.website.to_lowercase();
    }
    format!(
        "{}@{:.4},{:.4}",
        l.lead.name.to_lowercase(),
        l.lead.lat,
        l.lead.lon
    )
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
                key TEXT NOT NULL,
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
                last_seen TEXT NOT NULL,
                UNIQUE(niche, key)
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
            let key = dedup_key(l);
            let matched = l.matched.join(",");
            let existed: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM leads WHERE niche=?1 AND key=?2)",
                params![l.niche, key],
                |r| r.get(0),
            )?;
            tx.execute(
                "INSERT INTO leads(niche,key,name,phone,website,text_source,lat,lon,source,
                                   intent,score,matched,first_seen,last_seen)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?13)
                 ON CONFLICT(niche,key) DO UPDATE SET
                   intent=excluded.intent, score=excluded.score,
                   matched=excluded.matched, last_seen=excluded.last_seen,
                   phone=CASE WHEN leads.phone='' THEN excluded.phone ELSE leads.phone END",
                params![
                    l.niche,
                    key,
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
            if existed {
                updated += 1;
            } else {
                fresh += 1;
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
