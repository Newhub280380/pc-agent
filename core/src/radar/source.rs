//! Источники лидов.
//!
//! Осознанное ограничение: собираем ТОЛЬКО контакты, которые организация
//! опубликовала сама как канал входящих обращений (карточка в 2ГИС, теги
//! `phone`/`website` в OSM, сайт). Личные номера частных лиц из переписок и
//! комментариев не парсим: это персональные данные без согласия (ст. 8 закона
//! РК «О персональных данных»), и рассылка по ним ведёт к блокировке номера.
//! Поэтому в структуре лида нет полей вроде «автор сообщения».

use super::config::{Geo, SourceCfg};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Lead {
    /// Название организации — то, что видно в карточке.
    pub name: String,
    /// Публичный телефон в формате +7XXXXXXXXXX (пусто, если не опубликован).
    pub phone: String,
    pub website: String,
    /// Текст, по которому сработало совпадение (рубрика, описание, часы).
    pub text_source: String,
    pub lat: f64,
    pub lon: f64,
    /// Имя источника: overpass | 2gis | fixture.
    pub source: String,
    pub timestamp: String,
}

pub trait Source {
    fn name(&self) -> &'static str;
    /// Возвращает сырые лиды: фильтрацию по ключевым словам и скоринг
    /// делает пайплайн, чтобы источники оставались взаимозаменяемыми.
    fn fetch(&self, keywords: &[String], geo: &Geo) -> Result<Vec<Lead>>;
}

pub fn build(cfg: &SourceCfg, api_key: Option<String>) -> Box<dyn Source> {
    match cfg {
        SourceCfg::Overpass { url, tags } => Box::new(Overpass {
            url: url.clone(),
            tags: tags.clone(),
        }),
        SourceCfg::TwoGis { url } => Box::new(TwoGis {
            url: url.clone(),
            key: api_key.unwrap_or_default(),
        }),
        SourceCfg::Fixture { path } => Box::new(Fixture { path: path.clone() }),
    }
}

/// Нормализация номера под КЗ: 8 707…, 707…, +7 707… → +7707…
/// Нужна для дедупликации: один бизнес встречается в нескольких источниках.
pub fn normalize_phone(raw: &str) -> String {
    let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    let d = match digits.len() {
        11 if digits.starts_with('8') => format!("7{}", &digits[1..]),
        10 if digits.starts_with('7') => format!("7{digits}"),
        _ => digits,
    };
    if d.len() == 11 && d.starts_with('7') {
        format!("+{d}")
    } else {
        String::new() // мусор вроде «звоните» отбрасываем, а не тащим в БД
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

pub struct Overpass {
    url: String,
    tags: Vec<String>,
}

impl Overpass {
    /// Overpass QL: узлы и полигоны с нужным тегом в радиусе. `out center tags`
    /// отдаёт координату даже для way/relation.
    fn query(&self, geo: &Geo) -> String {
        let mut q = String::from("[out:json][timeout:25];(");
        for t in &self.tags {
            let (k, v) = t.split_once('=').unwrap_or((t.as_str(), ""));
            let filter = if v.is_empty() {
                format!("[\"{k}\"]")
            } else {
                format!("[\"{k}\"=\"{v}\"]")
            };
            for kind in ["node", "way"] {
                q.push_str(&format!(
                    "{kind}(around:{},{},{}){filter};",
                    geo.radius_m, geo.lat, geo.lon
                ));
            }
        }
        q.push_str(");out center tags 200;");
        q
    }
}

impl Source for Overpass {
    fn name(&self) -> &'static str {
        "overpass"
    }

    fn fetch(&self, _keywords: &[String], geo: &Geo) -> Result<Vec<Lead>> {
        let body = ureq::post(&self.url)
            .timeout(std::time::Duration::from_secs(60))
            .send_form(&[("data", &self.query(geo))])
            .context("overpass: запрос не прошёл")?
            .into_string()?;
        let json: serde_json::Value = serde_json::from_str(&body).context("overpass: не JSON")?;
        Ok(parse_overpass(&json))
    }
}

pub fn parse_overpass(json: &serde_json::Value) -> Vec<Lead> {
    let mut out = vec![];
    for el in json["elements"].as_array().unwrap_or(&vec![]) {
        let tags = &el["tags"];
        let name = tags["name"].as_str().unwrap_or("").trim().to_string();
        if name.is_empty() {
            continue; // безымянная точка бесполезна для оффера
        }
        let phone = ["phone", "contact:phone", "contact:mobile", "mobile"]
            .iter()
            .filter_map(|k| tags[*k].as_str())
            .map(normalize_phone)
            .find(|p| !p.is_empty())
            .unwrap_or_default();
        let website = ["website", "contact:website", "contact:instagram"]
            .iter()
            .filter_map(|k| tags[*k].as_str())
            .map(|s| s.to_string())
            .next()
            .unwrap_or_default();
        // Текст для скоринга: рубрика + описание + услуги.
        let text = [
            "shop",
            "amenity",
            "healthcare",
            "description",
            "healthcare:speciality",
            "opening_hours",
            "payment:kaspi",
            "brand",
        ]
        .iter()
        .filter_map(|k| tags[*k].as_str())
        .collect::<Vec<_>>()
        .join(" ");
        let (lat, lon) = match (el["lat"].as_f64(), el["lon"].as_f64()) {
            (Some(a), Some(b)) => (a, b),
            _ => (
                el["center"]["lat"].as_f64().unwrap_or_default(),
                el["center"]["lon"].as_f64().unwrap_or_default(),
            ),
        };
        out.push(Lead {
            name,
            phone,
            website,
            text_source: text,
            lat,
            lon,
            source: "overpass".into(),
            timestamp: now(),
        });
    }
    out
}

pub struct TwoGis {
    url: String,
    key: String,
}

impl Source for TwoGis {
    fn name(&self) -> &'static str {
        "2gis"
    }

    fn fetch(&self, keywords: &[String], geo: &Geo) -> Result<Vec<Lead>> {
        if self.key.trim().is_empty() {
            anyhow::bail!("2gis: нет TWOGIS_API_KEY — источник пропущен");
        }
        let mut out = vec![];
        for kw in keywords {
            let resp = ureq::get(&self.url)
                .timeout(std::time::Duration::from_secs(30))
                .query("q", kw)
                .query("point", &format!("{},{}", geo.lon, geo.lat))
                .query("radius", &geo.radius_m.to_string())
                .query("fields", "items.point,items.contact_groups,items.rubrics")
                .query("page_size", "50")
                .query("key", &self.key)
                .call()
                .with_context(|| format!("2gis: запрос по «{kw}» не прошёл"))?
                .into_string()?;
            let json: serde_json::Value = serde_json::from_str(&resp).context("2gis: не JSON")?;
            out.extend(parse_2gis(&json));
        }
        Ok(out)
    }
}

