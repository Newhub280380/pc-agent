//! Псевдо-фаззинг парсеров без nightly и cargo-fuzz.
//!
//! Зачем отдельный модуль: cargo-fuzz требует nightly-тулчейн и отдельный
//! билд, а нам нужно гонять это в обычном CI на stable. Здесь детерминированный
//! ГПСЧ (сид фиксирован — падение всегда воспроизводимо) бросает мусор во все
//! места, куда попадают данные извне: ответ LLM, дамп uiautomator, .env,
//! поисковый запрос к памяти.
//!
//! Правило: любой из этих входов может вернуть ошибку, но НЕ имеет права
//! паниковать — паника в рабочем потоке роняет задачу целиком.

#![cfg(test)]

use crate::agent::action::Decision;
use crate::llm::extract_json;

/// xorshift: без зависимостей и одинаков на всех платформах.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn pick<'a>(&mut self, v: &[&'a str]) -> &'a str {
        v[(self.next() % v.len() as u64) as usize]
    }
}

/// Куски, из которых собираются «почти правильные» ответы модели — именно они
/// ломают парсеры чаще, чем случайные байты.
const CHUNKS: &[&str] = &[
    "{",
    "}",
    "[",
    "]",
    "\"",
    ":",
    ",",
    "```json",
    "```",
    "action",
    "click_xy",
    "type_text",
    "adb_shell",
    "\\u0000",
    "\\",
    "null",
    "true",
    "-1e999",
    "9999999999999999999",
    "привет",
    "\u{1f600}",
    "\n",
    "\t",
    " ",
    "0",
    "x",
    "y",
    "rm -rf /",
    "'; reboot; '",
    "<node ",
    "bounds=",
    "[0,0][100,100]",
    "content-desc=",
    "text=",
    "/>",
    "=",
    "#",
    "OPENAI_API_KEY",
    "ROUTER_TOKEN",
];

fn soup(rng: &mut Rng, parts: usize) -> String {
    (0..parts).map(|_| rng.pick(CHUNKS)).collect()
}

#[test]
fn llm_answers_never_panic() {
    let mut rng = Rng(0x5eed_1234_dead_beef);
    for _ in 0..20_000 {
        let s = {
            let n = (rng.next() % 24) as usize;
            soup(&mut rng, n)
        };
        if let Ok(v) = extract_json(&s) {
            // Второй слой: даже валидный JSON не должен превращаться в действие,
            // если это не наш закрытый набор.
            let _ = serde_json::from_value::<Decision>(v);
        }
    }
}

#[test]
fn ui_dump_never_panics() {
    let mut rng = Rng(0xfeed_face_0000_0001);
    for _ in 0..20_000 {
        let s = {
            let n = (rng.next() % 30) as usize;
            soup(&mut rng, n)
        };
        let _ = crate::android::parse_ui_xml(&s);
    }
}

#[test]
fn memory_queries_never_panic() {
    let dir = std::env::temp_dir().join(format!("pcagent-fuzz-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mem = crate::memory::Memory::open(&dir.join("fuzz.db")).unwrap();
    let mut rng = Rng(0x0bad_c0de_0000_0007);
    for _ in 0..2_000 {
        let q = {
            let n = (rng.next() % 10) as usize;
            soup(&mut rng, n)
        };
        // FTS5 обязан пережить кавычки, звёздочки, NEAR и прочий синтаксис.
        let _ = mem.recall("web:example.com", &q, 5).unwrap();
        let _ = mem.recall_global(&q, 5).unwrap();
        let _ = mem
            .remember("web:example.com", "fact", &q, &q, "", 0.5)
            .unwrap();
    }
}
