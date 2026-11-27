//! Слой суждений с пакетной ратификацией (контур RAM, epic 9-4, Д0).
//!
//! Очередь `proposals` в brain.db с жизненным циклом
//! propose → review → accept/reject/withdraw → исход эпизодом.
//! Четыре генератора пишут сюда (trend из consolidation, correction из
//! явных фраз оператора и молчаливого канала file-events, contradiction из
//! разрешённых конфликтов, expensive_episode из дорогих эпизодов);
//! ратификация — только от principal из конфига, подпись текстом
//! «утверждаю [N]» / «отклоняю [N]: причина».
//!
//! Таблицы создаёт `SqliteMemory::init_schema` (единый источник DDL,
//! как memory_refs в 9-2 и self_change_receipts в 9-3); здесь — только
//! чтение/запись поверх существующего файла. Чистая логика над rusqlite:
//! вызывающий с путём к brain.db открывает файл напрямую (паттерн 9-2/9-3),
//! без изменения Memory-трейта апстрима.
//!
//! Гарантии:
//! - propose идемпотентен по `source_hash` (UNIQUE);
//! - apply_signed атомарен: записи DEPLOY (accepted/rejected эпизоды +
//!   смена статусов) и receipt с prev-цепочкой — в одной транзакции;
//!   частичный отказ оставляет остаток pending;
//! - подпись принимается только от principal (тройная привязка
//!   packet_id + mirror_hash + principal);
//! - отказ оператора — сохранённый эпизод (с комментарием), дороже
//!   принятия по следу аудита;
//! - proposals-строки не удаляются: старые без подписи «затухают с аудитом»
//!   (status='expired') и исключены из retention-очистки по построению.

use crate::receipts::{RECEIPT_BUSY_TIMEOUT, brain_db_path};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::path::Path;
use zeroclaw_api::memory_traits::RatificationProposal;

/// Имя таблицы очереди предложений.
pub const PROPOSALS_TABLE: &str = "proposals";
/// Имя таблицы ссылок предложений на память.
pub const PROPOSAL_REFS_TABLE: &str = "proposal_refs";
/// Имя append-only журнала ратификации.
pub const RATIFICATION_RECEIPTS_TABLE: &str = "ratification_receipts";
/// Имя узкого журнала фактов file_write (без diff — ADR-016).
pub const FILE_EVENTS_TABLE: &str = "file_events";

/// Busy-timeout прямых открытий brain.db из runtime (паттерн receipts.rs:
/// агентский ход, worker и CLI могут держать БД одновременно).
pub const PROPOSALS_BUSY_TIMEOUT: std::time::Duration = RECEIPT_BUSY_TIMEOUT;

/// Статус: ждёт ратификации.
pub const STATUS_PENDING: &str = "pending";
/// Статус: принято, исход задеплоен.
pub const STATUS_ACCEPTED: &str = "accepted";
/// Статус: отклонено оператором, эпизод сохранён.
pub const STATUS_REJECTED: &str = "rejected";
/// Статус: затухло без подписи (с аудитом, строка остаётся).
pub const STATUS_EXPIRED: &str = "expired";

/// Стабильный source-hash от частей (SHA-256 hex). Производители предложений
/// обязаны давать детерминированный ключ: повторная генерация того же факта
/// схлопывается в одну строку очереди.
#[must_use]
pub fn source_hash(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

/// Исход propose: создана новая строка или существовала (идемпотентность).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProposeOutcome {
    pub created: bool,
    pub proposal_id: i64,
}

/// Одна позиция пакета (порядок нумерации в подписи оператора — 1-based).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PacketItem {
    pub proposal_id: i64,
    pub position: u32,
    pub kind: String,
    pub payload: String,
    pub priority: String,
}

/// Пакет из pending-предложений, собранный для доставки оператору.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub packet_id: String,
    pub items: Vec<PacketItem>,
    /// Сколько кандидатов удержано throttle'ем при сборке.
    pub throttled: u32,
}

/// Одна запись append-only журнала ратификации.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    pub id: i64,
    pub packet_id: String,
    pub mirror_hash: String,
    pub principal: String,
    pub prev_hash: String,
    pub created_at: String,
}

/// Решение оператора по одной позиции пакета.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemDecision {
    Accept,
    Reject,
}

/// Исход apply_signed: что разрешено, что отклонено, что осталось ждать,
/// и receipt (только когда хотя бы одна позиция разрешена).
#[derive(Debug, Clone, Default)]
pub struct ApplyOutcome {
    pub accepted: Vec<i64>,
    pub rejected: Vec<i64>,
    pub receipt: Option<Receipt>,
}

/// Счётчики proposals по статусам (расширение `memory stats`, паттерн 9-3).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProposalCounts {
    pub pending: u64,
    pub accepted: u64,
    pub rejected: u64,
    pub expired: u64,
}

impl ProposalCounts {
    #[must_use]
    pub fn total(&self) -> u64 {
        self.pending + self.accepted + self.rejected + self.expired
    }
}

/// Факт file_write из узкого журнала (без содержимого — ADR-016).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEvent {
    pub id: i64,
    pub path: String,
    pub agent: Option<String>,
    pub turn_id: Option<String>,
    pub created_at: String,
}

// ── Dream revision operations (epic 9-6) ─────────────────────────────

/// The eight dream revision actions with equal status in the queue
/// (OQ: «очередь только новых фактов обречена» — validate/prune/relink
/// идут с тем же весом, что и promote).
pub const REVISION_ACTIONS: &[&str] = &[
    "promote",
    "demote",
    "link",
    "archive",
    "supersede",
    "merge",
    "validate",
    "refute",
];

/// Одна revision-операция из строгого JSON-ответа сна. Ратифицируются
/// факты и доказательства, не формат (Anthropic-вывод): `rationale` —
/// обязательное обоснование, `refs` — durable-ссылки на эпизоды/строки,
/// по которым принимается решение.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RevisionOp {
    /// Одно из [`REVISION_ACTIONS`].
    pub action: String,
    /// Целевая строка памяти (key) для операций над существующим суждением.
    #[serde(default)]
    pub target: Option<String>,
    /// Новый текст (новая версия/факт) для promote/supersede/merge/refute.
    #[serde(default)]
    pub content: Option<String>,
    /// Обязательное обоснование операции.
    pub rationale: String,
    /// Durable-ссылки (keys) на доказательства.
    #[serde(default)]
    pub refs: Vec<String>,
}

/// Разобрать строгий JSON ответ сна в список операций (не прозу).
/// Принимает либо JSON-массив операций, либо объект с полем `operations`.
/// Каждая операция валидируется: action ∈ REVISION_ACTIONS, rationale
/// непуст, для операций над существующим суждением target обязателен.
/// Результат усечён до `max_ops` — остаток входов остаётся в дельте.
pub fn parse_revision_ops(payload: &str, max_ops: usize) -> Result<Vec<RevisionOp>> {
    let trimmed = payload.trim();
    let value: serde_json::Value = serde_json::from_str(trimmed)
        .with_context(|| "dream output is not valid JSON (operations, not prose)")?;
    let list = match &value {
        serde_json::Value::Array(items) => items.clone(),
        serde_json::Value::Object(map) => map
            .get("operations")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .with_context(|| "dream output object lacks an `operations` array")?,
        _ => bail!("dream output must be a JSON array of operations"),
    };
    let mut ops = Vec::new();
    for item in list {
        let op: RevisionOp = serde_json::from_value(item)
            .with_context(|| "dream operation has invalid structure")?;
        if !REVISION_ACTIONS.contains(&op.action.as_str()) {
            bail!("unknown dream revision action: {}", op.action);
        }
        if op.rationale.trim().is_empty() {
            bail!("dream revision operation '{}' lacks a rationale", op.action);
        }
        // promote создаёт новую запись (target не нужен); все операции над
        // существующими суждениями — включая link — требуют target: иначе
        // link маппил бы обоснование как source_key и писал мусорные
        // memory_refs.
        let needs_target = op.action != "promote";
        if needs_target && op.target.as_deref().is_none_or(str::is_empty) {
            bail!("dream revision operation '{}' lacks a target", op.action);
        }
        if matches!(
            op.action.as_str(),
            "promote" | "supersede" | "merge" | "refute"
        ) && op.content.as_deref().is_none_or(str::is_empty)
        {
            bail!("dream revision operation '{}' lacks content", op.action);
        }
        ops.push(op);
        if ops.len() >= max_ops {
            break;
        }
    }
    Ok(ops)
}

