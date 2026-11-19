//! Self-change receipts и drift-метрика (контур RAM, epic 9-3, Д0).
//!
//! Каждая правка narrative/judgment агентом сопровождается receipt'ом:
//! verdict `grounded` (цитата из IDENTITY.md найдена ровно один раз, ≥
//! quote_min_chars) или `contested` (правка принята, но проекция на ось
//! не выразима — копится в drift-метрике). Таблицу создаёт
//! `SqliteMemory::init_schema` (единый источник DDL, как memory_refs в 9-2);
//! здесь — только чтение/запись поверх существующего файла.
//!
//! Доступ — по пути к brain.db (паттерн 9-2: вызывающий с data_dir открывает
//! файл напрямую, без изменения Memory-трейта апстрима).

use crate::permissions::QuoteVerdict;
use std::path::{Path, PathBuf};

/// Имя таблицы receipt'ов самоизменения.
pub const SELF_CHANGE_RECEIPTS_TABLE: &str = "self_change_receipts";

/// meta-ключ drift-счётчика в memory_meta.
pub const DRIFT_META_KEY: &str = "self_change_drift";

/// meta-ключ общего счётчика отказов гейта полномочий (epic 9-8, BH-13).
pub const DENIALS_TOTAL_META_KEY: &str = "permissions_denials_total";

/// Префикс per-agent ключей отказов гейта в memory_meta.
pub const DENIALS_AGENT_META_PREFIX: &str = "permissions_denials:";

/// Busy-timeout записи receipt'ов (ревью-виток 1, BH-4): агентский ход,
/// `memory stats` и гигиена могут держать brain.db одновременно; без
/// таймаута SQLITE_BUSY молча роняет receipt, и drift-метрика расходится
/// с фактом. 5 с — тот же порядок, что sqlite_open_timeout_secs.
pub(crate) const RECEIPT_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Открыть соединение для receipt'ов с busy_timeout.
fn open_receipts_connection(db_path: &Path) -> anyhow::Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open(db_path)?;
    conn.busy_timeout(RECEIPT_BUSY_TIMEOUT)?;
    Ok(conn)
}

/// Путь к brain.db относительно data_dir (общий с 9-2/гигиеной).
#[must_use]
pub fn brain_db_path(data_dir: &Path) -> PathBuf {
    data_dir.join("memory").join("brain.db")
}

/// Сводка receipt'ов для `memory stats` и self-review отчётов (OQ-3).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReceiptStats {
    /// Всего receipt'ов.
    pub total: u64,
    /// Вердикт grounded.
    pub grounded: u64,
    /// Вердикт contested.
    pub contested: u64,
    /// Drift-метрика: сумма contested-правок без проекции на ось.
    pub drift: u64,
}

/// Записать receipt самоизменения. Contested-вердикт атомарно увеличивает
/// drift-счётчик (та же транзакция — отчёт не расходится с фактом).
pub fn record_self_change_receipt(
    conn: &rusqlite::Connection,
    agent: &str,
    kind: &str,
    quote: Option<&str>,
    verdict: QuoteVerdict,
) -> anyhow::Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO self_change_receipts (agent, kind, quote, verdict, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            agent,
            kind,
            quote,
            verdict.as_str(),
            chrono::Local::now().to_rfc3339()
        ],
    )?;
    if verdict.is_contested() {
        tx.execute(
            "INSERT INTO memory_meta (key, value) VALUES (?1, '1') \
             ON CONFLICT(key) DO UPDATE SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)",
            rusqlite::params![DRIFT_META_KEY],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Сводка receipt'ов по открытому соединению. Таблицы нет (БД старше
/// миграции) — нули, не ошибка.
pub fn self_change_receipt_stats(conn: &rusqlite::Connection) -> anyhow::Result<ReceiptStats> {
    let mut stats = ReceiptStats::default();
    let table_exists: bool = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        rusqlite::params![SELF_CHANGE_RECEIPTS_TABLE],
        |row| {
            let n: i64 = row.get(0)?;
            Ok(n > 0)
        },
    )?;
    if table_exists {
        let (total, grounded, contested): (i64, i64, i64) = conn.query_row(
            "SELECT COUNT(*), \
                    COALESCE(SUM(CASE WHEN verdict = 'grounded' THEN 1 ELSE 0 END), 0), \
                    COALESCE(SUM(CASE WHEN verdict = 'contested' THEN 1 ELSE 0 END), 0) \
             FROM self_change_receipts",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        stats.total = u64::try_from(total).unwrap_or(0);
        stats.grounded = u64::try_from(grounded).unwrap_or(0);
        stats.contested = u64::try_from(contested).unwrap_or(0);
    }
    let drift: Option<String> = conn
        .query_row(
            "SELECT value FROM memory_meta WHERE key = ?1",
            rusqlite::params![DRIFT_META_KEY],
            |row| row.get(0),
        )
        .ok();
    stats.drift = drift.and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    Ok(stats)
}

/// Записать receipt по пути к БД. Файла нет — no-op с WARN в логе вызывающего
/// не нужен: реceipt не может существовать без БД, в которой правили.
pub fn record_self_change_receipt_at(
    db_path: &Path,
    agent: &str,
    kind: &str,
    quote: Option<&str>,
    verdict: QuoteVerdict,
) -> anyhow::Result<()> {
    if !db_path.exists() {
        return Ok(());
    }
    let conn = open_receipts_connection(db_path)?;
    record_self_change_receipt(&conn, agent, kind, quote, verdict)
}

/// Сводка по пути к БД; файла нет — нули.
pub fn self_change_receipt_stats_at(db_path: &Path) -> anyhow::Result<ReceiptStats> {
    if !db_path.exists() {
        return Ok(ReceiptStats::default());
    }
    let conn =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(RECEIPT_BUSY_TIMEOUT)?;
    self_change_receipt_stats(&conn)
}

// ── Denial telemetry (epic 9-8, BH-13) ───────────────────────────────

/// Сводка отказов гейта полномочий для `memory stats` и self-review:
/// общий total + per-agent счётчики. Производные метрики (memory_meta),
/// не конфиг.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DenialStats {
    /// Всего отказов по всем агентам.
    pub total: u64,
    /// Per-agent счётчики, отсортированные по убыванию count.
    pub per_agent: Vec<(String, u64)>,
}

/// Проверить существование таблицы (паттерн self_change_receipt_stats).
fn meta_table_exists(conn: &rusqlite::Connection) -> anyhow::Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'memory_meta'",
        [],
        |row| row.get(0),
    )?;
    Ok(n > 0)
}

