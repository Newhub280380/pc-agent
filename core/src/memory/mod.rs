//! Память агента — «стеллаж с полками».
//!
//! ТРЕБОВАНИЕ ПОЛЬЗОВАТЕЛЯ: агент не должен читать весь массив памяти.
//! Реализация двухуровневая:
//!   1. ПОЛКА (shelf) — контекст: домен сайта, имя приложения, пакет Android,
//!      либо служебные `lessons` / `profile`. Полка выбирается детерминированно
//!      по текущему экрану (URL/заголовок окна), без обращения к LLM.
//!   2. ВНУТРИ ПОЛКИ — полнотекстовый поиск SQLite FTS5 (BM25) + вес свежести
//!      и полезности. В контекст модели уезжает 5-10 записей, а не 10 000.
//!
//! Почему SQLite, а не векторная БД:
//!   - один файл, ноль внешних сервисов, bundled в .exe;
//!   - FTS5/BM25 на конкретных UI-фактах («кнопка Оплатить внизу справа»)
//!     работает не хуже эмбеддингов и стоит 0 токенов и 0 мс сети;
//!   - при 100k+ записей и «смысловых» запросах имеет смысл добавить
//!     эмбеддинги — это в roadmap v2 (sqlite-vec), схема уже готова: колонка
//!     `embedding BLOB` зарезервирована.
//!
//! Узкое место: FTS5 плохо ищет по синонимам («купить» vs «оформить заказ»).
//! Варианты: (а) при записи сохранять нормализованный смысл через LLM
//! (делаем — поле `summary`), (б) добавить эмбеддинги, (в) вести словарь
//! синонимов домена. Сейчас реализован вариант (а) как самый дешёвый.

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryItem {
    pub id: i64,
    pub shelf: String,
    /// fact | ui_hint | credential_hint | lesson | outcome
    pub kind: String,
    pub title: String,
    pub body: String,
    pub summary: String,
    pub importance: f64,
    pub uses: i64,
    pub created_at: String,
    pub last_used: String,
}

pub struct Memory {
    conn: Connection,
}

impl Memory {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let conn = Connection::open(path).context("не удалось открыть файл памяти")?;
        // WAL: GUI-поток читает лог/статистику, пока агент пишет — без блокировок.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let m = Self { conn };
        m.migrate()?;
        Ok(m)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS memories (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                shelf TEXT NOT NULL,
                kind TEXT NOT NULL,
                title TEXT NOT NULL,
                body TEXT NOT NULL,
                summary TEXT NOT NULL DEFAULT '',
                importance REAL NOT NULL DEFAULT 0.5,
                uses INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                last_used TEXT NOT NULL,
                embedding BLOB
            );
            CREATE INDEX IF NOT EXISTS idx_mem_shelf ON memories(shelf, kind);

