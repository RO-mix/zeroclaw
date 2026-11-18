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

/// Throttle: коррекции оператора бессрочны и бюджету не подлежат; остальные
/// классы удерживаются в порядке ценности (expensive_episode, contradiction,
/// trend) в пределах недельного бюджета — trend вырезается первым.
/// Возвращает (admitted, throttled_count).
fn apply_throttle(items: Vec<PacketItem>, weekly_budget: u32) -> (Vec<PacketItem>, u32) {
    let mut admitted: Vec<PacketItem> = items
        .iter()
        .filter(|item| item.kind == RatificationProposal::KIND_CORRECTION)
        .cloned()
        .collect();
    let mut remaining = (weekly_budget as usize).saturating_sub(admitted.len());
    for keep in [
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

    let now = chrono::Local::now().to_rfc3339();
    let tx = conn.unchecked_transaction()?;
    let mut outcome = ApplyOutcome::default();
    for item in selected {
        let status = match decision_of(item.position) {
            ItemDecision::Accept => STATUS_ACCEPTED,
            ItemDecision::Reject => STATUS_REJECTED,
        };
        let episode_key = format!("ratification_{status}_{}", item.proposal_id);
        let episode_content = match decision_of(item.position) {
            ItemDecision::Accept => {
                format!("[ratified {}] {}", item.kind, item.payload)
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
pub fn expire_stale(conn: &Connection, days: u32) -> Result<u64> {
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
    Ok(u64::try_from(changed).unwrap_or(0))
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

        let outcome = apply_signed(&conn, &rat, &packet, "operator-1", &[], None).unwrap();
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
        let outcome2 = apply_signed(&conn, &rat, &packet2, "operator-1", &[], None).unwrap();
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

        let err = apply_signed(&conn, &rat, &packet, "someone-else", &[], None)
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
        let err = apply_signed(&conn, &rat, &packet, "operator-1", &[], None)
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

        let faded = expire_stale(&conn, 7).unwrap();
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
}