/// Записать отказ гейта: атомарный инкремент общего и per-agent
/// счётчиков (та же транзакция — отчёт не расходится с фактом; тот же
/// upsert-арифметический паттерн, что drift-счётчик). Таблицы
/// `memory_meta` нет (БД старше миграции) — no-op.
pub fn record_denial(conn: &rusqlite::Connection, agent: &str) -> anyhow::Result<()> {
    if !meta_table_exists(conn)? {
        return Ok(());
    }
    let tx = conn.unchecked_transaction()?;
    for key in [
        DENIALS_TOTAL_META_KEY.to_string(),
        format!("{DENIALS_AGENT_META_PREFIX}{agent}"),
    ] {
        tx.execute(
            "INSERT INTO memory_meta (key, value) VALUES (?1, '1') \
             ON CONFLICT(key) DO UPDATE SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)",
            rusqlite::params![key],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// `record_denial` по пути к БД. Файла нет — no-op (паттерн
/// record_self_change_receipt_at: счётчик не может существовать без БД
/// гейта).
pub fn record_denial_at(db_path: &Path, agent: &str) -> anyhow::Result<()> {
    if !db_path.exists() {
        return Ok(());
    }
    let conn = open_receipts_connection(db_path)?;
    record_denial(&conn, agent)
}

/// Сводка отказов по открытому соединению. Таблицы `memory_meta` нет
/// (БД старше миграции) — нули, не ошибка.
pub fn denial_stats(conn: &rusqlite::Connection) -> anyhow::Result<DenialStats> {
    let mut stats = DenialStats::default();
    if !meta_table_exists(conn)? {
        return Ok(stats);
    }
    // substr вместо LIKE (ревью-виток 1, ECH): `_` в префиксе —
    // односимвольный wildcard, чужой ключ вида `permissionsXdenials:…`
    // попал бы в per-agent, но не в total.
    let mut stmt = conn.prepare(
        "SELECT key, value FROM memory_meta \
         WHERE key = ?1 OR substr(key, 1, length(?2)) = ?2",
    )?;
    let rows = stmt.query_map(
        rusqlite::params![DENIALS_TOTAL_META_KEY, DENIALS_AGENT_META_PREFIX],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    )?;
    for row in rows {
        let (key, value) = row?;
        let count = value.parse::<u64>().unwrap_or(0);
        if key == DENIALS_TOTAL_META_KEY {
            stats.total = count;
        } else if let Some(alias) = key.strip_prefix(DENIALS_AGENT_META_PREFIX) {
            stats.per_agent.push((alias.to_string(), count));
        }
    }
    stats
        .per_agent
        .sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    Ok(stats)
}

/// Сводка отказов по пути к БД; файла нет — нули.
pub fn denial_stats_at(db_path: &Path) -> anyhow::Result<DenialStats> {
    if !db_path.exists() {
        return Ok(DenialStats::default());
    }
    let conn =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(RECEIPT_BUSY_TIMEOUT)?;
    denial_stats(&conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_with_receipts() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let db = brain_db_path(tmp.path());
        let mem = crate::sqlite::SqliteMemory::new("test", tmp.path()).unwrap();
        drop(mem);
        (tmp, db)
    }

    #[test]
    fn record_and_read_back_grounded_and_contested() {
        let (_tmp, db) = mem_with_receipts();
        record_self_change_receipt_at(
            &db,
            "legalplat",
            "narrative",
            Some("стать надёжным хранителем контура"),
            QuoteVerdict::Grounded,
        )
        .unwrap();
        record_self_change_receipt_at(
            &db,
            "legalplat",
            "narrative",
            None,
            QuoteVerdict::ContestedMissing,
        )
        .unwrap();
        record_self_change_receipt_at(
            &db,
            "legalplat",
            "narrative",
            Some("неоднозначная цитата"),
            QuoteVerdict::ContestedMultiple,
        )
        .unwrap();

        let stats = self_change_receipt_stats_at(&db).unwrap();
        assert_eq!(stats.total, 3);
        assert_eq!(stats.grounded, 1);
        assert_eq!(stats.contested, 2);
        assert_eq!(stats.drift, 2, "drift копит contested-правки");

        // Receipt row keeps the grounding quote and a timestamp (I/O matrix:
        // «запись принята + receipt (quote, created)»).
        let conn = rusqlite::Connection::open(&db).unwrap();
        let (quote, verdict, created_at): (Option<String>, String, String) = conn
            .query_row(
                "SELECT quote, verdict, created_at FROM self_change_receipts \
                 WHERE verdict = 'grounded'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(quote.as_deref(), Some("стать надёжным хранителем контура"));
        assert_eq!(verdict, "grounded");
        assert!(!created_at.is_empty());
        let missing_quote: Option<String> = conn
            .query_row(
                "SELECT quote FROM self_change_receipts WHERE verdict = 'contested' LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(missing_quote, None);
    }

    #[test]
    fn drift_increments_per_contested_receipt() {
        let (_tmp, db) = mem_with_receipts();
        for _ in 0..3 {
            record_self_change_receipt_at(
                &db,
                "a",
                "narrative",
                None,
                QuoteVerdict::ContestedMissing,
            )
            .unwrap();
        }
        record_self_change_receipt_at(&db, "a", "narrative", Some("x"), QuoteVerdict::Grounded)
            .unwrap();
        let stats = self_change_receipt_stats_at(&db).unwrap();
        assert_eq!(stats.drift, 3);
        assert_eq!(stats.grounded, 1);
        // Перечитывание не дублирует метрику.
        let again = self_change_receipt_stats_at(&db).unwrap();
        assert_eq!(again.drift, 3);
    }

    #[test]
    fn missing_db_is_zero_stats_and_noop_write() {
        let tmp = tempfile::tempdir().unwrap();
        let db = brain_db_path(tmp.path());
        assert_eq!(
            self_change_receipt_stats_at(&db).unwrap(),
            ReceiptStats::default()
        );
        record_self_change_receipt_at(&db, "a", "narrative", None, QuoteVerdict::Grounded).unwrap();
        assert!(!db.exists(), "no-op write must not create the database");
    }

    #[test]
    fn legacy_db_without_table_reports_zero_receipts() {
        let tmp = tempfile::tempdir().unwrap();
        let db = brain_db_path(tmp.path());
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE memory_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);")
            .unwrap();
        drop(conn);
        assert_eq!(
            self_change_receipt_stats_at(&db).unwrap(),
            ReceiptStats::default()
        );
    }

    #[test]
    fn denial_counters_increment_total_and_per_agent() {
        let (_tmp, db) = mem_with_receipts();
        for _ in 0..5 {
            record_denial_at(&db, "legalplat").unwrap();
        }
        record_denial_at(&db, "other").unwrap();

        let stats = denial_stats_at(&db).unwrap();
        assert_eq!(stats.total, 6, "total копит отказы всех агентов");
        assert_eq!(
            stats.per_agent,
            vec![("legalplat".to_string(), 5), ("other".to_string(), 1)],
            "per-agent строки отсортированы по убыванию count"
        );

        // Перечитывание не дублирует метрику.
        let again = denial_stats_at(&db).unwrap();
        assert_eq!(again.total, 6);
        assert_eq!(again.per_agent.len(), 2);
    }

    #[test]
    fn missing_db_is_zero_denial_stats_and_noop_write() {
        let tmp = tempfile::tempdir().unwrap();
        let db = brain_db_path(tmp.path());
        assert_eq!(denial_stats_at(&db).unwrap(), DenialStats::default());
        record_denial_at(&db, "a").unwrap();
        assert!(!db.exists(), "no-op write must not create the database");
    }

    #[test]
    fn legacy_db_without_meta_table_reports_zero_denials_and_noops_write() {
        let tmp = tempfile::tempdir().unwrap();
        let db = brain_db_path(tmp.path());
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE self_change_receipts (id INTEGER PRIMARY KEY);")
            .unwrap();
        drop(conn);
        assert_eq!(denial_stats_at(&db).unwrap(), DenialStats::default());
        record_denial_at(&db, "a").unwrap();
    }
}