/// Предложение kind=revision из dream-операции (payload — строгий JSON,
/// схема proposals не меняется).
///
/// `source_hash` — семантический (epic 9-8, DR-1): части
/// `[dream-revision, action, target, content]`. `_run_tag` вызывающий
/// (dream.rs) передаёт по-прежнему, но в хэш не входит; `rationale`
/// исключён намеренно — переформулировка обоснования не должна порождать
/// дубль и не должна маскировать тот же факт. Механика propose прежняя:
/// UNIQUE + `ON CONFLICT DO NOTHING` — семантический дубль от
/// последующих прогонов сна молча скипается (pending/accepted/rejected
/// одинаково: отклонённое не воскресает само).
#[must_use]
pub fn revision_proposal(
    agent: Option<&str>,
    op: &RevisionOp,
    _run_tag: &str,
) -> RatificationProposal {
    let mut refs: Vec<String> = op.refs.clone();
    if let Some(target) = &op.target
        && !refs.contains(target)
    {
        refs.push(target.clone());
    }
    let payload = serde_json::to_string(op).unwrap_or_default();
    let target = op.target.as_deref().unwrap_or_default();
    let content = op.content.as_deref().unwrap_or_default();
    let mut proposal = RatificationProposal::new(
        RatificationProposal::KIND_REVISION,
        payload,
        RatificationProposal::PRIORITY_NORMAL,
        source_hash(&["dream-revision", &op.action, target, content]),
    )
    .with_refs(refs);
    proposal.agent = agent.map(str::to_string);
    proposal
}

/// Очередь ревалидации: принятые ратификатором факты (исходные эпизоды
/// accepted, Core, не-суперседнутые) старше `older_than_days` возвращаются
/// на переоценку против свежих эпизодов. Per-agent scope через join на
/// agents; `agent = None` — общий план (системный сон отдельной визой).
pub fn revalidate_queue(
    conn: &Connection,
    older_than_days: u32,
    agent: Option<&str>,
    limit: usize,
) -> Result<Vec<String>> {
    let cutoff = chrono::Local::now() - chrono::Duration::days(i64::from(older_than_days));
    let cutoff = cutoff.to_rfc3339();
    let agent_clause = if agent.is_some() {
        "AND (SELECT alias FROM agents WHERE agents.id = memories.agent_id) = ?2"
    } else {
        ""
    };
    let sql = format!(
        "SELECT key FROM memories \
         WHERE category = 'core' AND superseded_by IS NULL \
           AND key LIKE 'ratification_accepted_%' AND updated_at <= ?1 \
           {agent_clause} \
         ORDER BY updated_at LIMIT ?3"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        rusqlite::params![cutoff, agent.unwrap_or(""), limit as i64],
        |row| row.get(0),
    )?;
    rows.collect::<Result<Vec<String>, _>>().map_err(Into::into)
}

/// Исполнить revision-операцию детерминированно, non-destructive: старые
/// версии суперседятся (supersede-цепочка memories, reuse 9-2 sentinel),
/// никаких DELETE. Выполняется внутри транзакции apply_signed — только
/// после подписи оператора (сон сам ничего не пишет в memories).
fn execute_revision_op(
    tx: &rusqlite::Transaction,
    op: &RevisionOp,
    now: &str,
    proposal_id: i64,
) -> Result<()> {
    let insert_version = |tx: &rusqlite::Transaction, key: &str, content: &str| -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        tx.execute(
            "INSERT INTO memories (
                id, key, content, category, created_at, updated_at,
                session_id, namespace, importance, agent_id, kind, pinned, tenant_id,
                superseded_by
             )
             VALUES (?1, ?2, ?3, 'core', ?4, ?5, NULL, 'default', 0.5,
                     (SELECT id FROM agents WHERE alias = 'default' LIMIT 1),
                     NULL, 0, NULL, NULL)",
            rusqlite::params![id, key, content, now, now],
        )?;
        Ok(id)
    };
    let link_refs = |tx: &rusqlite::Transaction, source_key: &str| -> Result<()> {
        for reference in &op.refs {
            tx.execute(
                "INSERT INTO memory_refs (source_key, target_key, created_at) \
                 VALUES (?1, ?2, ?3)",
                rusqlite::params![source_key, reference, now],
            )?;
        }
        Ok(())
    };
    let supersede_target = |tx: &rusqlite::Transaction, target: &str, new_id: &str| -> Result<()> {
        tx.execute(
            "UPDATE memories SET superseded_by = ?1 WHERE key = ?2 AND superseded_by IS NULL",
            rusqlite::params![new_id, target],
        )?;
        Ok(())
    };

    match op.action.as_str() {
        "promote" => {
            let key = format!("dream_promote_{proposal_id}");
            insert_version(tx, &key, op.content.as_deref().unwrap_or_default())?;
            link_refs(tx, &key)?;
        }
        "demote" => {
            // Обратимое понижение значимости; строка остаётся находимой.
            tx.execute(
                "UPDATE memories SET importance = MAX(importance - 0.2, 0.05), updated_at = ?1 \
                 WHERE key = ?2 AND superseded_by IS NULL",
                rusqlite::params![now, op.target],
            )?;
        }
        "link" => {
            let source = op.target.as_deref().unwrap_or(op.rationale.as_str());
            link_refs(tx, source)?;
        }
        "archive" => {
            // Архивирование = обратимое скрытие (sentinel 9-2), не DELETE.
            tx.execute(
                "UPDATE memories SET superseded_by = ?1, updated_at = ?2 \
                 WHERE key = ?3 AND superseded_by IS NULL",
                rusqlite::params![crate::ARCHIVED_SENTINEL, now, op.target],
            )?;
        }
        "supersede" | "refute" => {
            let key = format!("dream_{}_{proposal_id}", op.action);
            let body = match op.action.as_str() {
                "refute" => format!(
                    "REFUTED: {}\n{}",
                    op.rationale,
                    op.content.as_deref().unwrap_or_default()
                ),
                _ => op.content.clone().unwrap_or_default(),
            };
            let new_id = insert_version(tx, &key, &body)?;
            if let Some(target) = &op.target {
                supersede_target(tx, target, &new_id)?;
            }
            link_refs(tx, &key)?;
        }
        "merge" => {
            let key = format!("dream_merge_{proposal_id}");
            let new_id = insert_version(tx, &key, op.content.as_deref().unwrap_or_default())?;
            if let Some(target) = &op.target {
                supersede_target(tx, target, &new_id)?;
            }
            for reference in &op.refs {
                supersede_target(tx, reference, &new_id)?;
            }
        }
        "validate" => {
            // Продление статуса: факт подтверждён свежими эпизодами.
            tx.execute(
                "UPDATE memories SET updated_at = ?1 WHERE key = ?2 AND superseded_by IS NULL",
                rusqlite::params![now, op.target],
            )?;
            link_refs(tx, op.target.as_deref().unwrap_or_default())?;
        }
        other => bail!("unknown dream revision action: {other}"),
    }
    Ok(())
}

fn sha256_hex(data: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn genesis_hash() -> String {
    sha256_hex("zeroclaw-ratification-genesis")
}

/// Хеш-отпечаток записи receipt (звено prev-цепочки следующей записи).
#[must_use]
pub fn receipt_hash(receipt: &Receipt) -> String {
    sha256_hex(&format!(
        "{}|{}|{}|{}|{}",
        receipt.packet_id,
        receipt.mirror_hash,
        receipt.principal,
        receipt.prev_hash,
        receipt.created_at
    ))
}

/// Канонический mirror пакета: JSON-массив позиций в порядке id.
/// serde_json::Map — BTreeMap (preserve_order не включён), поэтому
/// сериализация детерминирована.
#[must_use]
pub fn mirror_hash(items: &[PacketItem]) -> String {
    let value: Vec<serde_json::Value> = items
        .iter()
        .map(|item| {
            serde_json::json!({
                "id": item.proposal_id,
                "kind": item.kind,
                "priority": item.priority,
                "payload": item.payload,
            })
        })
        .collect();
    sha256_hex(&serde_json::to_string(&value).unwrap_or_default())
}

/// Открыть соединение поверх brain.db с busy_timeout (паттерн receipts.rs).
fn open_connection(db_path: &Path) -> Result<Connection> {
    let conn = Connection::open(db_path)?;
    conn.busy_timeout(RECEIPT_BUSY_TIMEOUT)?;
    Ok(conn)
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        params![name],
        |row| row.get(0),
    )?;
    Ok(n > 0)
}

