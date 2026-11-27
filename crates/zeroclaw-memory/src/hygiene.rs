use crate::budget;
use crate::policy::PolicyEnforcer;
use crate::sqlite_permissions::{check_sqlite_storage, prepare_existing_sqlite_storage};
use anyhow::Result;
use chrono::{DateTime, Duration, Local, NaiveDate, Utc};
use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration as StdDuration, SystemTime};
use zeroclaw_config::schema::MemoryConfig;

const HYGIENE_INTERVAL_HOURS: i64 = 12;
const STATE_FILE: &str = "memory_hygiene_state.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct HygieneReport {
    archived_memory_files: u64,
    archived_session_files: u64,
    purged_memory_archives: u64,
    purged_session_archives: u64,
    pruned_conversation_rows: u64,
    pruned_daily_rows: u64,
    pruned_core_rows: u64,
    archived_core_rows: u64,
    archived_daily_rows: u64,
    skipped_referenced_conversation_rows: u64,
    // serde(default): state-файл, записанный бинарником до 9-8, не содержит
    // этих полей — без default чтение падало и каденс гигиены сбрасывался
    // (ревью-виток 1, BH).
    #[serde(default)]
    pruned_file_event_rows: u64,
    #[serde(default)]
    pruned_self_change_receipt_rows: u64,
    #[serde(default)]
    pruned_ratification_receipt_rows: u64,
}

impl HygieneReport {
    fn total_actions(&self) -> u64 {
        self.archived_memory_files
            + self.archived_session_files
            + self.purged_memory_archives
            + self.purged_session_archives
            + self.pruned_conversation_rows
            + self.pruned_daily_rows
            + self.pruned_core_rows
            + self.archived_core_rows
            + self.archived_daily_rows
            + self.pruned_file_event_rows
            + self.pruned_self_change_receipt_rows
            + self.pruned_ratification_receipt_rows
    }
}

/// Сколько строк журнальных таблиц подрезано одним прогоном (epic 9-8).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct JournalPruneReport {
    file_events: u64,
    self_change_receipts: u64,
    ratification_receipts: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct HygieneState {
    last_run_at: Option<String>,
    last_report: HygieneReport,
}

/// Наблюдаемость решений памяти (9-17): событие решения гигиены. Эмитятся
/// только вызывающим под `debug_decisions`, строго после персистента
/// действия; атрибуты — скаляры.
fn record_hygiene_decision(action: &str, category: &str, rows_affected: u64, reason: &str) {
    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(
            ::serde_json::json!({
                "error_key": "memory.hygiene_decision",
                "subsystem": "hygiene",
                "action": action,
                "category": category,
                "rows_affected": rows_affected,
                "reason": reason,
            })
        ),
        "memory hygiene decision"
    );
}

/// Run memory/session hygiene if the cadence window has elapsed.
/// This function is intentionally best-effort: callers should log and continue on failure.
pub fn run_if_due(config: &MemoryConfig, workspace_dir: &Path) -> Result<()> {
    if !config.hygiene_enabled {
        return Ok(());
    }

    if !should_run_now(workspace_dir)? {
        return Ok(());
    }

    // Use policy engine for per-category retention overrides.
    let enforcer = PolicyEnforcer::new(&config.policy);
    let conversation_retention = enforcer.retention_days_for_category(
        &crate::traits::MemoryCategory::Conversation,
        config.conversation_retention_days,
    );
    let daily_retention = enforcer.retention_days_for_category(
        &crate::traits::MemoryCategory::Daily,
        config.daily_retention_days,
    );
    let core_retention = enforcer.retention_days_for_category(
        &crate::traits::MemoryCategory::Core,
        config.core_retention_days,
    );

    let mut report = HygieneReport {
        archived_memory_files: archive_daily_memory_files(
            workspace_dir,
            config.archive_after_days,
        )?,
        archived_session_files: archive_session_files(workspace_dir, config.archive_after_days)?,
        purged_memory_archives: purge_memory_archives(workspace_dir, config.purge_after_days)?,
        purged_session_archives: purge_session_archives(workspace_dir, config.purge_after_days)?,
        ..HygieneReport::default()
    };

    // Row-level retention. `delete` mode (default) physically removes expired
    // rows exactly as before. `archive` mode fades expired core/daily rows via
    // the supersede sentinel (recall drops them, direct get by key still finds
    // them) and physically purges conversation rows only past
    // `physical_purge_days` and only when nothing references them.
    match config.retention_mode.as_str() {
        RETENTION_MODE_ARCHIVE | "delete" => {}
        other => anyhow::bail!("unknown memory.retention_mode: {other}"),
    }
    let archive_mode = config.retention_mode == RETENTION_MODE_ARCHIVE;
    if archive_mode {
        report.pruned_conversation_rows = purge_conversation_rows(
            workspace_dir,
            config.physical_purge_days,
            &mut report.skipped_referenced_conversation_rows,
        )?;
        report.archived_daily_rows =
            archive_category_rows(workspace_dir, daily_retention, "daily", false)?;
        report.archived_core_rows =
            archive_category_rows(workspace_dir, core_retention, "core", true)?;
    } else {
        report.pruned_conversation_rows =
            prune_category_rows(workspace_dir, conversation_retention, "conversation", false)?;
        report.pruned_daily_rows =
            prune_category_rows(workspace_dir, daily_retention, "daily", false)?;
        report.pruned_core_rows = prune_category_rows(workspace_dir, core_retention, "core", true)?;
    }
    // Бюджетная компактификация физически DELETE-ит строки — только в
    // delete-режиме: в archive-режиме долговременные строки не удаляются
    // никаким путём (ревью 9-2: budget overflow не обходит затухание).
    let mut budget_rows = BudgetRows::default();
    if !archive_mode {
        budget_rows = compact_budget_rows(workspace_dir, config)?;
        report.pruned_daily_rows += budget_rows.daily_rows;
        report.pruned_core_rows += budget_rows.core_rows;
    }

    // Наблюдаемость решений (9-17): одно событие на retention-ветку, ПОСЛЕ
    // персистента. action=skip — ветка отключена конфигом (retention 0 /
    // бюджеты не заданы), rows_affected=0. Бюджетная компактификация
    // (reason=cap) эмитится отдельно от retention-веток (reason=retention).
    if config.debug_decisions {
        if archive_mode {
            if config.physical_purge_days == 0 {
                record_hygiene_decision("skip", "conversation", 0, "retention");
            } else {
                record_hygiene_decision(
                    "prune",
                    "conversation",
                    report.pruned_conversation_rows,
                    "retention",
                );
            }
            if daily_retention == 0 {
                record_hygiene_decision("skip", "daily", 0, "retention");
            } else {
                record_hygiene_decision(
                    "archive",
                    "daily",
                    report.archived_daily_rows,
                    "retention",
                );
            }
            if core_retention == 0 {
                record_hygiene_decision("skip", "core", 0, "retention");
            } else {
                record_hygiene_decision("archive", "core", report.archived_core_rows, "retention");
            }
        } else {
            if conversation_retention == 0 {
                record_hygiene_decision("skip", "conversation", 0, "retention");
            } else {
                record_hygiene_decision(
                    "prune",
                    "conversation",
                    report.pruned_conversation_rows,
                    "retention",
                );
            }
            if daily_retention == 0 {
                record_hygiene_decision("skip", "daily", 0, "retention");
            } else {
                // pruned_*_rows уже включает строки бюджетной компактификации
                // (compact_budget_rows прибавлен в report выше) — вычитаем,
                // чтобы retention-событие не задваивало их.
                record_hygiene_decision(
                    "prune",
                    "daily",
                    report.pruned_daily_rows - budget_rows.daily_rows,
                    "retention",
                );
            }
            if core_retention == 0 {
                record_hygiene_decision("skip", "core", 0, "retention");
            } else {
                record_hygiene_decision(
                    "prune",
                    "core",
                    report.pruned_core_rows - budget_rows.core_rows,
                    "retention",
                );
            }
            let budgets_set =
                config.core_max_rows > 0 || config.core_max_bytes > 0 || config.daily_max_rows > 0;
            if !budgets_set {
                record_hygiene_decision("skip", "budget", 0, "cap");
            } else {
                record_hygiene_decision("compact", "daily", budget_rows.daily_rows, "cap");
                record_hygiene_decision("compact", "core", budget_rows.core_rows, "cap");
            }
        }
    }

    // Retention cleanup is independent from new audit collection: disabling
    // audit stops new rows, but existing sensitive rows still honor retention.
    if let Err(e) = prune_audit_entries(workspace_dir, config.audit_retention_days) {
        ::zeroclaw_log::record!(
            DEBUG,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
            "audit pruning skipped"
        );
    }

    // Журнальные таблицы brain.db (epic 9-8, BH-8/RAM-2): retention на
    // file_events + self_change_receipts и на ratification_receipts. Ошибка
    // уборки не роняет фазу (тот же паттерн, что prune_audit_entries).
    match prune_journal_tables(workspace_dir, config) {
        Ok(journal) => {
            report.pruned_file_event_rows = journal.file_events;
            report.pruned_self_change_receipt_rows = journal.self_change_receipts;
            report.pruned_ratification_receipt_rows = journal.ratification_receipts;
        }
        Err(e) => {
            // WARN, не DEBUG (ревью-виток 1, BH/спека «warn-видимость»):
            // стойкий фейл уборки иначе неотличим от «нечего подрезать».
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                "journal pruning skipped"
            );
        }
    }

    write_state(workspace_dir, &report)?;

    if report.total_actions() > 0 {
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
            &format!(
                "memory hygiene complete: archived_memory={} archived_sessions={} purged_memory={} purged_sessions={} pruned_conversation={} pruned_daily={} pruned_core={} archived_core={} archived_daily={} skipped_referenced_conversation={} pruned_file_events={} pruned_self_change_receipts={} pruned_ratification_receipts={}",
                report.archived_memory_files,
                report.archived_session_files,
                report.purged_memory_archives,
                report.purged_session_archives,
                report.pruned_conversation_rows,
                report.pruned_daily_rows,
                report.pruned_core_rows,
                report.archived_core_rows,
                report.archived_daily_rows,
                report.skipped_referenced_conversation_rows,
                report.pruned_file_event_rows,
                report.pruned_self_change_receipt_rows,
                report.pruned_ratification_receipt_rows,
            )
        );
    }

    Ok(())
}

