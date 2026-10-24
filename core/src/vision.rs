//! Зрение: прицел по экрану и визуальный судья.
//!
//! Зачем отдельный модуль. До этого агент попадал по элементу только если его
//! видело UI-дерево или OCR. На холстах, в Canvas-приложениях, в Instagram в
//! браузере и во всём, что рисует себя картинкой, дерево пусто — и шаг падал с
//! «элемент не найден», хотя человек кнопку видит.
//!
//! Решение: vision-модель класса computer-use (Holo3.1 от H Company, формат
//! OpenAI-совместимый) получает тот же скриншот и описание цели, а отдаёт
//! координаты клика. У Holo своя договорённость: координаты — целые в
//! диапазоне [0, 1000], нормированные к присланной картинке, начало отсчёта
//! слева сверху (hub.hcompany.ai/models-api, «Coordinates in [0, 1000]»).
//! Поэтому масштабируем обратно по размерам того же кадра — иначе промах.
//!
//! Второе применение той же модели — судья: «на экране правда открыт Direct?»,
//! «кнопка нажалась?». Это закрывает дыру self-verify, где подтверждения
//! приходилось искать в тексте. Судья отвечает да/нет/не знаю; «не знаю» —
//! законный ответ, он уходит выше, а не подделывает успех.
//!
//! Почему не отдельный HTTP-клиент к H Company: ядро по уставу не знает ни
//! одного вендора, все ключи и адреса живут в Go-роутере. Здесь только имя
//! провайдера, которым просим ответить (`provider` в запросе роутера).

use crate::llm::{self, LlmClient, Message};
use anyhow::{anyhow, bail, Result};

/// Ответ судьи. Unknown — честное «не разобрал», эскалация выше.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Look {
    Yes,
    No,
    Unknown,
}

/// Глаза агента. Пусто = зрения нет, работаем как раньше (дерево + OCR).
pub struct Eyes {
    llm: LlmClient,
    /// Имя провайдера для роутера: пусто — пусть выбирает сам из vision-умеющих.
    provider: String,
}

/// Инструкция прицела. Короткая намеренно: Holo обучен на таком формате, а
/// длинные рассуждения в этом режиме только добавляют шум.
const AIM_SYSTEM: &str = "Localize an element on the GUI image according to the instruction \
and output the click position. Answer strictly as JSON {\"x\":int,\"y\":int} where x and y are \
integers in [0,1000] normalized to the image, origin top-left.";

const JUDGE_SYSTEM: &str = "Ты визуальный контролёр. Смотришь скриншот и отвечаешь только по \
тому, что видно на картинке. Ответ строго JSON: {\"ok\":true|false,\"почему\":\"коротко\"}. \
Если по картинке понять нельзя — {\"ok\":null,\"почему\":\"не видно\"}.";