/// Предложить item в очередь. Идемпотентно по `source_hash`: существующая
/// строка (в любом статусе) не дублируется.
pub fn propose(conn: &Connection, proposal: &RatificationProposal) -> Result<ProposeOutcome> {
    let now = chrono::Local::now().to_rfc3339();
    let tx = conn.unchecked_transaction()?;
    let changed = tx.execute(
        "INSERT INTO proposals (agent, kind, payload, status, priority, source_hash, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(source_hash) DO NOTHING",
        params![
            proposal.agent,
            proposal.kind,
            proposal.payload,
            STATUS_PENDING,
            proposal.priority,
            proposal.source_hash,
            now,
        ],
    )?;
    let proposal_id: i64 = tx.query_row(
        "SELECT id FROM proposals WHERE source_hash = ?1",
        params![proposal.source_hash],
        |row| row.get(0),
    )?;
    if changed > 0 {
        for (idx, reference) in proposal.source_refs.iter().enumerate() {
            tx.execute(
                "INSERT INTO proposal_refs (proposal_id, position, ref_key, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![proposal_id, idx as i64, reference, now],
            )?;
        }
    }
    tx.commit()?;
    Ok(ProposeOutcome {
        created: changed > 0,
        proposal_id,
    })
}

/// `propose` по пути к БД; файла нет — no-op Ok(false) (паттерн receipts.rs:
/// receipt не может существовать без БД, в которой правили).
pub fn propose_at(db_path: &Path, proposal: &RatificationProposal) -> Result<ProposeOutcome> {
    if !db_path.exists() {
        return Ok(ProposeOutcome {
            created: false,
            proposal_id: 0,
        });
    }
    let conn = open_connection(db_path)?;
    propose(&conn, proposal)
}

/// Счётчики по статусам. Таблицы нет (БД старше миграции) — нули, не ошибка.
pub fn counts(conn: &Connection) -> Result<ProposalCounts> {
    let mut stats = ProposalCounts::default();
    if !table_exists(conn, PROPOSALS_TABLE)? {
        return Ok(stats);
    }
    let mut stmt = conn.prepare("SELECT status, COUNT(*) FROM proposals GROUP BY status")?;
    let mapped = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    for row in mapped {
        let (status, n) = row?;
        let n = u64::try_from(n).unwrap_or(0);
        match status.as_str() {
            STATUS_PENDING => stats.pending = n,
            STATUS_ACCEPTED => stats.accepted = n,
            STATUS_REJECTED => stats.rejected = n,
            STATUS_EXPIRED => stats.expired = n,
            _ => {}
        }
    }
    Ok(stats)
}

/// Счётчики по пути к БД; файла нет — нули.
pub fn counts_at(db_path: &Path) -> Result<ProposalCounts> {
    if !db_path.exists() {
        return Ok(ProposalCounts::default());
    }
    let conn = Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(RECEIPT_BUSY_TIMEOUT)?;
    counts(&conn)
}

/// Pending-предложения старше `older_than_days` (календарный триггер;
/// коррекции оператора бессрочны и сюда не попадают).
pub fn aged_pending(conn: &Connection, older_than_days: u32) -> Result<Vec<i64>> {
    let cutoff = chrono::Local::now() - chrono::Duration::days(i64::from(older_than_days));
    let cutoff = cutoff.to_rfc3339();
    let mut stmt = conn.prepare(
        "SELECT id FROM proposals \
         WHERE status = ?1 AND kind != ?2 AND created_at <= ?3 \
         ORDER BY id",
    )?;
    let rows = stmt.query_map(
        params![
            STATUS_PENDING,
            RatificationProposal::KIND_CORRECTION,
            cutoff
        ],
        |row| row.get(0),
    )?;
    rows.collect::<Result<Vec<i64>, _>>().map_err(Into::into)
}

/// Коррекции оператора без пакета (событийный триггер — немедленная сборка).
pub fn unpacked_corrections(conn: &Connection) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT id FROM proposals \
         WHERE status = ?1 AND kind = ?2 AND packet_id IS NULL \
         ORDER BY id",
    )?;
    let rows = stmt.query_map(
        params![STATUS_PENDING, RatificationProposal::KIND_CORRECTION],
        |row| row.get(0),
    )?;
    rows.collect::<Result<Vec<i64>, _>>().map_err(Into::into)
}

fn load_items(conn: &Connection, ids: &[i64]) -> Result<Vec<PacketItem>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut items = Vec::with_capacity(ids.len());
    for id in ids.iter() {
        let item = conn
            .query_row(
                "SELECT id, kind, payload, priority FROM proposals \
                 WHERE id = ?1 AND status = ?2",
                params![id, STATUS_PENDING],
                |row| {
                    Ok(PacketItem {
                        proposal_id: row.get(0)?,
                        position: 0,
                        kind: row.get(1)?,
                        payload: row.get(2)?,
                        priority: row.get(3)?,
                    })
                },
            )
            .with_context(|| format!("loading pending proposal {id}"))?;
        items.push(item);
    }
    for (idx, item) in items.iter_mut().enumerate() {
        item.position = (idx + 1) as u32;
    }
    Ok(items)
}

/// Throttle: коррекции оператора бессрочны и бюджету не подлежат; revision-
/// операции сна (epic 9-6) идут с равным статусом (validate/prune/relink —
/// не дно очереди); остальные классы удерживаются в порядке ценности
/// (expensive_episode, contradiction, trend) в пределах недельного бюджета —
/// trend вырезается первым.
/// Возвращает (admitted, throttled_count).
fn apply_throttle(items: Vec<PacketItem>, weekly_budget: u32) -> (Vec<PacketItem>, u32) {
    let mut admitted: Vec<PacketItem> = items
        .iter()
        .filter(|item| item.kind == RatificationProposal::KIND_CORRECTION)
        .cloned()
        .collect();
    let mut remaining = (weekly_budget as usize).saturating_sub(admitted.len());
    for keep in [
        RatificationProposal::KIND_REVISION,
        RatificationProposal::KIND_EXPENSIVE_EPISODE,
        RatificationProposal::KIND_CONTRADICTION,
        RatificationProposal::KIND_TREND,
    ] {
        for item in items.iter().filter(|item| item.kind == keep) {
            if remaining == 0 {
                break;
            }
            admitted.push(item.clone());
            remaining -= 1;
        }
    }
    admitted.sort_by_key(|item| item.proposal_id);
    for (idx, item) in admitted.iter_mut().enumerate() {
        item.position = (idx + 1) as u32;
    }
    let throttled = items.len() - admitted.len();
    (admitted, throttled as u32)
}

/// Собрать пакет из id кандидатов: throttle по бюджету, привязка packet_id.
/// Возвращает None, если после throttle не осталось позиций.
pub fn assemble_packet(
    conn: &Connection,
    packet_id: &str,
    candidate_ids: &[i64],
    weekly_budget: u32,
) -> Result<Option<Packet>> {
    let items = load_items(conn, candidate_ids)?;
    if items.is_empty() {
        return Ok(None);
    }
    let (items, throttled) = apply_throttle(items, weekly_budget);
    if items.is_empty() {
        return Ok(None);
    }
    let tx = conn.unchecked_transaction()?;
    for item in &items {
        tx.execute(
            "UPDATE proposals SET packet_id = ?1 WHERE id = ?2 AND status = ?3",
            params![packet_id, item.proposal_id, STATUS_PENDING],
        )?;
    }
    tx.commit()?;
    Ok(Some(Packet {
        packet_id: packet_id.to_string(),
        items,
        throttled,
    }))
}

/// Активный пакет: последний доставленный пакет с pending-позициями
/// (подпись принимается только на то, что оператор видел).
pub fn active_packet(conn: &Connection) -> Result<Option<Packet>> {
    let mut stmt = conn.prepare(
        "SELECT packet_id FROM proposals \
         WHERE status = ?1 AND packet_id IS NOT NULL AND delivered_at IS NOT NULL \
         GROUP BY packet_id \
         ORDER BY MAX(delivered_at) DESC, MAX(id) DESC \
         LIMIT 1",
    )?;
    let packet_id: Option<String> = stmt
        .query_row(params![STATUS_PENDING], |row| row.get(0))
        .optional()?;
    let Some(packet_id) = packet_id else {
        return Ok(None);
    };
    let mut stmt = conn.prepare(
        "SELECT id FROM proposals \
         WHERE status = ?1 AND packet_id = ?2 \
         ORDER BY id",
    )?;
    let ids: Vec<i64> = stmt
        .query_map(params![STATUS_PENDING, packet_id], |row| row.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let items = load_items(conn, &ids)?;
    Ok(Some(Packet {
        packet_id,
        items,
        throttled: 0,
    }))
}

/// Пакеты, назначенные но не доставленные (retry по расписанию).
pub fn undelivered_packets(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT packet_id FROM proposals \
         WHERE status = ?1 AND packet_id IS NOT NULL AND delivered_at IS NULL \
         ORDER BY packet_id",
    )?;
    let rows = stmt.query_map(params![STATUS_PENDING], |row| row.get(0))?;
    rows.collect::<Result<Vec<String>, _>>().map_err(Into::into)
}