fn should_run_now(workspace_dir: &Path) -> Result<bool> {
    let path = state_path(workspace_dir);
    if !path.exists() {
        return Ok(true);
    }

    let raw = fs::read_to_string(&path)?;
    let state: HygieneState = match serde_json::from_str(&raw) {
        Ok(s) => s,
        Err(_) => return Ok(true),
    };

    let Some(last_run_at) = state.last_run_at else {
        return Ok(true);
    };

    let last = match DateTime::parse_from_rfc3339(&last_run_at) {
        Ok(ts) => ts.with_timezone(&Utc),
        Err(_) => return Ok(true),
    };

    Ok(Utc::now().signed_duration_since(last) >= Duration::hours(HYGIENE_INTERVAL_HOURS))
}

fn write_state(workspace_dir: &Path, report: &HygieneReport) -> Result<()> {
    let path = state_path(workspace_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let state = HygieneState {
        last_run_at: Some(Utc::now().to_rfc3339()),
        last_report: report.clone(),
    };
    let json = serde_json::to_vec_pretty(&state)?;
    fs::write(path, json)?;
    Ok(())
}

fn state_path(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join("state").join(STATE_FILE)
}

fn archive_daily_memory_files(workspace_dir: &Path, archive_after_days: u32) -> Result<u64> {
    if archive_after_days == 0 {
        return Ok(0);
    }

    let memory_dir = workspace_dir.join("memory");
    if !memory_dir.is_dir() {
        return Ok(0);
    }

    let archive_dir = memory_dir.join("archive");
    fs::create_dir_all(&archive_dir)?;

    let cutoff = Local::now().date_naive() - Duration::days(i64::from(archive_after_days));
    let mut moved = 0_u64;

    for entry in fs::read_dir(&memory_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }

        let Some(filename) = path.file_name().and_then(|f| f.to_str()) else {
            continue;
        };

        let Some(file_date) = memory_date_from_filename(filename) else {
            continue;
        };

        if file_date < cutoff {
            move_to_archive(&path, &archive_dir)?;
            moved += 1;
        }
    }

    Ok(moved)
}

fn archive_session_files(workspace_dir: &Path, archive_after_days: u32) -> Result<u64> {
    if archive_after_days == 0 {
        return Ok(0);
    }

    let sessions_dir = workspace_dir.join("sessions");
    if !sessions_dir.is_dir() {
        return Ok(0);
    }

    let archive_dir = sessions_dir.join("archive");
    fs::create_dir_all(&archive_dir)?;

    let cutoff_date = Local::now().date_naive() - Duration::days(i64::from(archive_after_days));
    let cutoff_time = SystemTime::now()
        .checked_sub(StdDuration::from_secs(
            u64::from(archive_after_days) * 24 * 60 * 60,
        ))
        .unwrap_or(SystemTime::UNIX_EPOCH);

    let mut moved = 0_u64;
    for entry in fs::read_dir(&sessions_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            continue;
        }

        let Some(filename) = path.file_name().and_then(|f| f.to_str()) else {
            continue;
        };

        if !is_legacy_session_artifact(filename) {
            continue;
        }

        let is_old = if let Some(date) = date_prefix(filename) {
            date < cutoff_date
        } else {
            is_older_than(&path, cutoff_time)
        };

        if is_old {
            move_to_archive(&path, &archive_dir)?;
            moved += 1;
        }
    }

    Ok(moved)
}

fn is_legacy_session_artifact(filename: &str) -> bool {
    date_prefix(filename).is_some()
        || filename.ends_with(".jsonl")
        || filename.ends_with(".jsonl.migrated")
}

fn purge_memory_archives(workspace_dir: &Path, purge_after_days: u32) -> Result<u64> {
    if purge_after_days == 0 {
        return Ok(0);
    }

    let archive_dir = workspace_dir.join("memory").join("archive");
    if !archive_dir.is_dir() {
        return Ok(0);
    }

    let cutoff = Local::now().date_naive() - Duration::days(i64::from(purge_after_days));
    let mut removed = 0_u64;

    for entry in fs::read_dir(&archive_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            continue;
        }

        let Some(filename) = path.file_name().and_then(|f| f.to_str()) else {
            continue;
        };

        let Some(file_date) = memory_date_from_filename(filename) else {
            continue;
        };

        if file_date < cutoff {
            fs::remove_file(&path)?;
            removed += 1;
        }
    }

    Ok(removed)
}

fn purge_session_archives(workspace_dir: &Path, purge_after_days: u32) -> Result<u64> {
    if purge_after_days == 0 {
        return Ok(0);
    }

    let archive_dir = workspace_dir.join("sessions").join("archive");
    if !archive_dir.is_dir() {
        return Ok(0);
    }

    let cutoff_date = Local::now().date_naive() - Duration::days(i64::from(purge_after_days));
    let cutoff_time = SystemTime::now()
        .checked_sub(StdDuration::from_secs(
            u64::from(purge_after_days) * 24 * 60 * 60,
        ))
        .unwrap_or(SystemTime::UNIX_EPOCH);

    let mut removed = 0_u64;
    for entry in fs::read_dir(&archive_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            continue;
        }

        let Some(filename) = path.file_name().and_then(|f| f.to_str()) else {
            continue;
        };

        if !is_legacy_session_artifact(filename) {
            continue;
        }

        let is_old = if let Some(date) = date_prefix(filename) {
            date < cutoff_date
        } else {
            is_older_than(&path, cutoff_time)
        };

        if is_old {
            fs::remove_file(&path)?;
            removed += 1;
        }
    }

    Ok(removed)
}

fn prune_category_rows(
    workspace_dir: &Path,
    retention_days: u32,
    category: &str,
    use_created_at: bool,
) -> Result<u64> {
    if retention_days == 0 {
        return Ok(0);
    }

    let db_path = workspace_dir.join("memory").join("brain.db");
    if !db_path.exists() {
        return Ok(0);
    }

    let conn = Connection::open(db_path)?;
    // Use WAL so hygiene pruning doesn't block agent reads
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")?;
    let cutoff = (Local::now() - Duration::days(i64::from(retention_days))).to_rfc3339();

    let timestamp_col = if use_created_at {
        "created_at"
    } else {
        "updated_at"
    };
    let sql = format!(
        "DELETE FROM memories WHERE category = ?1 AND {timestamp_col} < ?2 AND pinned = 0",
    );
    let affected = conn.execute(&sql, params![category, cutoff])?;

    Ok(u64::try_from(affected).unwrap_or(0))
}