impl Eyes {
    /// Провайдер зрения берём из VISION_PROVIDER (по умолчанию holo). Если его
    /// нет в роутере — отдаём None, чтобы агент не бился в заведомо мёртвый
    /// вызов на каждом шаге: 10 секунд таймаута на шаг дороже отсутствия глаз.
    pub fn new(llm: LlmClient, preferred: &str, available: &[(String, bool)]) -> Option<Self> {
        let want = preferred.trim().to_lowercase();
        if want == "нет" || want == "off" {
            return None;
        }
        if !want.is_empty() {
            if let Some((name, _)) = available
                .iter()
                .find(|(n, vision)| *vision && n.eq_ignore_ascii_case(&want))
            {
                return Some(Self {
                    llm,
                    provider: name.clone(),
                });
            }
        }
        // Нужного нет — годится любой, кто принимает картинки: прицел хуже,
        // но «вижу хоть как-то» лучше, чем «не вижу вообще».
        available
            .iter()
            .find(|(_, vision)| *vision)
            .map(|(name, _)| Self {
                llm,
                provider: name.clone(),
            })
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// Координаты клика по описанию цели. (w, h) — размеры ровно того кадра,
    /// который передаём: масштабировать по чужим размерам значит промахнуться.
    pub fn locate(&self, png: &[u8], w: i32, h: i32, target: &str) -> Result<(i32, i32)> {
        if w <= 0 || h <= 0 {
            bail!("неизвестен размер кадра — прицел невозможен");
        }
        let msgs = vec![
            Message::system(AIM_SYSTEM),
            Message::user_with_image(format!("Instruction: {target}"), png),
        ];
        let out = self
            .llm
            .complete_as(&self.provider, &msgs, "aim", true, 0.0)?;
        let (nx, ny) = parse_point(&out.text)?;
        Ok(to_pixels(nx, ny, w, h))
    }

    /// Визуальная проверка факта: «это действительно произошло?».
    pub fn judge(&self, png: &[u8], question: &str) -> Look {
        let msgs = vec![
            Message::system(JUDGE_SYSTEM),
            Message::user_with_image(format!("Вопрос: {question}"), png),
        ];
        match self
            .llm
            .complete_as(&self.provider, &msgs, "judge", true, 0.0)
        {
            Ok(out) => parse_look(&out.text),
            Err(e) => {
                log::warn!("судья зрения недоступен: {e}");
                Look::Unknown
            }
        }
    }
}

/// Holo отвечает то строгим JSON, то «Click(x=120, y=880)» — поддерживаем оба,
/// потому что лишний повторный вызов это секунды и деньги на каждом шаге.
pub fn parse_point(text: &str) -> Result<(i32, i32)> {
    if let Ok(v) = llm::extract_json(text) {
        let pick = |k: &str| -> Option<i64> {
            v.get(k)
                .and_then(|n| n.as_i64().or_else(|| n.as_f64().map(|f| f.round() as i64)))
        };
        if let (Some(x), Some(y)) = (pick("x"), pick("y")) {
            return Ok(clamp_point(x, y));
        }
        // Вариант со вложенным объектом/массивом: {"point":[x,y]} и {"click":{...}}.
        for key in ["point", "click", "position", "coordinate", "coordinates"] {
            match v.get(key) {
                Some(serde_json::Value::Array(a)) if a.len() >= 2 => {
                    let x = a[0].as_i64().unwrap_or(-1);
                    let y = a[1].as_i64().unwrap_or(-1);
                    if x >= 0 && y >= 0 {
                        return Ok(clamp_point(x, y));
                    }
                }
                Some(serde_json::Value::Object(o)) => {
                    if let (Some(x), Some(y)) = (
                        o.get("x").and_then(|n| n.as_i64()),
                        o.get("y").and_then(|n| n.as_i64()),
                    ) {
                        return Ok(clamp_point(x, y));
                    }
                }
                _ => {}
            }
        }
    }
    let (x, y) = scan_pair(text).ok_or_else(|| {
        let head: String = text.chars().take(160).collect();
        anyhow!("в ответе зрения нет координат: {head}")
    })?;
    Ok(clamp_point(x, y))
}

/// Разбор «x=120, y=880», «(120, 880)», «120 880» без regex-зависимости.
fn scan_pair(text: &str) -> Option<(i64, i64)> {
    let mut nums = vec![];
    let mut cur = String::new();
    for ch in text.chars() {
        if ch.is_ascii_digit() {
            cur.push(ch);
            // Переполнение i64 на мусорной строке из тысячи цифр недопустимо.
            if cur.len() > 9 {
                cur.clear();
            }
        } else if !cur.is_empty() {
            if let Ok(n) = cur.parse::<i64>() {
                nums.push(n);
            }
            cur.clear();
        }
    }
    if let Ok(n) = cur.parse::<i64>() {
        nums.push(n);
    }
    match nums.len() {
        0 | 1 => None,
        _ => Some((nums[0], nums[1])),
    }
}

/// Диапазон Holo — [0, 1000]. Выход за него значит либо модель ответила в
/// пикселях, либо соврала; обрезаем, чтобы клик не ушёл за экран.
fn clamp_point(x: i64, y: i64) -> (i32, i32) {
    (x.clamp(0, 1000) as i32, y.clamp(0, 1000) as i32)
}

/// Нормированные [0,1000] → пиксели того же кадра.
pub fn to_pixels(nx: i32, ny: i32, w: i32, h: i32) -> (i32, i32) {
    let x = (nx as i64 * w as i64) / 1000;
    let y = (ny as i64 * h as i64) / 1000;
    // Клик ровно по правой/нижней границе уходит в соседний монитор или мимо
    // окна, поэтому держим внутри кадра.
    (
        (x as i32).clamp(0, (w - 1).max(0)),
        (y as i32).clamp(0, (h - 1).max(0)),
    )
}

pub fn parse_look(text: &str) -> Look {
    let Ok(v) = llm::extract_json(text) else {
        // Модель могла ответить словом. Берём только однозначные формы:
        // «похоже, да» — это не подтверждение факта.
        let t = text.trim().to_lowercase();
        return match t.as_str() {
            "да" | "yes" | "true" => Look::Yes,
            "нет" | "no" | "false" => Look::No,
            _ => Look::Unknown,
        };
    };
    match v.get("ok") {
        Some(serde_json::Value::Bool(true)) => Look::Yes,
        Some(serde_json::Value::Bool(false)) => Look::No,
        _ => Look::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_strict_json() {
        assert_eq!(parse_point("{\"x\":120,\"y\":880}").unwrap(), (120, 880));
    }

    #[test]
    fn parses_holo_click_form() {
        assert_eq!(parse_point("Click(x=250, y=500)").unwrap(), (250, 500));
        assert_eq!(
            parse_point("```json\n{\"x\":1,\"y\":2}\n```").unwrap(),
            (1, 2)
        );
        assert_eq!(parse_point("точка: (10, 20)").unwrap(), (10, 20));
    }

    #[test]
    fn parses_nested_forms() {
        assert_eq!(parse_point("{\"point\":[7,9]}").unwrap(), (7, 9));
        assert_eq!(
            parse_point("{\"click\":{\"x\":3,\"y\":4}}").unwrap(),
            (3, 4)
        );
    }

    #[test]
    fn never_panics_on_garbage() {
        for c in [
            "",
            "   ",
            "```",
            "}{",
            "нет координат",
            "{\"x\":\"сто\"}",
            "9999999999999999999999 8888888888888888888",
            "\u{1f600}",
        ] {
            let _ = parse_point(c);
        }
        assert!(parse_point("нет координат").is_err());
    }

    #[test]
    fn clamps_out_of_range() {
        assert_eq!(parse_point("{\"x\":5000,\"y\":-30}").unwrap(), (1000, 0));
    }

    #[test]
    fn scales_to_pixels() {
        assert_eq!(to_pixels(500, 500, 1920, 1080), (960, 540));
        assert_eq!(to_pixels(0, 0, 1920, 1080), (0, 0));
        // Крайняя точка не должна вылетать за кадр.
        assert_eq!(to_pixels(1000, 1000, 1920, 1080), (1919, 1079));
        assert_eq!(to_pixels(500, 500, 0, 0), (0, 0));
    }

    #[test]
    fn judge_answers() {
        assert_eq!(parse_look("{\"ok\":true}"), Look::Yes);
        assert_eq!(parse_look("{\"ok\":false,\"почему\":\"пусто\"}"), Look::No);
        assert_eq!(parse_look("{\"ok\":null}"), Look::Unknown);
        assert_eq!(parse_look("да"), Look::Yes);
        assert_eq!(parse_look("вроде бы да"), Look::Unknown);
    }

    #[test]
    fn eyes_pick_provider() {
        let llm = LlmClient::new("http://127.0.0.1:1", "t");
        let avail = vec![
            ("kilo".to_string(), false),
            ("holo".to_string(), true),
            ("nvidia".to_string(), true),
        ];
        let eyes = Eyes::new(llm.clone(), "holo", &avail).unwrap();
        assert_eq!(eyes.provider(), "holo");
        // Нужного нет — берём первого зрячего, а не падаем.
        let eyes = Eyes::new(llm.clone(), "holo", &avail[..1].to_vec()).is_none();
        assert!(eyes);
        let eyes = Eyes::new(llm.clone(), "holo", &avail[2..].to_vec()).unwrap();
        assert_eq!(eyes.provider(), "nvidia");
        assert!(Eyes::new(llm, "нет", &avail).is_none());
    }
}