/// Загрузить пакет по id (для доставки/retry).
pub fn packet_by_id(conn: &Connection, packet_id: &str) -> Result<Option<Packet>> {
    let mut stmt = conn.prepare(
        "SELECT id FROM proposals \
         WHERE status = ?1 AND packet_id = ?2 \
         ORDER BY id",
    )?;
    let ids: Vec<i64> = stmt
        .query_map(params![STATUS_PENDING, packet_id], |row| row.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    if ids.is_empty() {
        return Ok(None);
    }
    let items = load_items(conn, &ids)?;
    Ok(Some(Packet {
        packet_id: packet_id.to_string(),
        items,
        throttled: 0,
    }))
}

/// Отметить пакет доставленным (время — канонический порядок активности).
pub fn mark_delivered(conn: &Connection, packet_id: &str) -> Result<()> {
    conn.execute(
        "UPDATE proposals SET delivered_at = ?1 \
         WHERE packet_id = ?2 AND status = ?3 AND delivered_at IS NULL",
        params![chrono::Local::now().to_rfc3339(), packet_id, STATUS_PENDING],
    )?;
    Ok(())
}

fn last_receipt(conn: &Connection) -> Result<Option<Receipt>> {
    let receipt = conn
        .query_row(
            "SELECT id, packet_id, mirror_hash, principal, prev_hash, created_at \
             FROM ratification_receipts ORDER BY id DESC LIMIT 1",
            [],
            |row| {
                Ok(Receipt {
                    id: row.get(0)?,
                    packet_id: row.get(1)?,
                    mirror_hash: row.get(2)?,
                    principal: row.get(3)?,
                    prev_hash: row.get(4)?,
                    created_at: row.get(5)?,
                })
            },
        )
        .optional()?;
    Ok(receipt)
}

/// Развернуть решение оператора по пакету.
///
/// `decisions` адресует позиции пакета (1-based, как в подписи); пустой
/// список = все позиции. Транзакция: accepted → запись в memories (Core),
/// rejected → сохранённый эпизод (Daily, archived-sentinel) с комментарием,
/// смена статусов и receipt с prev-цепочкой. Остаток остаётся pending.
/// Отказ не-principal — до любой записи, receipt не пишется.
pub fn apply_signed(
    conn: &Connection,
    ratification: &zeroclaw_config::schema::MemoryRatificationConfig,
    packet: &Packet,
    signer: &str,
    decisions: &[(u32, ItemDecision)],
    reject_reason: Option<&str>,
    debug_decisions: bool,
) -> Result<ApplyOutcome> {
    if signer != ratification.principal || ratification.principal.is_empty() {
        bail!("ratification refused: signer is not the configured principal");
    }
    // Тройная привязка: mirror вычисляется заново над текущим pending-составом.
    if mirror_hash(&packet.items) != mirror_hash_of(conn, &packet.packet_id)? {
        bail!("ratification refused: packet mirror changed since delivery");
    }

    let selected: Vec<&PacketItem> = if decisions.is_empty() {
        packet.items.iter().collect()
    } else {
        packet
            .items
            .iter()
            .filter(|item| decisions.iter().any(|(pos, _)| *pos == item.position))
            .collect()
    };
    if selected.is_empty() {
        bail!("ratification refused: no matching pending items in the packet");
    }
    let decision_of = |position: u32| -> ItemDecision {
        decisions
            .iter()
            .find(|(pos, _)| *pos == position)
            .map_or(ItemDecision::Accept, |(_, d)| *d)
    };

    // Revision-позиции (epic 9-6): payload — строгий JSON операции. Разбор
    // до транзакции: мусорный payload отказывает применение целиком
    // (receipt не пишется, DEPLOY не выполняется).
    let mut revision_ops: std::collections::HashMap<i64, RevisionOp> =
        std::collections::HashMap::new();
    for item in &selected {
        if item.kind == RatificationProposal::KIND_REVISION
            && decision_of(item.position) == ItemDecision::Accept
        {
            let op = parse_revision_ops(&item.payload, 1)
                .with_context(|| {
                    format!("revision proposal {} has invalid payload", item.proposal_id)
                })?
                .into_iter()
                .next()
                .with_context(|| {
                    format!("revision proposal {} has empty payload", item.proposal_id)
                })?;
            revision_ops.insert(item.proposal_id, op);
        }
    }

    let now = chrono::Local::now().to_rfc3339();
    let tx = conn.unchecked_transaction()?;
    let mut outcome = ApplyOutcome::default();
    for item in &selected {
        let status = match decision_of(item.position) {
            ItemDecision::Accept => STATUS_ACCEPTED,
            ItemDecision::Reject => STATUS_REJECTED,
        };
        let episode_key = format!("ratification_{status}_{}", item.proposal_id);
        let episode_content = match decision_of(item.position) {
            ItemDecision::Accept => {
                if let Some(op) = revision_ops.get(&item.proposal_id) {
                    // Исход — эпизод с action/обоснованием/source-refs;
                    // сама операция исполняется ниже (non-destructive).
                    format!(
                        "[ratified revision] {}: {} refs=[{}]",
                        op.action,
                        op.rationale,
                        op.refs.join(", ")
                    )
                } else {
                    format!("[ratified {}] {}", item.kind, item.payload)
                }
            }
            ItemDecision::Reject => {
                format!(
                    "[rejected {}] {} — reason: {}",
                    item.kind,
                    item.payload,
                    reject_reason.unwrap_or("no reason given")
                )
            }
        };
        // Исход — эпизодом: accepted → Core (исход задеплоирован), rejected →
        // архивный эпизод (затухает с аудитом, direct get находит).
        let (category, superseded_by): (&str, Option<&str>) = match decision_of(item.position) {
            ItemDecision::Accept => ("core", None),
            ItemDecision::Reject => ("daily", Some(crate::ARCHIVED_SENTINEL)),
        };
        tx.execute(
            "INSERT INTO memories (
                id, key, content, category, created_at, updated_at,
                session_id, namespace, importance, agent_id, kind, pinned, tenant_id,
                superseded_by
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, 'default', 0.5,
                     (SELECT id FROM agents WHERE alias = 'default' LIMIT 1),
                     NULL, 0, NULL, ?7)",
            params![
                uuid::Uuid::new_v4().to_string(),
                episode_key,
                episode_content,
                category,
                now,
                now,
                superseded_by,
            ],
        )?;
        tx.execute(
            "UPDATE proposals SET status = ?1, resolved_at = ?2 WHERE id = ?3",
            params![status, now, item.proposal_id],
        )?;
        if let Some(op) = revision_ops.get(&item.proposal_id) {
            execute_revision_op(&tx, op, &now, item.proposal_id)
                .with_context(|| format!("executing revision proposal {}", item.proposal_id))?;
        }
        match decision_of(item.position) {
            ItemDecision::Accept => outcome.accepted.push(item.proposal_id),
            ItemDecision::Reject => outcome.rejected.push(item.proposal_id),
        }
    }

    // Receipt — только после успеха ВСЕХ записей DEPLOY (та же транзакция).
    let prev_hash = last_receipt(&tx)?
        .map(|receipt| receipt_hash(&receipt))
        .unwrap_or_else(genesis_hash);
    let mirror = mirror_hash(&packet.items);
    tx.execute(
        "INSERT INTO ratification_receipts \
             (packet_id, mirror_hash, principal, prev_hash, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![packet.packet_id, mirror, signer, prev_hash, now],
    )?;
    let receipt_id = tx.last_insert_rowid();
    tx.commit()?;
    // Наблюдаемость решений (9-17): вердикты — строго ПОСЛЕ commit, событие
    // не влияет на исход (best-effort по построению record!).
    if debug_decisions {
        for item in &selected {
            let decision = match decision_of(item.position) {
                ItemDecision::Accept => "accepted",
                ItemDecision::Reject => "rejected",
            };
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(::serde_json::json!({
                        "error_key": "memory.proposal_verdict",
                        "subsystem": "proposals",
                        "decision": decision,
                        "source": "ratification",
                        "keys_count": 1,
                        "has_reason": decision == "rejected" && reject_reason.is_some(),
                    })),
                "memory proposal verdict"
            );
        }
    }
    outcome.receipt = Some(Receipt {
        id: receipt_id,
        packet_id: packet.packet_id.clone(),
        mirror_hash: mirror,
        principal: signer.to_string(),
        prev_hash,
        created_at: now,
    });
    Ok(outcome)
}

/// Mirror текущего pending-состава пакета (для сверки при подписи).
fn mirror_hash_of(conn: &Connection, packet_id: &str) -> Result<String> {
    let packet = packet_by_id(conn, packet_id)?;
    Ok(packet.map_or_else(|| genesis_hash(), |p| mirror_hash(&p.items)))
}