const RETENTION_MODE_ARCHIVE: &str = "archive";

/// Archive-mode replacement for `prune_category_rows` on long-term rows
/// (epic 9-2): instead of DELETE, expired non-pinned rows are faded via the
/// supersede sentinel. Recall already filters `superseded_by IS NULL`, so the
/// rows disappear from search while a direct get by key still finds them.
/// Rows already archived are left untouched so repeat runs do not recount.
fn archive_category_rows(
    workspace_dir: &Path,
    retention_days: u32,
    category: &str,
    use_created_at: bool,
) -> Result<u64> {
    if retention_days == 0 {
        return Ok(0);
    }

    let db_path = workspace_dir.join("memory").join("brain.db");
    if !db_path.exists() {
        return Ok(0);
    }

    let conn = Connection::open(db_path)?;
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")?;
    let cutoff = (Local::now() - Duration::days(i64::from(retention_days))).to_rfc3339();

    let timestamp_col = if use_created_at {
        "created_at"
    } else {
        "updated_at"
    };
    let sql = format!(
        "UPDATE memories SET superseded_by = ?3 \
         WHERE category = ?1 AND {timestamp_col} < ?2 AND pinned = 0 \
         AND superseded_by IS NULL",
    );
    let affected = conn.execute(&sql, params![category, cutoff, crate::ARCHIVED_SENTINEL])?;

    Ok(u64::try_from(affected).unwrap_or(0))
}

/// Archive-mode physical cleanup for conversation rows (epic 9-2): DELETE
/// only rows older than `purge_days`, and only when no `memory_refs` row
/// targets their key. Referenced rows are kept and counted into `skipped`.
fn purge_conversation_rows(
    workspace_dir: &Path,
    purge_days: u32,
    skipped: &mut u64,
) -> Result<u64> {
    *skipped = 0;
    if purge_days == 0 {
        return Ok(0);
    }

    let Some(db_path) = prepare_existing_sqlite_storage(workspace_dir, "brain.db")? else {
        return Ok(0);
    };

    let mut conn = Connection::open(db_path)?;
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")?;
    // memory_refs создаётся только в init_schema (SqliteMemory) — единый
    // источник DDL; без defensive CREATE, чтобы не плодить дрейф схемы.
    let cutoff = (Local::now() - Duration::days(i64::from(purge_days))).to_rfc3339();

    // COUNT и DELETE в одной транзакции: отчёт не расходится с фактом при
    // конкурентной записи memory_refs.
    let tx = conn.transaction()?;
    let referenced: i64 = tx.query_row(
        "SELECT COUNT(*) FROM memories          WHERE category = 'conversation' AND updated_at < ?1 AND pinned = 0          AND EXISTS (SELECT 1 FROM memory_refs WHERE target_key = memories.key)",
        params![cutoff],
        |row| row.get(0),
    )?;
    *skipped = u64::try_from(referenced).unwrap_or(0);

    let affected = tx.execute(
        "DELETE FROM memories          WHERE category = 'conversation' AND updated_at < ?1 AND pinned = 0          AND NOT EXISTS (SELECT 1 FROM memory_refs WHERE target_key = memories.key)",
        params![cutoff],
    )?;
    tx.commit()?;

    Ok(u64::try_from(affected).unwrap_or(0))
}

#[derive(Debug, Clone, Copy, Default)]
struct BudgetRows {
    core_rows: u64,
    daily_rows: u64,
}

fn compact_budget_rows(workspace_dir: &Path, config: &MemoryConfig) -> Result<BudgetRows> {
    if config.core_max_rows == 0 && config.core_max_bytes == 0 && config.daily_max_rows == 0 {
        return Ok(BudgetRows::default());
    }

    let db_path = workspace_dir.join("memory").join("brain.db");
    if !db_path.exists() {
        return Ok(BudgetRows::default());
    }

    let conn = Connection::open(db_path)?;
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")?;
    let core = budget::compact_category_to_budget(&conn, "core", config)?;
    let daily = budget::compact_category_to_budget(&conn, "daily", config)?;
    Ok(BudgetRows {
        core_rows: core.evicted_by_count + core.evicted_by_bytes,
        daily_rows: daily.evicted_by_count + daily.evicted_by_bytes,
    })
}

fn prune_audit_entries(workspace_dir: &Path, retention_days: u32) -> Result<()> {
    if retention_days == 0 {
        return Ok(());
    }

    let Some(db_path) = prepare_existing_sqlite_storage(workspace_dir, "audit.db")? else {
        return Ok(());
    };

    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE;
    #[cfg(unix)]
    let flags = flags | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let conn = Connection::open_with_flags(&db_path, flags)?;
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")?;
    check_sqlite_storage(&db_path)?;
    let cutoff = (Local::now() - Duration::days(i64::from(retention_days))).to_rfc3339();

    let affected = conn.execute(
        "DELETE FROM memory_audit WHERE timestamp < ?1",
        params![cutoff],
    )?;

    if affected > 0 {
        ::zeroclaw_log::record!(
            DEBUG,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(
                ::serde_json::json!({"affected": affected, "retention_days": retention_days})
            ),
            "pruned  audit entries older than  days"
        );
    }

    Ok(())
}

/// Журнальные таблицы brain.db (epic 9-8, BH-8/RAM-2): `file_events` и
/// `self_change_receipts` подпираются `journal_retention_days`, append-only
/// `ratification_receipts` — своим ключом. DELETE по `created_at < cutoff`;
/// 0 = keep forever (skip — байт-в-байт до включения). Таблицы нет (БД
/// старше миграции) — no-op, нули (паттерн receipts.rs `table_exists`).
/// Имён таблиц извне нет: только фиксированный список констант модуля.
fn prune_journal_tables(workspace_dir: &Path, config: &MemoryConfig) -> Result<JournalPruneReport> {
    let mut report = JournalPruneReport::default();
    if config.journal_retention_days == 0 && config.ratification_receipts_retention_days == 0 {
        return Ok(report);
    }

    let Some(db_path) = prepare_existing_sqlite_storage(workspace_dir, "brain.db")? else {
        return Ok(report);
    };

    let conn = Connection::open(db_path)?;
    // БД занята агентским ходом — busy_timeout-паттерн receipts.rs вместо
    // SQLITE_BUSY на первой же строке.
    conn.busy_timeout(crate::receipts::RECEIPT_BUSY_TIMEOUT)?;
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")?;

    // Одна транзакция на все три таблицы (ревью-виток 1, ECH): частичная
    // уборка с нулевым отчётом хуже отката фазы целиком.
    let tx = conn.unchecked_transaction()?;
    report.file_events = prune_journal_table(
        &tx,
        crate::proposals::FILE_EVENTS_TABLE,
        config.journal_retention_days,
    )?;
    report.self_change_receipts = prune_journal_table(
        &tx,
        crate::receipts::SELF_CHANGE_RECEIPTS_TABLE,
        config.journal_retention_days,
    )?;
    report.ratification_receipts = prune_journal_table(
        &tx,
        crate::proposals::RATIFICATION_RECEIPTS_TABLE,
        config.ratification_receipts_retention_days,
    )?;
    tx.commit()?;

    if report.file_events > 0 || report.self_change_receipts > 0 || report.ratification_receipts > 0
    {
        ::zeroclaw_log::record!(
            DEBUG,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(
                ::serde_json::json!({
                    "file_events": report.file_events,
                    "self_change_receipts": report.self_change_receipts,
                    "ratification_receipts": report.ratification_receipts,
                })
            ),
            "pruned journal rows older than retention"
        );
    }

    Ok(report)
}

/// DELETE из одной журнальной таблицы по возрасту; retention 0 или таблицы
/// нет — 0.
fn prune_journal_table(conn: &Connection, table: &str, retention_days: u32) -> Result<u64> {
    if retention_days == 0 {
        return Ok(0);
    }
    let exists: bool = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        params![table],
        |row| {
            let n: i64 = row.get(0)?;
            Ok(n > 0)
        },
    )?;
    if !exists {
        return Ok(0);
    }
    // u32::MAX days overflows the chrono range → panic inside the hygiene
    // phase (ревью-виток 1, ECH). Out-of-range retention = skip with WARN.
    let Some(cutoff_dt) =
        Local::now().checked_sub_signed(Duration::days(i64::from(retention_days)))
    else {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_attrs(::serde_json::json!({"retention_days": retention_days})),
            "journal retention out of range, skipping prune"
        );
        return Ok(0);
    };
    let cutoff = cutoff_dt.to_rfc3339();
    let affected = conn.execute(
        &format!("DELETE FROM {table} WHERE created_at < ?1"),
        params![cutoff],
    )?;
    Ok(u64::try_from(affected).unwrap_or(0))
}