pub fn parse_2gis(json: &serde_json::Value) -> Vec<Lead> {
    let mut out = vec![];
    for item in json["result"]["items"].as_array().unwrap_or(&vec![]) {
        let name = item["name"].as_str().unwrap_or("").trim().to_string();
        if name.is_empty() {
            continue;
        }
        let mut phone = String::new();
        let mut website = String::new();
        for g in item["contact_groups"].as_array().unwrap_or(&vec![]) {
            for c in g["contacts"].as_array().unwrap_or(&vec![]) {
                match c["type"].as_str().unwrap_or("") {
                    "phone" if phone.is_empty() => {
                        phone = normalize_phone(c["value"].as_str().unwrap_or(""));
                    }
                    "website" | "instagram" if website.is_empty() => {
                        website = c["value"].as_str().unwrap_or("").to_string();
                    }
                    _ => {}
                }
            }
        }
        let rubrics = item["rubrics"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .filter_map(|r| r["name"].as_str())
            .collect::<Vec<_>>()
            .join(" ");
        out.push(Lead {
            name,
            phone,
            website,
            text_source: format!("{rubrics} {}", item["address_name"].as_str().unwrap_or("")),
            lat: item["point"]["lat"].as_f64().unwrap_or_default(),
            lon: item["point"]["lon"].as_f64().unwrap_or_default(),
            source: "2gis".into(),
            timestamp: now(),
        });
    }
    out
}

/// Fixture — сохранённый ответ Overpass или 2ГИС. Так прогон повторяем
/// офлайн: полезно и для тестов, и когда лимит API уже выбран.
pub struct Fixture {
    path: String,
}

impl Source for Fixture {
    fn name(&self) -> &'static str {
        "fixture"
    }

    fn fetch(&self, _keywords: &[String], _geo: &Geo) -> Result<Vec<Lead>> {
        let text = std::fs::read_to_string(&self.path)
            .with_context(|| format!("{}: не читается", self.path))?;
        let json: serde_json::Value = serde_json::from_str(&text).context("fixture: не JSON")?;
        let mut leads = parse_overpass(&json);
        leads.extend(parse_2gis(&json));
        for l in &mut leads {
            l.source = "fixture".into();
        }
        Ok(leads)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kz_phones_are_normalized_and_garbage_dropped() {
        assert_eq!(normalize_phone("8 (707) 123-45-67"), "+77071234567");
        assert_eq!(normalize_phone("+7 707 123 45 67"), "+77071234567");
        assert_eq!(normalize_phone("707 123 45 67"), "+77071234567");
        assert_eq!(normalize_phone("звоните в инстаграм"), "");
        assert_eq!(normalize_phone("123"), "");
    }

    #[test]
    fn overpass_answer_is_parsed_with_center_fallback() {
        let json = serde_json::json!({"elements":[
            {"type":"node","lat":43.2,"lon":76.9,"tags":{"name":"Салон","shop":"beauty","phone":"+7 727 000 00 00"}},
            {"type":"way","center":{"lat":43.3,"lon":76.8},"tags":{"name":"Аптека","amenity":"pharmacy"}},
            {"type":"node","lat":1.0,"lon":1.0,"tags":{"shop":"beauty"}}
        ]});
        let leads = parse_overpass(&json);
        assert_eq!(leads.len(), 2, "безымянная точка должна отбрасываться");
        assert_eq!(leads[0].phone, "+77270000000");
        assert_eq!(leads[1].lat, 43.3);
    }

    #[test]
    fn two_gis_answer_is_parsed() {
        let json = serde_json::json!({"result":{"items":[{
            "name":"Клиника","address_name":"пр. Абая 1","point":{"lat":43.1,"lon":76.5},
            "rubrics":[{"name":"косметология"}],
            "contact_groups":[{"contacts":[
                {"type":"phone","value":"8 701 111 22 33"},
                {"type":"website","value":"clinic.kz"}]}]
        }]}});
        let leads = parse_2gis(&json);
        assert_eq!(leads.len(), 1);
        assert_eq!(leads[0].phone, "+77011112233");
        assert_eq!(leads[0].website, "clinic.kz");
        assert!(leads[0].text_source.contains("косметология"));
    }
}