            -- FTS5 внешним содержимым: индекс не дублирует данные.
            CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
                title, body, summary, content='memories', content_rowid='id',
                tokenize='unicode61 remove_diacritics 2'
            );
            CREATE TRIGGER IF NOT EXISTS mem_ai AFTER INSERT ON memories BEGIN
                INSERT INTO memories_fts(rowid, title, body, summary)
                VALUES (new.id, new.title, new.body, new.summary);
            END;
            CREATE TRIGGER IF NOT EXISTS mem_ad AFTER DELETE ON memories BEGIN
                INSERT INTO memories_fts(memories_fts, rowid, title, body, summary)
                VALUES ('delete', old.id, old.title, old.body, old.summary);
            END;
            CREATE TRIGGER IF NOT EXISTS mem_au AFTER UPDATE ON memories BEGIN
                INSERT INTO memories_fts(memories_fts, rowid, title, body, summary)
                VALUES ('delete', old.id, old.title, old.body, old.summary);
                INSERT INTO memories_fts(rowid, title, body, summary)
                VALUES (new.id, new.title, new.body, new.summary);
            END;

            -- Карточка полки: краткий индекс, который агент читает ПЕРВЫМ.
            -- Это и есть «прочитать нужную полку, а не весь массив».
            CREATE TABLE IF NOT EXISTS shelves (
                name TEXT PRIMARY KEY,
                digest TEXT NOT NULL DEFAULT '',
                items INTEGER NOT NULL DEFAULT 0,
                updated_at TEXT NOT NULL
            );

            -- Самообучение: подпись ошибки -> рабочий фикс.
            CREATE TABLE IF NOT EXISTS lessons (
                signature TEXT PRIMARY KEY,
                context TEXT NOT NULL,
                cause TEXT NOT NULL,
                fix TEXT NOT NULL,
                hits INTEGER NOT NULL DEFAULT 1,
                success INTEGER NOT NULL DEFAULT 0,
                updated_at TEXT NOT NULL
            );

            -- Журнал шагов: сырьё для разбора полётов и отчётов.
            CREATE TABLE IF NOT EXISTS steps (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                task_id TEXT NOT NULL,
                step INTEGER NOT NULL,
                action TEXT NOT NULL,
                result TEXT NOT NULL,
                ok INTEGER NOT NULL,
                created_at TEXT NOT NULL
            );
            "#,
        )?;
        Ok(())
    }

    /// Нормализация имени полки. Домен важнее пути: facebook.com/ads и
    /// facebook.com/settings — одна полка, иначе знания дробятся в пыль.
    pub fn shelf_for(context: &str) -> String {
        let c = context.trim().to_lowercase();
        if let Some(rest) = c.split("://").nth(1) {
            let host = rest.split('/').next().unwrap_or(rest);
            let host = host.trim_start_matches("www.");
            return format!("web:{host}");
        }
        if c.contains(".apk") || c.matches('.').count() >= 2 && !c.contains(' ') {
            return format!("app:{c}");
        }
        let short: String = c.chars().take(48).collect();
        format!("app:{short}")
    }

    pub fn remember(
        &self,
        shelf: &str,
        kind: &str,
        title: &str,
        body: &str,
        summary: &str,
        importance: f64,
    ) -> Result<i64> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO memories (shelf, kind, title, body, summary, importance, created_at, last_used)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
            params![shelf, kind, title, body, summary, importance, now],
        )?;
        let id = self.conn.last_insert_rowid();
        self.conn.execute(
            "INSERT INTO shelves (name, items, updated_at) VALUES (?1, 1, ?2)
             ON CONFLICT(name) DO UPDATE SET items = items + 1, updated_at = ?2",
            params![shelf, now],
        )?;
        Ok(id)
    }

    /// Поиск ТОЛЬКО внутри полки. Ранжирование: BM25 + важность + свежесть.
    pub fn recall(&self, shelf: &str, query: &str, limit: usize) -> Result<Vec<MemoryItem>> {
        let q = sanitize_fts(query);
        if q.is_empty() {
            return self.recent(shelf, limit);
        }
        let mut stmt = self.conn.prepare(
            "SELECT m.id, m.shelf, m.kind, m.title, m.body, m.summary, m.importance, m.uses,
                    m.created_at, m.last_used,
                    bm25(memories_fts) AS rank
             FROM memories_fts f
             JOIN memories m ON m.id = f.rowid
             WHERE memories_fts MATCH ?1 AND m.shelf = ?2
             ORDER BY (rank - m.importance * 2.0 - MIN(m.uses, 10) * 0.1) ASC
             LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(params![q, shelf, limit as i64], row_to_item)?
            .collect::<Result<Vec<_>, _>>()?;
        self.touch(&rows)?;
        Ok(rows)
    }

    /// Поиск по всем полкам — нужен, когда задача новая и полка неизвестна.
    pub fn recall_global(&self, query: &str, limit: usize) -> Result<Vec<MemoryItem>> {
        let q = sanitize_fts(query);
        if q.is_empty() {
            return Ok(vec![]);
        }
        let mut stmt = self.conn.prepare(
            "SELECT m.id, m.shelf, m.kind, m.title, m.body, m.summary, m.importance, m.uses,
                    m.created_at, m.last_used, bm25(memories_fts) AS rank
             FROM memories_fts f JOIN memories m ON m.id = f.rowid
             WHERE memories_fts MATCH ?1
             ORDER BY (rank - m.importance * 2.0) ASC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![q, limit as i64], row_to_item)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn recent(&self, shelf: &str, limit: usize) -> Result<Vec<MemoryItem>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, shelf, kind, title, body, summary, importance, uses, created_at, last_used
             FROM memories WHERE shelf = ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![shelf, limit as i64], row_to_item)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn touch(&self, items: &[MemoryItem]) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        for it in items {
            self.conn.execute(
                "UPDATE memories SET uses = uses + 1, last_used = ?2 WHERE id = ?1",
                params![it.id, now],
            )?;
        }
        Ok(())
    }

    pub fn shelf_digest(&self, shelf: &str) -> Result<String> {
        Ok(self
            .conn
            .query_row(
                "SELECT digest FROM shelves WHERE name = ?1",
                params![shelf],
                |r| r.get::<_, String>(0),
            )
            .unwrap_or_default())
    }

    pub fn set_shelf_digest(&self, shelf: &str, digest: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO shelves (name, digest, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(name) DO UPDATE SET digest = ?2, updated_at = ?3",
            params![shelf, digest, now],
        )?;
        Ok(())
    }

    // ---- самообучение ----

    pub fn save_lesson(
        &self,
        signature: &str,
        context: &str,
        cause: &str,
        fix: &str,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO lessons (signature, context, cause, fix, updated_at) VALUES (?1,?2,?3,?4,?5)
             ON CONFLICT(signature) DO UPDATE SET hits = hits + 1, cause = ?3, fix = ?4, updated_at = ?5",
            params![signature, context, cause, fix, now],
        )?;
        Ok(())
    }

    pub fn mark_lesson_worked(&self, signature: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE lessons SET success = success + 1 WHERE signature = ?1",
            params![signature],
        )?;
        Ok(())
    }

    /// Уроки, релевантные текущему контексту. Подмешиваются в промпт ДО
    /// действия — именно это делает агента «умнее с каждой ошибкой».
    pub fn lessons_for(&self, context: &str, limit: usize) -> Result<Vec<(String, String)>> {
        let like = format!("%{}%", context.to_lowercase());
        let mut stmt = self.conn.prepare(
            "SELECT cause, fix FROM lessons
             WHERE lower(context) LIKE ?1
             ORDER BY (success + 1.0) / (hits + 1.0) DESC, hits DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![like, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<Vec<(String, String)>, _>>()?;
        Ok(rows)
    }

    pub fn log_step(
        &self,
        task_id: &str,
        step: i64,
        action: &str,
        result: &str,
        ok: bool,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO steps (task_id, step, action, result, ok, created_at) VALUES (?1,?2,?3,?4,?5,?6)",
            params![task_id, step, action, result, ok as i32, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn stats(&self) -> Result<(i64, i64, i64)> {
        let mems: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))?;
        let shelves: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM shelves", [], |r| r.get(0))?;
        let lessons: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM lessons", [], |r| r.get(0))?;
        Ok((mems, shelves, lessons))
    }
}

fn row_to_item(r: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryItem> {
    Ok(MemoryItem {
        id: r.get(0)?,
        shelf: r.get(1)?,
        kind: r.get(2)?,
        title: r.get(3)?,
        body: r.get(4)?,
        summary: r.get(5)?,
        importance: r.get(6)?,
        uses: r.get(7)?,
        created_at: r.get(8)?,
        last_used: r.get(9)?,
    })
}

/// FTS5 падает на спецсимволах пользовательского ввода — чистим и склеиваем
/// токены через OR, чтобы частичное совпадение тоже находилось.
fn sanitize_fts(q: &str) -> String {
    let tokens: Vec<String> = q
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().count() >= 3)
        .take(12)
        .map(|t| format!("{}*", t.to_lowercase()))
        .collect();
    tokens.join(" OR ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Memory {
        // Файловая БД в tempdir, а не :memory:, потому что нас интересует
        // именно поведение реального SQLite с FTS5 и WAL.
        let dir = std::env::temp_dir().join(format!("pcagent-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{}.db", rand::random::<u64>()));
        Memory::open(&path).unwrap()
    }

    #[test]
    fn shelf_groups_by_domain() {
        assert_eq!(
            Memory::shelf_for("https://www.facebook.com/adsmanager/x"),
            "web:facebook.com"
        );
        assert_eq!(
            Memory::shelf_for("https://facebook.com/settings"),
            "web:facebook.com"
        );
        assert_eq!(
            Memory::shelf_for("com.google.android.youtube"),
            "app:com.google.android.youtube"
        );
    }

    #[test]
    fn recall_stays_inside_its_shelf() {
        let m = mem();
        m.remember(
            "web:a.com",
            "fact",
            "Кнопка оплатить",
            "внизу справа",
            "",
            0.5,
        )
        .unwrap();
        m.remember(
            "web:b.com",
            "fact",
            "Кнопка оплатить",
            "вверху слева",
            "",
            0.5,
        )
        .unwrap();

        let found = m.recall("web:a.com", "кнопка оплатить", 5).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].body, "внизу справа");
    }

    #[test]
    fn recall_survives_punctuation_in_query() {
        let m = mem();
        m.remember(
            "web:a.com",
            "fact",
            "Форма входа",
            "email + пароль",
            "",
            0.5,
        )
        .unwrap();
        // FTS5 упал бы на голых спецсимволах — проверяем санитайзер.
        let found = m
            .recall("web:a.com", "форма (входа)? \"email\"", 5)
            .unwrap();
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn lessons_are_scoped_and_countable() {
        let m = mem();
        m.save_lesson("sig1", "web:a.com", "кнопка не нажалась", "жди загрузки")
            .unwrap();
        m.mark_lesson_worked("sig1").unwrap();
        let les = m.lessons_for("web:a.com", 5).unwrap();
        assert_eq!(les.len(), 1);
        assert!(m.lessons_for("web:other.com", 5).unwrap().is_empty());
    }

    #[test]
    fn digest_round_trips() {
        let m = mem();
        m.set_shelf_digest("web:a.com", "тут логин через телефон")
            .unwrap();
        assert_eq!(
            m.shelf_digest("web:a.com").unwrap(),
            "тут логин через телефон"
        );
        assert_eq!(m.shelf_digest("web:none.com").unwrap(), "");
    }
}