fn memory_date_from_filename(filename: &str) -> Option<NaiveDate> {
    let stem = filename.strip_suffix(".md")?;
    // Split on '_' first (handles YYYY-MM-DD_suffix.md).
    let date_part = stem.split('_').next().unwrap_or(stem);
    // If the date part has more than 10 chars (e.g. 2026-03-28-1442),
    // take only the YYYY-MM-DD prefix.
    let date_str = if date_part.len() > 10 {
        date_part.get(..10)?
    } else {
        date_part
    };
    NaiveDate::parse_from_str(date_str, "%Y-%m-%d").ok()
}

fn date_prefix(filename: &str) -> Option<NaiveDate> {
    if filename.len() < 10 {
        return None;
    }
    let boundary = {
        let mut i = 10.min(filename.len());
        while i > 0 && !filename.is_char_boundary(i) {
            i -= 1;
        }
        i
    };
    NaiveDate::parse_from_str(&filename[..boundary], "%Y-%m-%d").ok()
}

fn is_older_than(path: &Path, cutoff: SystemTime) -> bool {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .map(|modified| modified < cutoff)
        .unwrap_or(false)
}

fn move_to_archive(src: &Path, archive_dir: &Path) -> Result<()> {
    let Some(filename) = src.file_name().and_then(|f| f.to_str()) else {
        return Ok(());
    };

    let target = unique_archive_target(archive_dir, filename);
    fs::rename(src, target)?;
    Ok(())
}

fn unique_archive_target(archive_dir: &Path, filename: &str) -> PathBuf {
    let direct = archive_dir.join(filename);
    if !direct.exists() {
        return direct;
    }

    let (stem, ext) = split_name(filename);
    for i in 1..10_000 {
        let candidate = if ext.is_empty() {
            archive_dir.join(format!("{stem}_{i}"))
        } else {
            archive_dir.join(format!("{stem}_{i}.{ext}"))
        };
        if !candidate.exists() {
            return candidate;
        }
    }

    direct
}

