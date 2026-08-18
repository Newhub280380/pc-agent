//! Конфиг RADAR: ниша и ГЕО меняются файлом, без пересборки .exe.
//!
//! Требование ТЗ — «на лету менять нишу и ГЕО», поэтому ключевые слова,
//! маркеры транзакции, точка и радиус живут в JSON, а не в коде.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Geo {
    /// Центр поиска (широта/долгота), например Алматы: 43.238949, 76.889709.
    pub lat: f64,
    pub lon: f64,
    /// Радиус в метрах. Overpass и 2ГИС оба ограничивают сверху — берём min.
    pub radius_m: u32,
    #[serde(default)]
    pub city: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceCfg {
    /// OpenStreetMap Overpass: работает без ключа, поэтому прототип
    /// запускается сразу. Контакты берутся из тегов `phone`/`website`,
    /// которые бизнес публикует сам.
    Overpass {
        #[serde(default = "default_overpass")]
        url: String,
        /// Теги OSM, которые считаем «нашей» нишей: `shop=beauty`, `amenity=pharmacy`.
        tags: Vec<String>,
    },
    /// 2ГИС Places API: точнее по КЗ, но нужен ключ (`TWOGIS_API_KEY`).
    TwoGis {
        #[serde(default = "default_2gis")]
        url: String,
    },
    /// Файл с готовым ответом источника — для тестов и повторных прогонов
    /// без сети.
    Fixture { path: String },
}

fn default_overpass() -> String {
    "https://overpass-api.de/api/interpreter".into()
}

fn default_2gis() -> String {
    "https://catalog.api.2gis.com/3.0/items".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RadarConfig {
    /// Имя ниши — попадает в БД, чтобы один файл держал несколько ниш.
    pub niche: String,
    /// Ключевые слова ниши: совпадение по названию/рубрике/описанию.
    pub keywords: Vec<String>,
    /// Слова-маркеры сделки для Intent Scoring.
    #[serde(default = "default_markers")]
    pub markers: Vec<String>,
    pub geo: Geo,
    pub sources: Vec<SourceCfg>,
    /// Лид с числом маркеров >= порога получает статус HOT.
    #[serde(default = "default_hot")]
    pub hot_threshold: u32,
}

fn default_markers() -> Vec<String> {
    [
        "цена",
        "заказ",
        "доставка",
        "каспи",
        "kaspi",
        "курьер",
        "рассрочка",
        "прайс",
        "наличие",
        "запись",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn default_hot() -> u32 {
    2
}

impl RadarConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("{}: не читается", path.display()))?;
        let cfg: Self = serde_json::from_str(text.trim_start_matches('\u{feff}'))
            .with_context(|| format!("{}: битый JSON", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        if self.keywords.is_empty() {
            anyhow::bail!("keywords пуст: без ключевых слов сканировать нечего");
        }
        if self.sources.is_empty() {
            anyhow::bail!("sources пуст: не указан ни один источник");
        }
        if !(-90.0..=90.0).contains(&self.geo.lat) || !(-180.0..=180.0).contains(&self.geo.lon) {
            anyhow::bail!("geo: координаты вне диапазона");
        }
        if self.geo.radius_m == 0 || self.geo.radius_m > 50_000 {
            anyhow::bail!("geo.radius_m: допустимо 1..50000 м");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(body: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("radar-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("radar.json");
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn loads_and_applies_defaults() {
        let p = write(
            r#"{"niche":"косметика","keywords":["филлеры"],
                "geo":{"lat":43.238949,"lon":76.889709,"radius_m":3000},
                "sources":[{"kind":"overpass","tags":["shop=beauty"]}]}"#,
        );
        let cfg = RadarConfig::load(&p).unwrap();
        assert_eq!(cfg.hot_threshold, 2);
        assert!(cfg.markers.iter().any(|m| m == "каспи"));
    }

    #[test]
    fn empty_keywords_are_rejected() {
        let p = write(
            r#"{"niche":"x","keywords":[],"geo":{"lat":0.0,"lon":0.0,"radius_m":100},
                "sources":[{"kind":"overpass","tags":["shop=beauty"]}]}"#,
        );
        let err = RadarConfig::load(&p).unwrap_err().to_string();
        assert!(err.contains("keywords"), "{err}");
    }

    #[test]
    fn insane_radius_is_rejected() {
        let p = write(
            r#"{"niche":"x","keywords":["a"],"geo":{"lat":0.0,"lon":0.0,"radius_m":900000},
                "sources":[{"kind":"overpass","tags":["shop=beauty"]}]}"#,
        );
        assert!(RadarConfig::load(&p)
            .unwrap_err()
            .to_string()
            .contains("radius_m"));
    }
}