/// Весь журнал receipts (новые сверху нет — по id, для verify_chain/статиста).
pub fn receipts(conn: &Connection) -> Result<Vec<Receipt>> {
    if !table_exists(conn, RATIFICATION_RECEIPTS_TABLE)? {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT id, packet_id, mirror_hash, principal, prev_hash, created_at \
         FROM ratification_receipts ORDER BY id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(Receipt {
            id: row.get(0)?,
            packet_id: row.get(1)?,
            mirror_hash: row.get(2)?,
            principal: row.get(3)?,
            prev_hash: row.get(4)?,
            created_at: row.get(5)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

/// Проверить prev-цепочку журнала: genesis → receipt_hash звено за звеном.
#[must_use]
pub fn verify_chain(receipts: &[Receipt]) -> bool {
    let mut expected = genesis_hash();
    for receipt in receipts {
        if receipt.prev_hash != expected {
            return false;
        }
        expected = receipt_hash(receipt);
    }
    true
}

/// Затухание-с-аудитом: pending-не-коррекции старше `days` → expired.
/// Строки не удаляются — retention-очистка proposals не предусмотрена,
/// исключение до resolved выполнено по построению. Коррекции бессрочны.
pub fn expire_stale(conn: &Connection, days: u32, debug_decisions: bool) -> Result<u64> {
    let cutoff = chrono::Local::now() - chrono::Duration::days(i64::from(days));
    let changed = conn.execute(
        "UPDATE proposals SET status = ?1, resolved_at = ?2 \
         WHERE status = ?3 AND kind != ?4 AND created_at <= ?5",
        params![
            STATUS_EXPIRED,
            chrono::Local::now().to_rfc3339(),
            STATUS_PENDING,
            RatificationProposal::KIND_CORRECTION,
            cutoff.to_rfc3339(),
        ],
    )?;
    let changed = u64::try_from(changed).unwrap_or(0);
    // Наблюдаемость решений (9-17): строго после персистента вердикта.
    if debug_decisions && changed > 0 {
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(
                ::serde_json::json!({
                    "error_key": "memory.proposal_verdict",
                    "subsystem": "proposals",
                    "decision": "expired",
                    "source": "expiry",
                    "keys_count": changed,
                    "has_reason": false,
                })
            ),
            "memory proposal verdict"
        );
    }
    Ok(changed)
}

/// Записать факт file_write в узкий журнал (путь, метка, turn-id — не diff).
pub fn record_file_event(
    conn: &Connection,
    path: &str,
    agent: Option<&str>,
    turn_id: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO file_events (path, agent, turn_id, created_at) \
         VALUES (?1, ?2, ?3, ?4)",
        params![path, agent, turn_id, chrono::Local::now().to_rfc3339()],
    )?;
    Ok(())
}

/// Записать факт file_write по пути к БД; файла нет — no-op.
pub fn record_file_event_at(
    db_path: &Path,
    path: &str,
    agent: Option<&str>,
    turn_id: Option<&str>,
) -> Result<()> {
    if !db_path.exists() {
        return Ok(());
    }
    let conn = open_connection(db_path)?;
    record_file_event(&conn, path, agent, turn_id)
}

/// Последние факты file_events (детектор молчаливого канала коррекций).
pub fn recent_file_events(conn: &Connection, limit: usize) -> Result<Vec<FileEvent>> {
    if !table_exists(conn, FILE_EVENTS_TABLE)? {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT id, path, agent, turn_id, created_at FROM file_events \
         ORDER BY id DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit as i64], |row| {
        Ok(FileEvent {
            id: row.get(0)?,
            path: row.get(1)?,
            agent: row.get(2)?,
            turn_id: row.get(3)?,
            created_at: row.get(4)?,
        })
    })?;
    let mut events: Vec<FileEvent> = rows.collect::<Result<Vec<_>, _>>()?;
    events.reverse();
    Ok(events)
}

/// Помощник для вызывающих с data_dir: путь к brain.db (единый источник —
/// receipts::brain_db_path).
#[must_use]
pub fn proposals_db_path(data_dir: &Path) -> std::path::PathBuf {
    brain_db_path(data_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroclaw_config::schema::MemoryRatificationConfig;

    fn test_ratification() -> MemoryRatificationConfig {
        MemoryRatificationConfig {
            enabled: true,
            principal: "operator-1".into(),
            ..MemoryRatificationConfig::default()
        }
    }

    /// Tempdir + свежая БД со всеми миграциями (DDL создаёт SqliteMemory).
    fn mem_db() -> (tempfile::TempDir, std::path::PathBuf, Connection) {
        let tmp = tempfile::tempdir().unwrap();
        let db = proposals_db_path(tmp.path());
        let mem = crate::sqlite::SqliteMemory::new("test", tmp.path()).unwrap();
        drop(mem);
        let conn = open_connection(&db).unwrap();
        (tmp, db, conn)
    }

    fn proposal(kind: &str, payload: &str, hash: &str) -> RatificationProposal {
        RatificationProposal::new(kind, payload, RatificationProposal::PRIORITY_NORMAL, hash)
    }

    // ── epic 9-17: наблюдаемость решений (proposal_verdict) ──

    #[test]
    fn apply_signed_emits_verdict_events_after_commit() {
        let (_obs_guard, _writer_guard, _hook_guard, mut rx) = crate::log_capture::install();
        let (_tmp, _db, conn) = mem_db();
        let rat = test_ratification();
        let ids: Vec<i64> = ["v1", "v2"]
            .iter()
            .map(|h| {
                propose(&conn, &proposal("trend", "fact", h))
                    .unwrap()
                    .proposal_id
            })
            .collect();
        let packet = assemble_packet(&conn, "pkt-v", &ids, 20).unwrap().unwrap();
        mark_delivered(&conn, "pkt-v").unwrap();

        // Отклонённый пакет целиком: явные reject-решения по обеим позициям
        // (позиции без решения дефолтят в Accept). Решение уже персистентно
        // (commit прошёл) — события после факта.
        let outcome = apply_signed(
            &conn,
            &rat,
            &packet,
            "operator-1",
            &[(1, ItemDecision::Reject), (2, ItemDecision::Reject)],
            Some("не соответствует оси"),
            true,
        )
        .unwrap();
        assert!(outcome.receipt.is_some());

        let events = crate::log_capture::collect(&mut rx, "memory.proposal_verdict");
        assert_eq!(events.len(), 2, "one verdict per item, got {events:?}");
        for event in &events {
            assert_eq!(event["attributes"]["decision"], "rejected");
            assert_eq!(event["attributes"]["source"], "ratification");
            assert_eq!(event["attributes"]["keys_count"], 1);
            assert_eq!(event["attributes"]["has_reason"], true);
        }
        crate::log_capture::assert_scalar_only(&events);

        // Флаг выкл: повторная подпись другого пакета — молча.
        let ids2: Vec<i64> = ["v3"]
            .iter()
            .map(|h| {
                propose(&conn, &proposal("trend", "fact 2", h))
                    .unwrap()
                    .proposal_id
            })
            .collect();
        let packet2 = assemble_packet(&conn, "pkt-v2", &ids2, 20)
            .unwrap()
            .unwrap();
        mark_delivered(&conn, "pkt-v2").unwrap();
        let _ = apply_signed(&conn, &rat, &packet2, "operator-1", &[], None, false).unwrap();
        let events = crate::log_capture::collect(&mut rx, "memory.proposal_verdict");
        assert!(
            events.is_empty(),
            "debug_decisions off: zero proposal_verdict events, got {events:?}"
        );
    }

    #[test]
    fn expire_stale_emits_expired_verdict_event() {
        let (_obs_guard, _writer_guard, _hook_guard, mut rx) = crate::log_capture::install();
        let (_tmp, _db, conn) = mem_db();
        // pending-строка «в прошлом» (created_at напрямую состариваем).
        let id = propose(&conn, &proposal("trend", "old fact", "exp-1"))
            .unwrap()
            .proposal_id;
        let stale = (chrono::Local::now() - chrono::Duration::days(30)).to_rfc3339();
        conn.execute(
            "UPDATE proposals SET created_at = ?1 WHERE id = ?2",
            params![stale, id],
        )
        .unwrap();

        let faded = expire_stale(&conn, 7, true).unwrap();
        assert_eq!(faded, 1);

        let events = crate::log_capture::collect(&mut rx, "memory.proposal_verdict");
        assert_eq!(events.len(), 1, "got {events:?}");
        assert_eq!(events[0]["attributes"]["decision"], "expired");
        assert_eq!(events[0]["attributes"]["source"], "expiry");
        assert_eq!(events[0]["attributes"]["keys_count"], 1);
        crate::log_capture::assert_scalar_only(&events);
    }

    #[test]
    fn propose_creates_and_is_idempotent_by_source_hash() {
        let (_tmp, _db, conn) = mem_db();
        let p = proposal("trend", "deploys drift to Mondays", "hash-1");
        let first = propose(&conn, &p).unwrap();
        assert!(first.created);
        let second = propose(&conn, &p).unwrap();
        assert!(!second.created, "same source_hash must not duplicate");
        assert_eq!(second.proposal_id, first.proposal_id);
        let stats = counts(&conn).unwrap();
        assert_eq!(stats.pending, 1);
        assert_eq!(stats.total(), 1);
    }

    #[test]
    fn propose_records_source_refs() {
        let (_tmp, _db, conn) = mem_db();
        let p = proposal("trend", "t", "hash-refs").with_refs(["daily_2026-01-01_x", "core_y"]);
        let outcome = propose(&conn, &p).unwrap();
        assert!(outcome.created);
        let refs: Vec<String> = conn
            .prepare("SELECT ref_key FROM proposal_refs WHERE proposal_id = ?1 ORDER BY position")
            .unwrap()
            .query_map(params![outcome.proposal_id], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(refs, vec!["daily_2026-01-01_x", "core_y"]);
    }

    #[test]
    fn revision_proposal_hash_is_semantic_across_runs_and_rationales() {
        // DR-1 (spec 9-8): run_tag и rationale не входят в source_hash.
        let op_a = RevisionOp {
            action: "promote".into(),
            target: None,
            content: Some("durable fact".into()),
            rationale: "two fresh episodes support it".into(),
            refs: vec!["daily_a".into()],
        };
        let op_b = RevisionOp {
            rationale: "переформулированное обоснование того же факта".into(),
            ..op_a.clone()
        };
        let p1 = revision_proposal(Some("ag"), &op_a, "run-1#0");
        let p2 = revision_proposal(Some("ag"), &op_b, "run-2#0");
        assert_eq!(
            p1.source_hash, p2.source_hash,
            "одинаковая (action, target, content) от разных прогонов сна — тот же hash"
        );

        // Changed content → новый hash → новая строка очереди.
        let op_c = RevisionOp {
            content: Some("changed fact".into()),
            ..op_a.clone()
        };
        let p3 = revision_proposal(Some("ag"), &op_c, "run-1#0");
        assert_ne!(
            p1.source_hash, p3.source_hash,
            "изменившийся контент — другое предложение"
        );

        // Changed target → новый hash.
        let op_d = RevisionOp {
            action: "demote".into(),
            target: Some("core_x".into()),
            content: None,
            rationale: "stale".into(),
            refs: vec![],
        };
        let op_e = RevisionOp {
            target: Some("core_y".into()),
            ..op_d.clone()
        };
        let p4 = revision_proposal(Some("ag"), &op_d, "run-1#0");
        let p5 = revision_proposal(Some("ag"), &op_e, "run-1#0");
        assert_ne!(
            p4.source_hash, p5.source_hash,
            "другой target — другое предложение"
        );
    }

    #[test]
    fn revision_proposal_semantic_dup_collapses_in_queue() {
        // Два прогона дриминга с одинаковой операцией → одна строка очереди:
        // вторая propose молча скипается (UNIQUE + ON CONFLICT DO NOTHING).
        let (_tmp, _db, conn) = mem_db();
        let op_a = RevisionOp {
            action: "supersede".into(),
            target: Some("daily_a".into()),
            content: Some("deploys drift to Tuesdays".into()),
            rationale: "two fresh episodes contradict Mondays".into(),
            refs: vec!["daily_a".into(), "daily_b".into()],
        };
        let op_b = RevisionOp {
            rationale: "fresh evidence again".into(),
            ..op_a.clone()
        };
        let first = propose(&conn, &revision_proposal(Some("ag"), &op_a, "run-1#0")).unwrap();
        let second = propose(&conn, &revision_proposal(Some("ag"), &op_b, "run-2#0")).unwrap();
        assert!(first.created);
        assert!(
            !second.created,
            "семантический дубль от последующего прогона сна не плодит строку"
        );
        assert_eq!(second.proposal_id, first.proposal_id);
        let stats = counts(&conn).unwrap();
        assert_eq!(stats.total(), 1);
        assert_eq!(stats.pending, 1);
    }

    #[test]
    fn assemble_applies_throttle_dropping_trend_first() {
        let (_tmp, _db, conn) = mem_db();
        let mut ids = Vec::new();
        for i in 0..3 {
            let p = proposal("trend", &format!("trend {i}"), &format!("t-{i}"));
            ids.push(propose(&conn, &p).unwrap().proposal_id);
        }
        for i in 0..2 {
            let p = proposal("correction", &format!("correction {i}"), &format!("c-{i}"));
            ids.push(propose(&conn, &p).unwrap().proposal_id);
        }
        let p = proposal("expensive_episode", "big episode", "e-1");
        ids.push(propose(&conn, &p).unwrap().proposal_id);

        // Бюджет 3: коррекции сохраняются полностью, trend вырезается первым.
        let packet = assemble_packet(&conn, "pkt-1", &ids, 3)
            .unwrap()
            .expect("packet assembled");
        assert_eq!(packet.items.len(), 3);
        assert_eq!(packet.throttled, 3);
        assert!(
            packet
                .items
                .iter()
                .all(|i| i.kind == "correction" || i.kind == "expensive_episode"),
            "trend must be dropped first, corrections never"
        );
        assert_eq!(
            packet
                .items
                .iter()
                .filter(|i| i.kind == "correction")
                .count(),
            2
        );

        // Номера позиций в пакете последовательны (1-based).
        for (idx, item) in packet.items.iter().enumerate() {
            assert_eq!(item.position, (idx + 1) as u32);
        }
    }

    #[test]
    fn apply_signed_full_accept_deploys_and_chains_receipts() {
        let (_tmp, _db, conn) = mem_db();
        let rat = test_ratification();
        let a = propose(&conn, &proposal("trend", "trend A", "a")).unwrap();
        let b = propose(&conn, &proposal("trend", "trend B", "b")).unwrap();
        let packet = assemble_packet(&conn, "pkt-a", &[a.proposal_id, b.proposal_id], 20)
            .unwrap()
            .unwrap();
        mark_delivered(&conn, "pkt-a").unwrap();

        let outcome = apply_signed(&conn, &rat, &packet, "operator-1", &[], None, false).unwrap();
        assert_eq!(outcome.accepted.len(), 2);
        assert!(outcome.rejected.is_empty());
        let receipt = outcome.receipt.unwrap();
        assert_eq!(receipt.packet_id, "pkt-a");
        assert_eq!(receipt.principal, "operator-1");
        assert_eq!(
            receipt.prev_hash,
            genesis_hash(),
            "first receipt hangs on genesis"
        );

        // DEPLOY: accepted outcomes лежат в memories как Core.
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE key LIKE 'ratification_accepted_%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(n, 2);

        // Второй пакет: цепочка продолжается.
        let c = propose(&conn, &proposal("trend", "trend C", "c")).unwrap();
        let packet2 = assemble_packet(&conn, "pkt-b", &[c.proposal_id], 20)
            .unwrap()
            .unwrap();
        mark_delivered(&conn, "pkt-b").unwrap();
        let outcome2 = apply_signed(&conn, &rat, &packet2, "operator-1", &[], None, false).unwrap();
        assert_eq!(outcome2.receipt.unwrap().prev_hash, receipt_hash(&receipt));

        let chain = receipts(&conn).unwrap();
        assert_eq!(chain.len(), 2);
        assert!(verify_chain(&chain), "prev-chain must verify");

        let stats = counts(&conn).unwrap();
        assert_eq!(stats.accepted, 3);
        assert_eq!(stats.pending, 0);
    }

    #[test]
    fn apply_signed_partial_reject_leaves_rest_pending() {
        let (_tmp, _db, conn) = mem_db();
        let rat = test_ratification();
        let ids: Vec<i64> = ["x1", "x2", "x3"]
            .iter()
            .enumerate()
            .map(|(i, h)| {
                propose(&conn, &proposal("trend", &format!("fact {i}"), h))
                    .unwrap()
                    .proposal_id
            })
            .collect();
        let packet = assemble_packet(&conn, "pkt-p", &ids, 20).unwrap().unwrap();
        mark_delivered(&conn, "pkt-p").unwrap();

        // «отклоняю 2: причина» — позиция 2 в архив, остаток pending.
        let outcome = apply_signed(
            &conn,
            &rat,
            &packet,
            "operator-1",
            &[(2, ItemDecision::Reject)],
            Some("устарело"),
            false,
        )
        .unwrap();
        assert_eq!(outcome.accepted.len(), 0);
        assert_eq!(outcome.rejected, vec![ids[1]]);

        let stats = counts(&conn).unwrap();
        assert_eq!(stats.rejected, 1);
        assert_eq!(stats.pending, 2, "unaddressed items stay pending");

        // Rejected-эпизод сохранён и затухает (archived sentinel, direct get).
        let (content, superseded): (String, Option<String>) = conn
            .query_row(
                "SELECT content, superseded_by FROM memories \
                 WHERE key = ?1",
                params![format!("ratification_rejected_{}", ids[1])],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(content.contains("устарело"));
        assert_eq!(superseded.as_deref(), Some(crate::ARCHIVED_SENTINEL));
    }

    #[test]
    fn non_principal_signature_is_refused_without_receipt() {
        let (_tmp, _db, conn) = mem_db();
        let rat = test_ratification();
        let a = propose(&conn, &proposal("trend", "t", "np-1")).unwrap();
        let packet = assemble_packet(&conn, "pkt-np", &[a.proposal_id], 20)
            .unwrap()
            .unwrap();
        mark_delivered(&conn, "pkt-np").unwrap();

        let err = apply_signed(&conn, &rat, &packet, "someone-else", &[], None, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not the configured principal"));

        // Ничего не изменилось: ни DEPLOY, ни receipt.
        assert!(receipts(&conn).unwrap().is_empty());
        let stats = counts(&conn).unwrap();
        assert_eq!(stats.pending, 1);
        assert_eq!(stats.accepted, 0);
    }

    #[test]
    fn mirror_change_after_delivery_refuses_signature() {
        let (_tmp, _db, conn) = mem_db();
        let rat = test_ratification();
        let a = propose(&conn, &proposal("trend", "t", "m-1")).unwrap();
        let packet = assemble_packet(&conn, "pkt-m", &[a.proposal_id], 20)
            .unwrap()
            .unwrap();
        mark_delivered(&conn, "pkt-m").unwrap();

        // Пакет «подменён»: позиция вышла из pending до подписи.
        conn.execute(
            "UPDATE proposals SET status = 'expired' WHERE id = ?1",
            params![a.proposal_id],
        )
        .unwrap();
        let err = apply_signed(&conn, &rat, &packet, "operator-1", &[], None, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("mirror changed"));
        assert!(receipts(&conn).unwrap().is_empty());
    }

    #[test]
    fn active_packet_requires_delivery() {
        let (_tmp, _db, conn) = mem_db();
        let a = propose(&conn, &proposal("trend", "t", "act-1")).unwrap();
        assemble_packet(&conn, "pkt-u", &[a.proposal_id], 20).unwrap();
        // Не доставлен — подписи не принимаются.
        assert!(active_packet(&conn).unwrap().is_none());
        mark_delivered(&conn, "pkt-u").unwrap();
        let packet = active_packet(&conn).unwrap().unwrap();
        assert_eq!(packet.packet_id, "pkt-u");
        assert_eq!(packet.items.len(), 1);
    }

    #[test]
    fn undelivered_packets_are_listed_for_retry() {
        let (_tmp, _db, conn) = mem_db();
        let a = propose(&conn, &proposal("correction", "c", "ud-1")).unwrap();
        assemble_packet(&conn, "pkt-r", &[a.proposal_id], 20).unwrap();
        assert_eq!(undelivered_packets(&conn).unwrap(), vec!["pkt-r"]);
        mark_delivered(&conn, "pkt-r").unwrap();
        assert!(undelivered_packets(&conn).unwrap().is_empty());
    }

    #[test]
    fn expire_stale_fades_with_audit_and_keeps_corrections() {
        let (_tmp, _db, conn) = mem_db();
        let old = propose(&conn, &proposal("trend", "old trend", "exp-1")).unwrap();
        let corr = propose(&conn, &proposal("correction", "corr", "exp-2")).unwrap();
        // Состариваем вручную (created_at в прошлом).
        let past = (chrono::Local::now() - chrono::Duration::days(30)).to_rfc3339();
        conn.execute(
            "UPDATE proposals SET created_at = ?1 WHERE id IN (?2, ?3)",
            params![past, old.proposal_id, corr.proposal_id],
        )
        .unwrap();

        let faded = expire_stale(&conn, 7, false).unwrap();
        assert_eq!(faded, 1, "only the non-correction expires");
        let stats = counts(&conn).unwrap();
        assert_eq!(stats.expired, 1);
        assert_eq!(stats.pending, 1, "corrections are perpetual");
        // Строка не удалена — аудит-след сохранён.
        let status: String = conn
            .query_row(
                "SELECT status FROM proposals WHERE id = ?1",
                params![old.proposal_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, STATUS_EXPIRED);
    }

    #[test]
    fn aged_pending_skips_young_and_correction_items() {
        let (_tmp, _db, conn) = mem_db();
        let old = propose(&conn, &proposal("trend", "old", "age-1")).unwrap();
        propose(&conn, &proposal("trend", "young", "age-2")).unwrap();
        propose(&conn, &proposal("correction", "c", "age-3")).unwrap();
        let past = (chrono::Local::now() - chrono::Duration::days(10)).to_rfc3339();
        conn.execute(
            "UPDATE proposals SET created_at = ?1 WHERE id = ?2",
            params![past, old.proposal_id],
        )
        .unwrap();
        let aged = aged_pending(&conn, 7).unwrap();
        assert_eq!(aged, vec![old.proposal_id]);
    }

    #[test]
    fn file_events_round_trip() {
        let (_tmp, _db, conn) = mem_db();
        record_file_event(&conn, "SOUL.md", Some("agent-a"), Some("turn-7")).unwrap();
        record_file_event(&conn, "wiki/notes.md", None, None).unwrap();
        let events = recent_file_events(&conn, 10).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].path, "SOUL.md");
        assert_eq!(events[0].agent.as_deref(), Some("agent-a"));
        assert_eq!(events[0].turn_id.as_deref(), Some("turn-7"));
        assert_eq!(events[1].path, "wiki/notes.md");
    }

    #[test]
    fn mirror_hash_is_stable_and_order_sensitive() {
        let mk = |id: i64, payload: &str| PacketItem {
            proposal_id: id,
            position: 1,
            kind: "trend".into(),
            payload: payload.into(),
            priority: "normal".into(),
        };
        let a = mirror_hash(&[mk(1, "x"), mk(2, "y")]);
        let b = mirror_hash(&[mk(1, "x"), mk(2, "y")]);
        let c = mirror_hash(&[mk(2, "y"), mk(1, "x")]);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn missing_db_yields_zero_counts_and_noop_propose() {
        let tmp = tempfile::tempdir().unwrap();
        let db = proposals_db_path(tmp.path());
        assert_eq!(counts_at(&db).unwrap(), ProposalCounts::default());
        let outcome = propose_at(&db, &proposal("trend", "t", "miss-1")).unwrap();
        assert!(!outcome.created);
        assert!(!db.exists(), "no-op write must not create the database");
    }

    #[test]
    fn throttle_never_drops_corrections_even_at_zero_budget() {
        let (_tmp, _db, conn) = mem_db();
        let corr = proposal("correction", "operator correction", "tb-1");
        let trend = proposal("trend", "trend x", "tb-2");
        let ids = [
            propose(&conn, &corr).unwrap().proposal_id,
            propose(&conn, &trend).unwrap().proposal_id,
        ];
        let packet = assemble_packet(&conn, "pkt-tb", &ids, 0)
            .unwrap()
            .expect("corrections assemble even at zero budget");
        assert_eq!(packet.items.len(), 1);
        assert_eq!(packet.items[0].kind, "correction");
        assert_eq!(packet.throttled, 1);
    }

    #[test]
    fn source_hash_is_deterministic() {
        assert_eq!(source_hash(&["a", "b"]), source_hash(&["a", "b"]));
        assert_ne!(source_hash(&["a", "b"]), source_hash(&["b", "a"]));
    }

    // ── Dream revision operations (epic 9-6) ────────────────────────

    #[test]
    fn parse_revision_ops_round_trips_all_eight_actions() {
        let ops: Vec<RevisionOp> = [
            ("promote", None, Some("new durable fact")),
            ("demote", Some("core_a"), None),
            ("link", Some("core_a"), None),
            ("archive", Some("core_a"), None),
            ("supersede", Some("core_a"), Some("new version")),
            ("merge", Some("core_a"), Some("merged text")),
            ("validate", Some("core_a"), None),
            ("refute", Some("core_a"), Some("correction")),
        ]
        .iter()
        .map(|(action, target, content)| RevisionOp {
            action: action.to_string(),
            target: target.map(str::to_string),
            content: content.map(str::to_string),
            rationale: format!("grounded rationale for {action}"),
            refs: vec!["daily_1".to_string()],
        })
        .collect();
        let payload = serde_json::to_string(&ops).unwrap();
        let parsed = parse_revision_ops(&payload, 20).unwrap();
        assert_eq!(parsed, ops, "all eight actions round-trip");
        // Объектная обёртка {"operations": [...]} тоже принимается.
        let wrapped = serde_json::json!({ "operations": ops }).to_string();
        assert_eq!(parse_revision_ops(&wrapped, 20).unwrap().len(), 8);
    }

    #[test]
    fn parse_revision_ops_rejects_prose_and_bad_actions() {
        assert!(parse_revision_ops("I think we should archive it", 10).is_err());
        assert!(parse_revision_ops("[{\"action\":\"rewrite\"}]", 10).is_err());
        // rationale обязателен (ратифицируются факты и обоснования, не формат).
        let no_rationale = r#"[{"action":"archive","target":"core_a","rationale":""}]"#;
        assert!(parse_revision_ops(no_rationale, 10).is_err());
        // supersede без target/content отказывает.
        let bare = r#"[{"action":"supersede","rationale":"x","refs":[]}]"#;
        assert!(parse_revision_ops(bare, 10).is_err());
    }

    #[test]
    fn parse_revision_ops_caps_at_limit() {
        let payload = serde_json::to_string(
            &(0..5)
                .map(|i| RevisionOp {
                    action: "validate".to_string(),
                    target: Some(format!("core_{i}")),
                    content: None,
                    rationale: format!("r{i}"),
                    refs: vec![],
                })
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let parsed = parse_revision_ops(&payload, 2).unwrap();
        assert_eq!(parsed.len(), 2, "operations capped at the run limit");
    }

    #[test]
    fn revalidate_queue_returns_aged_accepted_facts() {
        let (_tmp, _db, conn) = mem_db();
        let now = chrono::Local::now().to_rfc3339();
        let old = (chrono::Local::now() - chrono::Duration::days(40)).to_rfc3339();
        // Принятый факт (исходной эпизод accepted) старше горизонта.
        conn.execute(
            "INSERT INTO memories (id, key, content, category, created_at, updated_at,
                session_id, namespace, importance, agent_id, kind, pinned, tenant_id)
             VALUES ('v1', 'ratification_accepted_1', 'ratified fact', 'core', ?1, ?2,
                     NULL, 'default', 0.5,
                     (SELECT id FROM agents WHERE alias = 'default' LIMIT 1),
                     NULL, 0, NULL)",
            params![old, old],
        )
        .unwrap();
        // Молодой принятый факт — не в очереди.
        conn.execute(
            "INSERT INTO memories (id, key, content, category, created_at, updated_at,
                session_id, namespace, importance, agent_id, kind, pinned, tenant_id)
             VALUES ('v2', 'ratification_accepted_2', 'fresh ratified fact', 'core', ?1, ?1,
                     NULL, 'default', 0.5,
                     (SELECT id FROM agents WHERE alias = 'default' LIMIT 1),
                     NULL, 0, NULL)",
            params![now],
        )
        .unwrap();
        // Суперседнутый старый факт — не в очереди (версионность non-destructive).
        conn.execute(
            "INSERT INTO memories (id, key, content, category, created_at, updated_at,
                session_id, namespace, importance, agent_id, kind, pinned, tenant_id, superseded_by)
             VALUES ('v3', 'ratification_accepted_3', 'aged superseded fact', 'core', ?1, ?1,
                     NULL, 'default', 0.5,
                     (SELECT id FROM agents WHERE alias = 'default' LIMIT 1),
                     NULL, 0, NULL, 'newer')",
            params![old],
        )
        .unwrap();

        let queue = revalidate_queue(&conn, 30, Some("default"), 10).unwrap();
        assert_eq!(queue, vec!["ratification_accepted_1".to_string()]);
    }

    #[test]
    fn apply_signed_executes_revision_ops_non_destructively() {
        let (_tmp, _db, conn) = mem_db();
        let rat = test_ratification();
        let now = chrono::Local::now().to_rfc3339();
        for (id, key, content) in [
            ("t1", "core_target", "deploys happen on Mondays"),
            ("t2", "core_evidence_a", "CI failed on Monday deploy"),
            ("t3", "core_evidence_b", "CI green on Tuesday deploy"),
        ] {
            conn.execute(
                "INSERT INTO memories (id, key, content, category, created_at, updated_at,
                    session_id, namespace, importance, agent_id, kind, pinned, tenant_id)
                 VALUES (?1, ?2, ?3, 'core', ?4, ?4, NULL, 'default', 0.5,
                         (SELECT id FROM agents WHERE alias = 'default' LIMIT 1),
                         NULL, 0, NULL)",
                params![id, key, content, now],
            )
            .unwrap();
        }
        // Пакет из трёх revision-предложений (supersede/validate/archive):
        // одно предложение = одна операция (одна позиция = одно решение).
        let mk = |action: &str,
                  target: Option<&str>,
                  content: Option<&str>,
                  rationale: &str,
                  refs: &[&str],
                  tag: &str| {
            let op = RevisionOp {
                action: action.to_string(),
                target: target.map(str::to_string),
                content: content.map(str::to_string),
                rationale: rationale.to_string(),
                refs: refs.iter().map(|r| r.to_string()).collect(),
            };
            let payload = serde_json::to_string(&[op]).unwrap();
            let proposal = RatificationProposal::new(
                RatificationProposal::KIND_REVISION,
                payload,
                RatificationProposal::PRIORITY_NORMAL,
                tag,
            );
            propose(&conn, &proposal).unwrap().proposal_id
        };
        let id_supersede = mk(
            "supersede",
            Some("core_target"),
            Some("deploys happen on Tuesdays"),
            "two episodes contradict Monday deploys",
            &["core_evidence_a", "core_evidence_b"],
            "rev-apply-1",
        );
        let id_validate = mk(
            "validate",
            Some("core_evidence_b"),
            None,
            "fresh episodes confirm",
            &["core_evidence_b"],
            "rev-apply-2",
        );
        let id_archive = mk(
            "archive",
            Some("core_evidence_a"),
            None,
            "episode stale",
            &[],
            "rev-apply-3",
        );
        let packet = assemble_packet(
            &conn,
            "pkt-rev",
            &[id_supersede, id_validate, id_archive],
            20,
        )
        .unwrap()
        .unwrap();
        mark_delivered(&conn, "pkt-rev").unwrap();

        let outcome = apply_signed(&conn, &rat, &packet, "operator-1", &[], None, false).unwrap();
        assert_eq!(outcome.accepted.len(), 3);

        // Non-destructive: старая версия суперседита новой (не удалена).
        let superseded_by: String = conn
            .query_row(
                "SELECT superseded_by FROM memories WHERE key = 'core_target'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_ne!(superseded_by, crate::ARCHIVED_SENTINEL);
        let new_row: (String, String) = conn
            .query_row(
                "SELECT id, content FROM memories WHERE id = ?1",
                params![superseded_by],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(new_row.1, "deploys happen on Tuesdays");
        // Archive — обратимое скрытие sentinel'ом 9-2.
        let archived: String = conn
            .query_row(
                "SELECT superseded_by FROM memories WHERE key = 'core_evidence_a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(archived, crate::ARCHIVED_SENTINEL);
        // Validate продлила строку (updated_at свежий).
        let updated: String = conn
            .query_row(
                "SELECT updated_at FROM memories WHERE key = 'core_evidence_b'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(updated >= now);
        // Исход — эпизод с action/обоснованием/source-refs.
        let episode: String = conn
            .query_row(
                "SELECT content FROM memories WHERE key = ?1",
                params![format!("ratification_accepted_{id_supersede}")],
                |row| row.get(0),
            )
            .unwrap();
        assert!(episode.contains("[ratified revision] supersede"));
        assert!(episode.contains("two episodes contradict Monday deploys"));
        assert!(episode.contains("core_evidence_a"));
        // Receipt-цепочка сходится.
        let chain = receipts(&conn).unwrap();
        assert!(verify_chain(&chain));
    }

    #[test]
    fn apply_signed_refuses_invalid_revision_payload() {
        let (_tmp, _db, conn) = mem_db();
        let rat = test_ratification();
        let id = propose(
            &conn,
            &RatificationProposal::new(
                RatificationProposal::KIND_REVISION,
                "проза вместо JSON-операции",
                RatificationProposal::PRIORITY_NORMAL,
                "rev-invalid-1",
            ),
        )
        .unwrap()
        .proposal_id;
        let packet = assemble_packet(&conn, "pkt-rev-bad", &[id], 20)
            .unwrap()
            .unwrap();
        mark_delivered(&conn, "pkt-rev-bad").unwrap();

        let err = apply_signed(&conn, &rat, &packet, "operator-1", &[], None, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid payload"), "got: {err}");
        // Ничего не применено: ни DEPLOY, ни receipt.
        assert!(receipts(&conn).unwrap().is_empty());
        let stats = counts(&conn).unwrap();
        assert_eq!(stats.pending, 1);
    }
}