fn split_name(filename: &str) -> (&str, &str) {
    match filename.rsplit_once('.') {
        Some((stem, ext)) => (stem, ext),
        None => (filename, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::SqliteMemory;
    use crate::traits::{Memory, MemoryCategory, StoreOptions};
    use filetime::{FileTime, set_file_mtime};
    use tempfile::TempDir;

    fn default_cfg() -> MemoryConfig {
        MemoryConfig::default()
    }

    fn set_old_mtime(path: &Path, days_old: i64) {
        let old = FileTime::from_system_time(
            (SystemTime::now() - StdDuration::from_secs(days_old as u64 * 24 * 60 * 60))
                .max(SystemTime::UNIX_EPOCH),
        );
        set_file_mtime(path, old).unwrap();
    }

    #[test]
    fn archives_old_daily_memory_files() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path();
        fs::create_dir_all(workspace.join("memory")).unwrap();

        let old = (Local::now().date_naive() - Duration::days(10))
            .format("%Y-%m-%d")
            .to_string();
        let today = Local::now().date_naive().format("%Y-%m-%d").to_string();

        let old_file = workspace.join("memory").join(format!("{old}.md"));
        let today_file = workspace.join("memory").join(format!("{today}.md"));
        fs::write(&old_file, "old note").unwrap();
        fs::write(&today_file, "fresh note").unwrap();

        run_if_due(&default_cfg(), workspace).unwrap();

        assert!(!old_file.exists(), "old daily file should be archived");
        assert!(
            workspace
                .join("memory")
                .join("archive")
                .join(format!("{old}.md"))
                .exists(),
            "old daily file should exist in memory/archive"
        );
        assert!(today_file.exists(), "today file should remain in place");
    }

    #[test]
    fn archives_old_session_files() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path();
        fs::create_dir_all(workspace.join("sessions")).unwrap();

        let old = (Local::now().date_naive() - Duration::days(10))
            .format("%Y-%m-%d")
            .to_string();
        let old_name = format!("{old}-agent.log");
        let old_file = workspace.join("sessions").join(&old_name);
        fs::write(&old_file, "old session").unwrap();

        run_if_due(&default_cfg(), workspace).unwrap();

        assert!(!old_file.exists(), "old session file should be archived");
        assert!(
            workspace
                .join("sessions")
                .join("archive")
                .join(&old_name)
                .exists(),
            "archived session file should exist"
        );
    }

    #[test]
    fn keeps_sqlite_session_artifacts_out_of_archives() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path();
        let sessions_dir = workspace.join("sessions");
        fs::create_dir_all(&sessions_dir).unwrap();

        let protected = ["sessions.db", "sessions.db-wal", "sessions.db-shm"];
        for filename in protected {
            let path = sessions_dir.join(filename);
            fs::write(&path, "sqlite artifact").unwrap();
            set_old_mtime(&path, 10);
        }

        run_if_due(&default_cfg(), workspace).unwrap();

        for filename in protected {
            assert!(
                sessions_dir.join(filename).exists(),
                "{filename} should remain in the hot sessions directory"
            );
            assert!(
                !sessions_dir.join("archive").join(filename).exists(),
                "{filename} must not be moved into the session archive"
            );
        }
    }

    #[test]
    fn archives_old_legacy_jsonl_session_files() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path();
        let sessions_dir = workspace.join("sessions");
        fs::create_dir_all(&sessions_dir).unwrap();

        let legacy_file = sessions_dir.join("legacy_session.jsonl");
        fs::write(&legacy_file, "legacy session").unwrap();
        set_old_mtime(&legacy_file, 10);

        run_if_due(&default_cfg(), workspace).unwrap();

        assert!(
            !legacy_file.exists(),
            "old legacy JSONL session file should be archived"
        );
        assert!(
            sessions_dir
                .join("archive")
                .join("legacy_session.jsonl")
                .exists(),
            "archived legacy JSONL session file should exist"
        );
    }

    #[test]
    fn purges_old_legacy_session_archives_but_keeps_sqlite_artifacts() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path();
        let archive_dir = workspace.join("sessions").join("archive");
        fs::create_dir_all(&archive_dir).unwrap();

        let protected = ["sessions.db", "sessions.db-wal", "sessions.db-shm"];
        for filename in protected {
            let path = archive_dir.join(filename);
            fs::write(&path, "sqlite artifact").unwrap();
            set_old_mtime(&path, 40);
        }

        let legacy_file = archive_dir.join("legacy_session.jsonl");
        fs::write(&legacy_file, "legacy session").unwrap();
        set_old_mtime(&legacy_file, 40);

        run_if_due(&default_cfg(), workspace).unwrap();

        assert!(
            !legacy_file.exists(),
            "old archived legacy session file should be purged"
        );
        for filename in protected {
            assert!(
                archive_dir.join(filename).exists(),
                "{filename} should remain in the session archive"
            );
        }
    }

    #[test]
    fn skips_second_run_within_cadence_window() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path();
        fs::create_dir_all(workspace.join("memory")).unwrap();

        let old_a = (Local::now().date_naive() - Duration::days(10))
            .format("%Y-%m-%d")
            .to_string();
        let file_a = workspace.join("memory").join(format!("{old_a}.md"));
        fs::write(&file_a, "first").unwrap();

        run_if_due(&default_cfg(), workspace).unwrap();
        assert!(!file_a.exists(), "first old file should be archived");

        let old_b = (Local::now().date_naive() - Duration::days(9))
            .format("%Y-%m-%d")
            .to_string();
        let file_b = workspace.join("memory").join(format!("{old_b}.md"));
        fs::write(&file_b, "second").unwrap();

        // Should skip because cadence gate prevents a second immediate run.
        run_if_due(&default_cfg(), workspace).unwrap();
        assert!(
            file_b.exists(),
            "second file should remain because run is throttled"
        );
    }

    #[test]
    fn purges_old_memory_archives() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path();
        let archive_dir = workspace.join("memory").join("archive");
        fs::create_dir_all(&archive_dir).unwrap();

        let old = (Local::now().date_naive() - Duration::days(40))
            .format("%Y-%m-%d")
            .to_string();
        let keep = (Local::now().date_naive() - Duration::days(5))
            .format("%Y-%m-%d")
            .to_string();

        let old_file = archive_dir.join(format!("{old}.md"));
        let keep_file = archive_dir.join(format!("{keep}.md"));
        fs::write(&old_file, "expired").unwrap();
        fs::write(&keep_file, "recent").unwrap();

        run_if_due(&default_cfg(), workspace).unwrap();

        assert!(!old_file.exists(), "old archived file should be purged");
        assert!(keep_file.exists(), "recent archived file should remain");
    }

    #[tokio::test]
    async fn prunes_old_conversation_rows_in_sqlite_backend() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path();

        let mem = SqliteMemory::new("sqlite", workspace).unwrap();
        mem.store("conv_old", "outdated", MemoryCategory::Conversation, None)
            .await
            .unwrap();
        mem.store("core_keep", "durable", MemoryCategory::Core, None)
            .await
            .unwrap();
        drop(mem);

        let db_path = workspace.join("memory").join("brain.db");
        let conn = Connection::open(&db_path).unwrap();
        let old_cutoff = (Local::now() - Duration::days(60)).to_rfc3339();
        conn.execute(
            "UPDATE memories SET created_at = ?1, updated_at = ?1 WHERE key = 'conv_old'",
            params![old_cutoff],
        )
        .unwrap();
        drop(conn);

        let mut cfg = default_cfg();
        cfg.archive_after_days = 0;
        cfg.purge_after_days = 0;
        cfg.conversation_retention_days = 30;

        run_if_due(&cfg, workspace).unwrap();

        let mem2 = SqliteMemory::new("sqlite", workspace).unwrap();
        assert!(
            mem2.get("conv_old").await.unwrap().is_none(),
            "old conversation rows should be pruned"
        );
        assert!(
            mem2.get("core_keep").await.unwrap().is_some(),
            "core memory should remain"
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn hygiene_decision_events_flag_gated() {
        let (_obs_guard, _writer_guard, _hook_guard, mut rx) = crate::log_capture::install();
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path();

        let mem = SqliteMemory::new("sqlite", workspace).unwrap();
        mem.store("daily_old", "stale", MemoryCategory::Daily, None)
            .await
            .unwrap();
        drop(mem);
        let conn = Connection::open(workspace.join("memory").join("brain.db")).unwrap();
        let old_cutoff = (Local::now() - Duration::days(60)).to_rfc3339();
        conn.execute(
            "UPDATE memories SET created_at = ?1, updated_at = ?1 WHERE key = 'daily_old'",
            params![old_cutoff],
        )
        .unwrap();
        drop(conn);

        // Флаг выкл: прогон полностью молчит на слое memory.* (retention не
        // трогаем — строка нужна следующему прогону).
        let mut cfg = default_cfg();
        cfg.archive_after_days = 0;
        cfg.purge_after_days = 0;
        run_if_due(&cfg, workspace).unwrap();
        let events = crate::log_capture::collect(&mut rx, "memory.hygiene_decision");
        assert!(
            events.is_empty(),
            "debug_decisions off: zero hygiene_decision events, got {events:?}"
        );

        // Сброс cadence-стейта, чтобы второй прогон реально выполнился.
        let _ = fs::remove_file(state_path(workspace));

        // Флаг вкл (archive-режим): события по каждой retention-ветке —
        // archive на daily (строка состарена), skip на conversation/core
        // (ветки отключены конфигом: physical_purge_days=0 / core=0).
        let mut cfg = default_cfg();
        cfg.archive_after_days = 0;
        cfg.purge_after_days = 0;
        cfg.retention_mode = "archive".into();
        cfg.physical_purge_days = 0;
        cfg.daily_retention_days = 30;
        cfg.core_retention_days = 0;
        cfg.debug_decisions = true;
        run_if_due(&cfg, workspace).unwrap();

        let events = crate::log_capture::collect(&mut rx, "memory.hygiene_decision");
        let action_of = |category: &str| {
            events
                .iter()
                .find(|e| e["attributes"]["category"] == category)
                .map(|e| e["attributes"]["action"].as_str().unwrap().to_string())
        };
        assert_eq!(action_of("daily").as_deref(), Some("archive"));
        assert_eq!(action_of("conversation").as_deref(), Some("skip"));
        assert_eq!(action_of("core").as_deref(), Some("skip"));
        let daily = events
            .iter()
            .find(|e| e["attributes"]["category"] == "daily")
            .unwrap();
        assert_eq!(daily["attributes"]["rows_affected"], 1);
        assert_eq!(daily["attributes"]["reason"], "retention");
        crate::log_capture::assert_scalar_only(&events);
    }

    #[tokio::test]
    async fn prunes_old_daily_rows_in_sqlite_backend() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path();

        let mem = SqliteMemory::new("sqlite", workspace).unwrap();
        mem.store("daily_old", "stale", MemoryCategory::Daily, None)
            .await
            .unwrap();
        mem.store("daily_recent", "fresh", MemoryCategory::Daily, None)
            .await
            .unwrap();
        drop(mem);

        let db_path = workspace.join("memory").join("brain.db");
        let conn = Connection::open(&db_path).unwrap();
        let old_cutoff = (Local::now() - Duration::days(60)).to_rfc3339();
        conn.execute(
            "UPDATE memories SET created_at = ?1, updated_at = ?1 WHERE key = 'daily_old'",
            params![old_cutoff],
        )
        .unwrap();
        drop(conn);

        let mut cfg = default_cfg();
        cfg.archive_after_days = 0;
        cfg.purge_after_days = 0;
        cfg.daily_retention_days = 30;

        run_if_due(&cfg, workspace).unwrap();

        let mem2 = SqliteMemory::new("sqlite", workspace).unwrap();
        assert!(
            mem2.get("daily_old").await.unwrap().is_none(),
            "old daily rows should be pruned"
        );
        assert!(
            mem2.get("daily_recent").await.unwrap().is_some(),
            "recent daily rows should remain"
        );
    }

    #[tokio::test]
    async fn zero_retention_disables_daily_pruning() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path();

        let mem = SqliteMemory::new("sqlite", workspace).unwrap();
        mem.store("daily_old", "stale", MemoryCategory::Daily, None)
            .await
            .unwrap();
        drop(mem);

        let db_path = workspace.join("memory").join("brain.db");
        let conn = Connection::open(&db_path).unwrap();
        let old_cutoff = (Local::now() - Duration::days(60)).to_rfc3339();
        conn.execute(
            "UPDATE memories SET created_at = ?1, updated_at = ?1 WHERE key = 'daily_old'",
            params![old_cutoff],
        )
        .unwrap();
        drop(conn);

        let mut cfg = default_cfg();
        cfg.daily_retention_days = 0; // default: keep forever

        run_if_due(&cfg, workspace).unwrap();

        let mem2 = SqliteMemory::new("sqlite", workspace).unwrap();
        assert!(
            mem2.get("daily_old").await.unwrap().is_some(),
            "daily rows should be kept when retention_days = 0"
        );
    }

    #[tokio::test]
    async fn prunes_old_core_rows_when_configured() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path();

        let mem = SqliteMemory::new("sqlite", workspace).unwrap();
        mem.store("core_old", "obsolete", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store(
            "core_updated",
            "touched recently",
            MemoryCategory::Core,
            None,
        )
        .await
        .unwrap();
        drop(mem);

        let db_path = workspace.join("memory").join("brain.db");
        let conn = Connection::open(&db_path).unwrap();
        let old_cutoff = (Local::now() - Duration::days(120)).to_rfc3339();
        let recent = (Local::now() - Duration::days(1)).to_rfc3339();
        // core_old: both timestamps are old → should be pruned
        conn.execute(
            "UPDATE memories SET created_at = ?1, updated_at = ?1 WHERE key = 'core_old'",
            params![old_cutoff],
        )
        .unwrap();
        // core_updated: created_at is old but updated_at is recent → still pruned
        // because core pruning keys on created_at, not updated_at
        conn.execute(
            "UPDATE memories SET created_at = ?1, updated_at = ?2 WHERE key = 'core_updated'",
            params![old_cutoff, recent],
        )
        .unwrap();
        drop(conn);

        let mut cfg = default_cfg();
        cfg.archive_after_days = 0;
        cfg.purge_after_days = 0;
        cfg.core_retention_days = 90;

        run_if_due(&cfg, workspace).unwrap();

        let mem2 = SqliteMemory::new("sqlite", workspace).unwrap();
        assert!(
            mem2.get("core_old").await.unwrap().is_none(),
            "old core rows should be pruned when retention is configured"
        );
        assert!(
            mem2.get("core_updated").await.unwrap().is_none(),
            "core rows with old created_at should be pruned even if updated_at is recent"
        );
    }

    fn superseded_by_of(workspace: &Path, key: &str) -> Option<String> {
        let conn = Connection::open(workspace.join("memory").join("brain.db")).unwrap();
        conn.query_row(
            "SELECT superseded_by FROM memories WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn archive_cfg() -> MemoryConfig {
        let mut cfg = default_cfg();
        cfg.archive_after_days = 0;
        cfg.purge_after_days = 0;
        cfg.retention_mode = "archive".into();
        cfg
    }

    #[test]
    fn default_retention_mode_is_delete_with_180_day_physical_purge() {
        let cfg = default_cfg();
        assert_eq!(cfg.retention_mode, "delete");
        assert_eq!(cfg.physical_purge_days, 180);
    }

    #[tokio::test]
    async fn archive_mode_fades_expired_core_rows_instead_of_deleting() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path();

        let mem = SqliteMemory::new("sqlite", workspace).unwrap();
        mem.store(
            "core_old",
            "obsolete durable fact",
            MemoryCategory::Core,
            None,
        )
        .await
        .unwrap();
        drop(mem);

        let conn = Connection::open(workspace.join("memory").join("brain.db")).unwrap();
        let old_cutoff = (Local::now() - Duration::days(120)).to_rfc3339();
        conn.execute(
            "UPDATE memories SET created_at = ?1, updated_at = ?1 WHERE key = 'core_old'",
            params![old_cutoff],
        )
        .unwrap();
        drop(conn);

        let mut cfg = archive_cfg();
        cfg.core_retention_days = 30;
        run_if_due(&cfg, workspace).unwrap();

        let mem2 = SqliteMemory::new("sqlite", workspace).unwrap();
        let entry = mem2
            .get("core_old")
            .await
            .unwrap()
            .expect("archived row must still be reachable by direct get");
        assert_eq!(
            entry.superseded_by.as_deref(),
            Some(crate::ARCHIVED_SENTINEL),
            "expired core row must be faded via the archive sentinel"
        );
        let recalled = mem2
            .recall("obsolete durable fact", 10, None, None, None)
            .await
            .unwrap();
        assert!(
            recalled.iter().all(|e| e.key != "core_old"),
            "archived row must not surface in recall"
        );
    }

    #[tokio::test]
    async fn archive_mode_does_not_touch_pinned_expired_rows() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path();

        let mem = SqliteMemory::new("sqlite", workspace).unwrap();
        mem.store_with_options(
            "core_pinned",
            "pinned durable fact",
            MemoryCategory::Core,
            None,
            StoreOptions {
                pinned: true,
                ..StoreOptions::default()
            },
        )
        .await
        .unwrap();
        drop(mem);

        let conn = Connection::open(workspace.join("memory").join("brain.db")).unwrap();
        let old_cutoff = (Local::now() - Duration::days(120)).to_rfc3339();
        conn.execute(
            "UPDATE memories SET created_at = ?1, updated_at = ?1 WHERE key = 'core_pinned'",
            params![old_cutoff],
        )
        .unwrap();
        drop(conn);

        let mut cfg = archive_cfg();
        cfg.core_retention_days = 30;
        run_if_due(&cfg, workspace).unwrap();

        assert_eq!(
            superseded_by_of(workspace, "core_pinned"),
            None,
            "pinned expired row must not be archived"
        );
    }

    #[tokio::test]
    async fn archive_mode_fades_expired_daily_rows() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path();

        let mem = SqliteMemory::new("sqlite", workspace).unwrap();
        mem.store("daily_old", "stale daily note", MemoryCategory::Daily, None)
            .await
            .unwrap();
        drop(mem);

        let conn = Connection::open(workspace.join("memory").join("brain.db")).unwrap();
        let old_cutoff = (Local::now() - Duration::days(60)).to_rfc3339();
        conn.execute(
            "UPDATE memories SET created_at = ?1, updated_at = ?1 WHERE key = 'daily_old'",
            params![old_cutoff],
        )
        .unwrap();
        drop(conn);

        let mut cfg = archive_cfg();
        cfg.daily_retention_days = 30;
        run_if_due(&cfg, workspace).unwrap();

        assert_eq!(
            superseded_by_of(workspace, "daily_old"),
            Some(crate::ARCHIVED_SENTINEL.to_string()),
            "expired daily row must be faded via the archive sentinel"
        );
    }

    #[tokio::test]
    async fn archive_mode_purges_unreferenced_conversation_rows() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path();

        let mem = SqliteMemory::new("sqlite", workspace).unwrap();
        mem.store(
            "conv_old",
            "outdated chatter",
            MemoryCategory::Conversation,
            None,
        )
        .await
        .unwrap();
        drop(mem);

        let conn = Connection::open(workspace.join("memory").join("brain.db")).unwrap();
        let old_cutoff = (Local::now() - Duration::days(200)).to_rfc3339();
        conn.execute(
            "UPDATE memories SET created_at = ?1, updated_at = ?1 WHERE key = 'conv_old'",
            params![old_cutoff],
        )
        .unwrap();
        drop(conn);

        let mut cfg = archive_cfg();
        cfg.physical_purge_days = 180;
        run_if_due(&cfg, workspace).unwrap();

        let mem2 = SqliteMemory::new("sqlite", workspace).unwrap();
        assert!(
            mem2.get("conv_old").await.unwrap().is_none(),
            "unreferenced conversation row past physical_purge_days must be deleted"
        );
    }

    #[tokio::test]
    async fn archive_mode_skips_referenced_conversation_rows_and_reports_them() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path();

        let mem = SqliteMemory::new("sqlite", workspace).unwrap();
        mem.store(
            "conv_old",
            "outdated chatter",
            MemoryCategory::Conversation,
            None,
        )
        .await
        .unwrap();
        drop(mem);

        let conn = Connection::open(workspace.join("memory").join("brain.db")).unwrap();
        let old_cutoff = (Local::now() - Duration::days(200)).to_rfc3339();
        conn.execute(
            "UPDATE memories SET created_at = ?1, updated_at = ?1 WHERE key = 'conv_old'",
            params![old_cutoff],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO memory_refs (source_key, target_key, created_at) \
             VALUES ('proposal_1', 'conv_old', ?1)",
            params![Local::now().to_rfc3339()],
        )
        .unwrap();
        drop(conn);

        let mut cfg = archive_cfg();
        cfg.physical_purge_days = 180;
        run_if_due(&cfg, workspace).unwrap();

        let mem2 = SqliteMemory::new("sqlite", workspace).unwrap();
        assert!(
            mem2.get("conv_old").await.unwrap().is_some(),
            "referenced conversation row must survive the physical purge"
        );

        let state: HygieneState = serde_json::from_str(
            &fs::read_to_string(workspace.join("state").join(STATE_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            state.last_report.skipped_referenced_conversation_rows, 1,
            "the skipped reference must be counted in the hygiene report"
        );
        assert_eq!(
            state.last_report.pruned_conversation_rows, 0,
            "no conversation row may be purged while it is referenced"
        );
    }

    #[tokio::test]
    async fn zero_retention_still_disables_archive_fading() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path();

        let mem = SqliteMemory::new("sqlite", workspace).unwrap();
        mem.store(
            "core_old",
            "obsolete durable fact",
            MemoryCategory::Core,
            None,
        )
        .await
        .unwrap();
        drop(mem);

        let conn = Connection::open(workspace.join("memory").join("brain.db")).unwrap();
        let old_cutoff = (Local::now() - Duration::days(120)).to_rfc3339();
        conn.execute(
            "UPDATE memories SET created_at = ?1, updated_at = ?1 WHERE key = 'core_old'",
            params![old_cutoff],
        )
        .unwrap();
        drop(conn);

        // Archive mode with retention 0 = keep forever, exactly like delete mode.
        let cfg = archive_cfg();
        assert_eq!(cfg.core_retention_days, 0);
        run_if_due(&cfg, workspace).unwrap();

        assert_eq!(
            superseded_by_of(workspace, "core_old"),
            None,
            "retention 0 must keep rows live even in archive mode"
        );
    }

    #[tokio::test]
    async fn run_if_due_enforces_the_core_row_budget() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path();

        let mem = SqliteMemory::new("sqlite", workspace).unwrap();
        mem.store_with_options(
            "core_low",
            "low value fact",
            MemoryCategory::Core,
            None,
            StoreOptions::default().with_importance(0.1),
        )
        .await
        .unwrap();
        mem.store_with_options(
            "core_high",
            "high value fact",
            MemoryCategory::Core,
            None,
            StoreOptions::default().with_importance(0.9),
        )
        .await
        .unwrap();
        mem.store_with_options(
            "core_pinned",
            "pinned but low value",
            MemoryCategory::Core,
            None,
            StoreOptions {
                importance: Some(0.05),
                pinned: true,
                ..StoreOptions::default()
            },
        )
        .await
        .unwrap();
        drop(mem);

        let mut cfg = default_cfg();
        cfg.archive_after_days = 0;
        cfg.purge_after_days = 0;
        cfg.core_max_rows = 2;

        run_if_due(&cfg, workspace).unwrap();

        let mem2 = SqliteMemory::new("sqlite", workspace).unwrap();
        assert!(
            mem2.get("core_low").await.unwrap().is_none(),
            "lowest-value non-pinned row is evicted to meet the budget"
        );
        assert!(
            mem2.get("core_high").await.unwrap().is_some(),
            "higher-value row survives compaction"
        );
        assert!(
            mem2.get("core_pinned").await.unwrap().is_some(),
            "pinned row survives despite the lowest importance"
        );
    }

    #[test]
    fn date_from_filename_handles_hyphen_suffix() {
        let d = memory_date_from_filename("2026-03-28-1442.md");
        assert!(d.is_some(), "YYYY-MM-DD-HHMM.md should be parsed");
        assert_eq!(d.unwrap(), NaiveDate::from_ymd_opt(2026, 3, 28).unwrap());
    }

    #[test]
    fn date_from_filename_handles_underscore_suffix() {
        let d = memory_date_from_filename("2026-03-28_session_notes.md");
        assert!(d.is_some(), "YYYY-MM-DD_suffix.md should be parsed");
        assert_eq!(d.unwrap(), NaiveDate::from_ymd_opt(2026, 3, 28).unwrap());
    }

    #[test]
    fn date_from_filename_plain_date() {
        let d = memory_date_from_filename("2026-03-28.md");
        assert!(d.is_some(), "YYYY-MM-DD.md should be parsed");
        assert_eq!(d.unwrap(), NaiveDate::from_ymd_opt(2026, 3, 28).unwrap());
    }

    fn seed_audit_db(workspace: &Path) -> PathBuf {
        let db_path =
            crate::sqlite_permissions::prepare_sqlite_storage(workspace, "audit.db").unwrap();
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE;
        #[cfg(unix)]
        let flags = flags | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        let conn = Connection::open_with_flags(&db_path, flags).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS memory_audit (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 operation TEXT NOT NULL,
                 key TEXT,
                 namespace TEXT,
                 session_id TEXT,
                 timestamp TEXT NOT NULL,
                 metadata TEXT
             );",
        )
        .unwrap();
        let stale = (Local::now() - Duration::days(45)).to_rfc3339();
        let fresh = Local::now().to_rfc3339();
        conn.execute(
            "INSERT INTO memory_audit (operation, timestamp) VALUES ('store', ?1)",
            params![stale],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO memory_audit (operation, timestamp) VALUES ('store', ?1)",
            params![fresh],
        )
        .unwrap();
        db_path
    }

    fn audit_row_count(db_path: &Path) -> i64 {
        let conn = Connection::open(db_path).unwrap();
        conn.query_row("SELECT COUNT(*) FROM memory_audit", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn prunes_stale_audit_rows_when_audit_enabled() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path();
        let db_path = seed_audit_db(workspace);

        let mut cfg = default_cfg();
        cfg.audit_enabled = true;
        assert_eq!(cfg.audit_retention_days, 30);
        run_if_due(&cfg, workspace).unwrap();

        assert_eq!(
            audit_row_count(&db_path),
            1,
            "audit rows past audit_retention_days must be pruned"
        );
    }

    // ── Journal table retention (epic 9-8) ────────────────────────────

    /// State-файл, записанный бинарником ДО 9-8, не содержит новых полей
    /// отчёта — чтение обязано пройти с нулями, а не сбрасывать каденс
    /// гигиены (ревью-виток 1, BH; фикс — #[serde(default)] на полях).
    #[test]
    fn hygiene_report_tolerates_pre_feature_state_file() {
        let json = r#"{
            "last_run_at": "2026-11-01T00:00:00+03:00",
            "last_report": {
                "archived_memory_files": 1,
                "archived_session_files": 0,
                "purged_memory_archives": 0,
                "purged_session_archives": 0,
                "pruned_conversation_rows": 0,
                "pruned_daily_rows": 0,
                "pruned_core_rows": 0,
                "archived_core_rows": 0,
                "archived_daily_rows": 0,
                "skipped_referenced_conversation_rows": 0
            }
        }"#;
        let state: HygieneState = serde_json::from_str(json).unwrap();
        assert_eq!(state.last_report.pruned_file_event_rows, 0);
        assert_eq!(state.last_report.pruned_self_change_receipt_rows, 0);
        assert_eq!(state.last_report.pruned_ratification_receipt_rows, 0);
        assert_eq!(state.last_report.archived_memory_files, 1);
    }

    /// Посеять в brain.db по две строки (старая + свежая) в каждый из
    /// трёх журнальных таблиц. Возвращает путь к БД.
    fn seed_journal_rows(workspace: &Path) -> PathBuf {
        let mem = SqliteMemory::new("sqlite", workspace).unwrap();
        drop(mem);
        let db_path = workspace.join("memory").join("brain.db");
        let conn = Connection::open(&db_path).unwrap();
        let old = (Local::now() - Duration::days(90)).to_rfc3339();
        let recent = Local::now().to_rfc3339();
        for ts in [&old, &recent] {
            conn.execute(
                "INSERT INTO file_events (path, agent, turn_id, created_at) \
                 VALUES ('note.md', 'ag', NULL, ?1)",
                params![ts],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO self_change_receipts (agent, kind, quote, verdict, created_at) \
                 VALUES ('ag', 'narrative', NULL, 'contested', ?1)",
                params![ts],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO ratification_receipts \
                     (packet_id, mirror_hash, principal, prev_hash, created_at) \
                 VALUES ('p1', 'm', 'op', 'prev', ?1)",
                params![ts],
            )
            .unwrap();
        }
        drop(conn);
        db_path
    }

    fn journal_row_count(db_path: &Path, table: &str) -> i64 {
        let conn = Connection::open(db_path).unwrap();
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
    }

    fn journal_cfg(workspace: &Path) -> MemoryConfig {
        let mut cfg = default_cfg();
        cfg.archive_after_days = 0;
        cfg.purge_after_days = 0;
        cfg.conversation_retention_days = 0;
        cfg.daily_retention_days = 0;
        cfg.core_retention_days = 0;
        cfg.audit_retention_days = 0;
        cfg
    }

    #[test]
    fn prunes_old_journal_rows_and_reports_counts() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path();
        let db_path = seed_journal_rows(workspace);

        let mut cfg = journal_cfg(workspace);
        cfg.journal_retention_days = 30;
        cfg.ratification_receipts_retention_days = 60;
        run_if_due(&cfg, workspace).unwrap();

        assert_eq!(
            journal_row_count(&db_path, "file_events"),
            1,
            "старая строка file_events подрезана, свежая на месте"
        );
        assert_eq!(
            journal_row_count(&db_path, "self_change_receipts"),
            1,
            "старая строка self_change_receipts подрезана, свежая на месте"
        );
        assert_eq!(
            journal_row_count(&db_path, "ratification_receipts"),
            1,
            "старая строка ratification_receipts подрезана, свежая на месте"
        );

        let state: HygieneState = serde_json::from_str(
            &fs::read_to_string(workspace.join("state").join(STATE_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(state.last_report.pruned_file_event_rows, 1);
        assert_eq!(state.last_report.pruned_self_change_receipt_rows, 1);
        assert_eq!(state.last_report.pruned_ratification_receipt_rows, 1);
    }

    #[test]
    fn zero_journal_retention_keeps_rows_byte_identical() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path();
        let db_path = seed_journal_rows(workspace);

        // Дефолт: оба ключа 0 = keep forever — ни удалений, ни ошибок.
        let cfg = journal_cfg(workspace);
        assert_eq!(cfg.journal_retention_days, 0);
        assert_eq!(cfg.ratification_receipts_retention_days, 0);
        run_if_due(&cfg, workspace).unwrap();

        assert_eq!(journal_row_count(&db_path, "file_events"), 2);
        assert_eq!(journal_row_count(&db_path, "self_change_receipts"), 2);
        assert_eq!(journal_row_count(&db_path, "ratification_receipts"), 2);

        let state: HygieneState = serde_json::from_str(
            &fs::read_to_string(workspace.join("state").join(STATE_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(state.last_report.pruned_file_event_rows, 0);
        assert_eq!(state.last_report.pruned_self_change_receipt_rows, 0);
        assert_eq!(state.last_report.pruned_ratification_receipt_rows, 0);
    }

    #[test]
    fn journal_retention_keys_are_independent() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path();
        let db_path = seed_journal_rows(workspace);

        // Только journal-ключ: ratification_receipts не тронуты.
        let mut cfg = journal_cfg(workspace);
        cfg.journal_retention_days = 30;
        run_if_due(&cfg, workspace).unwrap();

        assert_eq!(journal_row_count(&db_path, "file_events"), 1);
        assert_eq!(journal_row_count(&db_path, "self_change_receipts"), 1);
        assert_eq!(
            journal_row_count(&db_path, "ratification_receipts"),
            2,
            "ratification_receipts_retention_days = 0 держит журнал ратификации"
        );
    }

    #[test]
    fn journal_pruning_noop_on_legacy_db_without_tables() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path();
        fs::create_dir_all(workspace.join("memory")).unwrap();
        let conn = Connection::open(workspace.join("memory").join("brain.db")).unwrap();
        conn.execute_batch("CREATE TABLE memory_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);")
            .unwrap();
        drop(conn);

        let mut cfg = journal_cfg(workspace);
        cfg.journal_retention_days = 30;
        cfg.ratification_receipts_retention_days = 30;
        run_if_due(&cfg, workspace).unwrap();

        let state: HygieneState = serde_json::from_str(
            &fs::read_to_string(workspace.join("state").join(STATE_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(state.last_report.pruned_file_event_rows, 0);
        assert_eq!(state.last_report.pruned_self_change_receipt_rows, 0);
        assert_eq!(state.last_report.pruned_ratification_receipt_rows, 0);
    }

    #[test]
    fn prunes_existing_audit_rows_when_audit_disabled() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path();
        let db_path = seed_audit_db(workspace);

        let cfg = default_cfg();
        assert!(!cfg.audit_enabled);
        run_if_due(&cfg, workspace).unwrap();

        assert_eq!(
            audit_row_count(&db_path),
            1,
            "disabling collection must not disable retention for existing audit rows"
        );
    }

    #[test]
    fn disabled_audit_does_not_create_missing_audit_db_for_pruning() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path();
        let audit_db = workspace.join("memory").join("audit.db");

        let cfg = default_cfg();
        assert!(!cfg.audit_enabled);
        run_if_due(&cfg, workspace).unwrap();

        assert!(
            !audit_db.exists(),
            "default-off audit must not create an audit db just to prune"
        );
        assert!(
            !workspace.join("memory").exists(),
            "audit pruning must not create the memory directory"
        );
    }

    #[cfg(unix)]
    #[test]
    fn audit_pruning_rejects_unsafe_sqlite_leaves_without_touching_outside_files() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        for suffix in ["", "-wal", "-shm"] {
            let root = TempDir::new().unwrap();
            let outside = TempDir::new().unwrap();
            let sentinel = outside.path().join("sentinel");
            fs::write(&sentinel, b"unchanged").unwrap();
            fs::set_permissions(&sentinel, fs::Permissions::from_mode(0o666)).unwrap();

            let db_path = seed_audit_db(root.path());
            let unsafe_path = crate::sqlite_permissions::sqlite_sidecar_path(&db_path, suffix);
            if suffix.is_empty() {
                fs::remove_file(&db_path).unwrap();
            }
            symlink(&sentinel, &unsafe_path).unwrap();

            assert!(prune_audit_entries(root.path(), 30).is_err(), "{suffix}");
            assert_eq!(fs::read(&sentinel).unwrap(), b"unchanged");
            assert_eq!(
                fs::metadata(&sentinel).unwrap().permissions().mode() & 0o777,
                0o666
            );
        }
    }
    #[tokio::test]
    async fn archive_mode_purge_days_zero_keeps_aged_conversation() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path();

        let mem = SqliteMemory::new("sqlite", workspace).unwrap();
        mem.store(
            "conv_old",
            "ancient conversation context",
            MemoryCategory::Conversation,
            None,
        )
        .await
        .unwrap();
        drop(mem);

        let conn = Connection::open(workspace.join("memory").join("brain.db")).unwrap();
        let old = (Local::now() - Duration::days(400)).to_rfc3339();
        conn.execute(
            "UPDATE memories SET created_at = ?1, updated_at = ?1 WHERE key = 'conv_old'",
            params![old],
        )
        .unwrap();
        drop(conn);

        let mut cfg = archive_cfg();
        cfg.physical_purge_days = 0;
        run_if_due(&cfg, workspace).unwrap();

        let mem2 = SqliteMemory::new("sqlite", workspace).unwrap();
        let entry = mem2.get("conv_old").await.unwrap();
        assert!(
            entry.is_some(),
            "physical_purge_days = 0 must keep conversation rows forever"
        );
        assert!(entry.unwrap().superseded_by.is_none());
    }

    #[tokio::test]
    async fn archive_mode_keeps_core_row_with_recent_activity() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path();

        let mem = SqliteMemory::new("sqlite", workspace).unwrap();
        mem.store(
            "core_updated",
            "still-maintained durable fact",
            MemoryCategory::Core,
            None,
        )
        .await
        .unwrap();
        drop(mem);

        let conn = Connection::open(workspace.join("memory").join("brain.db")).unwrap();
        let old_created = (Local::now() - Duration::days(120)).to_rfc3339();
        conn.execute(
            "UPDATE memories SET created_at = ?1 WHERE key = 'core_updated'",
            params![old_created],
        )
        .unwrap();
        drop(conn);

        let mut cfg = archive_cfg();
        cfg.core_retention_days = 30;
        run_if_due(&cfg, workspace).unwrap();

        // Семантика (дока schema.rs): core-возраст — абсолютный, от first
        // write; свежий updated_at не спасает от архивации. Пин от регресса
        // на updated_at: при нём эта строка НЕ заархивировалась бы.
        assert_eq!(
            superseded_by_of(workspace, "core_updated"),
            Some(crate::ARCHIVED_SENTINEL.to_string()),
            "core retention is absolute age from created_at"
        );
    }

    #[tokio::test]
    async fn archive_mode_repeat_run_does_not_recount_archived_rows() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path();

        let mem = SqliteMemory::new("sqlite", workspace).unwrap();
        mem.store(
            "core_old",
            "obsolete durable fact",
            MemoryCategory::Core,
            None,
        )
        .await
        .unwrap();
        drop(mem);

        let conn = Connection::open(workspace.join("memory").join("brain.db")).unwrap();
        let old = (Local::now() - Duration::days(120)).to_rfc3339();
        conn.execute(
            "UPDATE memories SET created_at = ?1, updated_at = ?1 WHERE key = 'core_old'",
            params![old],
        )
        .unwrap();
        drop(conn);

        let mut cfg = archive_cfg();
        cfg.core_retention_days = 30;
        run_if_due(&cfg, workspace).unwrap();

        // сброс троттлинга и повторный прогон
        std::fs::remove_file(state_path(workspace)).unwrap();
        run_if_due(&cfg, workspace).unwrap();

        let state: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(state_path(workspace)).unwrap()).unwrap();
        assert_eq!(
            state["last_report"]["archived_core_rows"].as_u64(),
            Some(0),
            "repeat run must not recount already-archived rows"
        );
    }
}
