//! Слой суждений с пакетной ратификацией — runtime-сторона (epic 9-4, Д0).
//!
//! Здесь живут то, что не может жить в `zeroclaw-memory` (обратная
//! зависимость): разбор текстовой подписи оператора и перехват ingress,
//! генератор «дорогих эпизодов» и молчаливого канала коррекций
//! (file-events), сборщик пакетов и доставка оператору штатным
//! announce-путём (DELIVERY_FN), рабочий компонент daemon (календарный +
//! событийный триггер), журнал фактов file_write.
//!
//! Всё за `[memory.ratification].enabled` (дефолт выкл): при выключенном
//! флаге перехват возвращает None (обычный turn), worker не стартует,
//! журнал file-events не регистрируется — поведение байт-в-байт прежнее.
//! Self-ратификация агентом невозможна по построению: подпись принимается
//! только от principal, агент не совпадает с этим origin.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::Result;
use zeroclaw_api::memory_traits::RatificationProposal;
use zeroclaw_config::schema::Config;
use zeroclaw_memory::proposals::{self, ItemDecision, Packet, mirror_hash};

use crate::i18n::{get_required_cli_string, get_required_cli_string_with_args};

/// Тик worker'а: событийный канал (коррекция → немедленная сборка)
/// опрашивается на каждом тике; календарный — раз в сутки.
const WORKER_TICK: Duration = Duration::from_secs(30);
/// Состояние календарного триггера (паттерн hygiene state-file).
const WORKER_STATE_FILE: &str = "ratification_worker_state.json";
/// Эпизод дороже этого размера (символов) — кандидат в генератор
/// expensive_episode. Константа намеренно не конфиг: порог эвристический.
const EXPENSIVE_EPISODE_MIN_CHARS: usize = 2000;
/// Сколько последних file_events детектор молчаливого канала просматривает.
const FILE_EVENT_SCAN_LIMIT: usize = 200;
/// Разница mtime файла и журнальной метки, ниже которой правка считается
/// агентской (записанный факт и фактическая запись могут расходиться
/// на доли секунды).
const OPERATOR_EDIT_SKEW: Duration = Duration::from_secs(2);

// ── Текстовая подпись оператора (OQ-4: работает во всех каналах) ─────

/// Разобранная подпись: «утверждаю [N]» / «отклоняю [N]: причина».
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub accept: bool,
    /// Позиции пакета (1-based); None = все.
    pub items: Option<Vec<u32>>,
    pub reason: Option<String>,
}

/// Разобрать текстовую подпись оператора. Возвращает None, если сообщение
/// не является подписью (обычный текст идёт в обычный turn).
#[must_use]
pub fn parse_signature(text: &str) -> Option<Signature> {
    let text = text.trim();
    let (head, reason) = match text.split_once(':') {
        Some((head, reason)) => (head.trim(), Some(reason.trim().to_string())),
        None => (text, None),
    };
    let mut tokens = head.split_whitespace();
    let verb = tokens.next()?.to_lowercase();
    let accept = match verb.trim_end_matches(',') {
        "утверждаю" => true,
        "отклоняю" => false,
        _ => return None,
    };
    let mut items = Vec::new();
    for token in tokens {
        let cleaned: String = token
            .trim_matches(|c: char| c == ',' || c == '.' || c == ';')
            .to_string();
        if cleaned.is_empty() {
            continue;
        }
        let Ok(n) = cleaned.parse::<u32>() else {
            // Мусор после глагола — не подпись («утверждаю, что ...»).
            return None;
        };
        if n == 0 {
            return None;
        }
        items.push(n);
    }
    let items = (!items.is_empty()).then_some(items);
    if !accept && reason.as_deref().is_some_and(str::is_empty) {
        return None;
    }
    Some(Signature {
        accept,
        items,
        reason,
    })
}

// ── Перехват ingress: подпись principal на активный пакет ───────────

/// Точка перехвата ответа оператора (orchestrator, перед process_channel_message_body).
/// Возвращает Some(ответ оператору), когда сообщение consumed как подпись;
/// None — обычный turn продолжается. Флаг выкл → всегда None.
pub fn intercept_operator_signature(
    config: &Config,
    sender: &str,
    content: &str,
) -> Option<String> {
    let ratification = &config.memory.ratification;
    if !ratification.enabled {
        return None;
    }
    let signature = parse_signature(content)?;
    if sender != ratification.principal || ratification.principal.is_empty() {
        // Не principal: подпись не принимается, receipt не пишется — но и
        // чужой «утверждаю» не должен глотать чужой turn.
        return None;
    }
    let db = proposals::proposals_db_path(&config.data_dir);
    let Some(conn) = open_db(&db) else {
        return Some(refused("ratification database unavailable"));
    };
    let Ok(Some(packet)) = proposals::active_packet(&conn) else {
        return Some(refused("no active packet awaiting signature"));
    };
    let decisions: Vec<(u32, ItemDecision)> = match &signature.items {
        Some(items) => items
            .iter()
            .map(|position| {
                (
                    *position,
                    if signature.accept {
                        ItemDecision::Accept
                    } else {
                        ItemDecision::Reject
                    },
                )
            })
            .collect(),
        None => Vec::new(),
    };
    match proposals::apply_signed(
        &conn,
        ratification,
        &packet,
        sender,
        &decisions,
        signature.reason.as_deref(),
    ) {
        Ok(outcome) => {
            let accepted = outcome.accepted.len().to_string();
            let rejected = outcome.rejected.len().to_string();
            let pending =
                (packet.items.len() - outcome.accepted.len() - outcome.rejected.len()).to_string();
            let packet_id = packet.packet_id.clone();
            let receipt = outcome
                .receipt
                .map(|receipt| receipt.id.to_string())
                .unwrap_or_default();
            if outcome.accepted.is_empty() {
                Some(get_required_cli_string_with_args(
                    "cli-ratification-reply-rejected",
                    &[
                        ("rejected", &rejected),
                        ("packet", &packet_id),
                        ("receipt", &receipt),
                    ],
                ))
            } else if outcome.rejected.is_empty() && pending == "0" {
                Some(get_required_cli_string_with_args(
                    "cli-ratification-reply-accepted",
                    &[
                        ("accepted", &accepted),
                        ("packet", &packet_id),
                        ("receipt", &receipt),
                    ],
                ))
            } else {
                Some(get_required_cli_string_with_args(
                    "cli-ratification-reply-partial",
                    &[
                        ("accepted", &accepted),
                        ("rejected", &rejected),
                        ("pending", &pending),
                        ("packet", &packet_id),
                    ],
                ))
            }
        }
        Err(e) => Some(refused(&e.to_string())),
    }
}

fn refused(reason: &str) -> String {
    get_required_cli_string_with_args("cli-ratification-refused", &[("reason", reason)])
}

fn open_db(db: &Path) -> Option<rusqlite::Connection> {
    if !db.exists() {
        return None;
    }
    let conn = rusqlite::Connection::open(db).ok()?;
    conn.busy_timeout(proposals::PROPOSALS_BUSY_TIMEOUT).ok()?;
    Some(conn)
}

// ── Узкий журнал file-events (факт вызова, не diff — ADR-016) ───────

/// Процесс-глобальный путь к brain.db для журнала фактов file_write.
/// Регистрируется daemon'ом только при включённой ратификации; без
/// регистрации journaling — no-op (фича выкл, поведение прежнее).
static FILE_EVENT_JOURNAL: OnceLock<PathBuf> = OnceLock::new();

/// Зарегистрировать журнал file-events (daemon, при enabled).
pub fn register_file_event_journal(data_dir: PathBuf) {
    let _ = FILE_EVENT_JOURNAL.set(proposals::proposals_db_path(&data_dir));
}

/// Записать факт успешного file_write (путь, turn-id, агент). Любая ошибка
/// журнала — DEBUG, не роняет tool call.
pub(crate) fn journal_agent_file_write(path: &str, turn_id: &str, agent: Option<&str>) {
    let Some(db) = FILE_EVENT_JOURNAL.get() else {
        return;
    };
    if let Err(e) = proposals::record_file_event_at(db, path, agent, Some(turn_id)) {
        ::zeroclaw_log::record!(
            DEBUG,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({"path": path, "error": format!("{e}")})),
            "file-events journal write failed"
        );
    }
}

// ── Сборка пакета и доставка ─────────────────────────────────────────

/// Отрендерить пакет для оператора (user-facing — через Fluent).
#[must_use]
pub fn render_packet(packet: &Packet) -> String {
    let mirror = mirror_hash(&packet.items);
    let count = packet.items.len().to_string();
    let mut out = get_required_cli_string_with_args(
        "cli-ratification-packet-intro",
        &[
            ("packet", &packet.packet_id),
            ("count", &count),
            ("mirror", &mirror),
        ],
    );
    for item in &packet.items {
        out.push('\n');
        out.push_str(&get_required_cli_string_with_args(
            "cli-ratification-packet-item",
            &[
                ("position", &item.position.to_string()),
                ("kind", &item.kind),
                ("payload", &item.payload),
            ],
        ));
    }
    if packet.throttled > 0 {
        out.push('\n');
        out.push_str(&get_required_cli_string_with_args(
            "cli-ratification-packet-throttled",
            &[("count", &packet.throttled.to_string())],
        ));
    }
    out.push('\n');
    out.push_str(&get_required_cli_string_with_args(
        "cli-ratification-packet-outro",
        &[],
    ));
    out
}

/// Разрешить канал доставки: dotted `<type>.<alias>` — как есть; голый
/// транспорт — первый настроенный alias.
fn resolve_delivery_channel(config: &Config, transport: &str) -> Option<String> {
    let transport = transport.trim();
    if transport.is_empty() {
        return None;
    }
    if transport.contains('.') {
        return Some(transport.to_string());
    }
    let aliases: Vec<&String> = match transport.to_ascii_lowercase().as_str() {
        "telegram" => config.channels.telegram.keys().collect(),
        "discord" => config.channels.discord.keys().collect(),
        "slack" => config.channels.slack.keys().collect(),
        _ => Vec::new(),
    };
    let alias = aliases.first()?;
    Some(format!("{transport}.{}", alias.as_str()))
}

/// Разрешить канал и адресата доставки. Target — principal (OQ-2: не
/// зависим от owner_chat_id канала).
fn resolve_delivery_target(config: &Config) -> Option<(String, String)> {
    let ratification = &config.memory.ratification;
    if ratification.principal.is_empty() {
        return None;
    }
    let channel = resolve_delivery_channel(config, &ratification.principal_transport)?;
    Some((channel, ratification.principal.clone()))
}

/// Транспорт доставки пакета оператору. Инъекция через замыкание, чтобы
/// worker_tick_sync/flush_deliveries можно было тестировать без
/// оркестратора; прод-реализация оборачивает cron::scheduler::deliver_announcement.
type DeliverSink = dyn Fn(&str, &str, &str) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>
    + Send
    + Sync;

/// Прод-реализация доставки: штатный announce-путь (DELIVERY_FN, см.
/// cron::scheduler). Config уходит в Arc — future доставки не должна
/// заимствовать стек worker'а.
fn production_sink(config: &Config) -> std::sync::Arc<DeliverSink> {
    let config = std::sync::Arc::new(config.clone());
    std::sync::Arc::new(move |channel: &str, target: &str, text: &str| {
        let config = config.clone();
        let channel = channel.to_string();
        let target = target.to_string();
        let text = text.to_string();
        Box::pin(async move {
            crate::cron::scheduler::deliver_announcement(&config, &channel, &target, None, &text)
                .await
        })
    })
}

/// Одна подготовленная к доставке попытка: пакет собран, текст отрендерен,
/// адресат разрешён. Сама доставка — async и не держит соединение с БД
/// (`&rusqlite::Connection` не переживает await — весь sqlite-шаг синхронен,
/// будущее worker'а остаётся Send).
#[derive(Debug, Clone)]
pub(crate) struct PendingDelivery {
    pub packet_id: String,
    pub channel: String,
    pub target: String,
    pub text: String,
}

/// Зеркало per-decision (9-5): для revision-позиций пакета собирает
/// затронутый подграф графа знаний (1–2 hops от узлов, резолвленных из
/// target'ов операций) в mirror-данные пакета. Гейт — `knowledge.auto_capture`
/// (флаг выкл → секция не добавляется, байт-в-байт; knowledge.db не
/// создаётся). Граф пуст или недоступен → None (не ошибка).
fn graph_mirror_context(config: &Config, packet: &Packet) -> Option<String> {
    if !config.knowledge.auto_capture {
        return None;
    }
    // Target'ы — строгий JSON revision-операций (массив — форма apply_signed 9-6).
    let mut targets: Vec<String> = Vec::new();
    for item in &packet.items {
        if item.kind != RatificationProposal::KIND_REVISION {
            continue;
        }
        if let Ok(ops) = proposals::parse_revision_ops(&item.payload, 1) {
            targets.extend(ops.iter().filter_map(|op| op.target.clone()));
        }
    }
    targets.sort();
    targets.dedup();
    if targets.is_empty() {
        return None;
    }
    let db_path = config.knowledge.resolved_db_path();
    if !db_path.exists() {
        return None;
    }
    let graph =
        zeroclaw_memory::knowledge_graph::KnowledgeGraph::new(&db_path, config.knowledge.max_nodes)
            .ok()?;
    let mut root_ids: Vec<String> = Vec::new();
    for target in &targets {
        root_ids.extend(graph.nodes_by_provenance("memory", target).ok()?);
        root_ids.extend(graph.nodes_by_provenance("episode", target).ok()?);
    }
    root_ids.sort();
    root_ids.dedup();
    if root_ids.is_empty() {
        return None;
    }
    let mut lines: Vec<String> = Vec::new();
    for root in root_ids.iter().take(4) {
        let Ok((nodes, edges)) = graph.get_subgraph(root, 2) else {
            continue;
        };
        let titles: std::collections::HashMap<String, String> = nodes
            .iter()
            .map(|node| (node.id.clone(), node.title.clone()))
            .collect();
        for node in nodes.iter().take(8) {
            lines.push(get_required_cli_string_with_args(
                "cli-ratification-graph-mirror-node",
                &[("type", node.node_type.as_str()), ("title", &node.title)],
            ));
        }
        for edge in edges.iter().take(8) {
            let to = titles.get(&edge.to_id).map(String::as_str).unwrap_or("?");
            lines.push(get_required_cli_string_with_args(
                "cli-ratification-graph-mirror-edge",
                &[("relation", edge.relation.as_str()), ("to", to)],
            ));
        }
    }
    lines.sort();
    lines.dedup();
    if lines.is_empty() {
        None
    } else {
        let mut section = get_required_cli_string("cli-ratification-graph-mirror-header");
        section.push('\n');
        section.push_str(&lines.join("\n"));
        Some(section)
    }
}

/// Подготовить доставку существующего пакета (retry-путь и свежие пакеты).
fn pending_for_packet(config: &Config, packet: &Packet) -> Option<PendingDelivery> {
    let (channel, target) = resolve_delivery_target(config)?;
    let mut text = render_packet(packet);
    if let Some(context) = graph_mirror_context(config, packet) {
        text.push_str("\n\n");
        text.push_str(&context);
    }
    Some(PendingDelivery {
        packet_id: packet.packet_id.clone(),
        channel,
        target,
        text,
    })
}

/// Отправить подготовленные доставки и отметить успешные. Неуспех оставляет
/// пакет pending (delivered_at IS NULL) — retry по расписанию.
pub(crate) async fn flush_deliveries(
    config: &Config,
    pending: Vec<PendingDelivery>,
    deliver: &DeliverSink,
) {
    for delivery in pending {
        match deliver(&delivery.channel, &delivery.target, &delivery.text).await {
            Ok(()) => {
                if let Err(e) = mark_packet_delivered(config, &delivery.packet_id) {
                    ::zeroclaw_log::record!(
                        DEBUG,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({
                                "packet": delivery.packet_id,
                                "error": format!("{e}"),
                            })),
                        "ratification delivered-mark failed"
                    );
                }
            }
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({
                            "packet": delivery.packet_id,
                            "error": format!("{e}"),
                        })),
                    "ratification packet delivery failed; stays pending for retry"
                );
            }
        }
    }
}

/// Отметить пакет доставленным (короткое отдельное соединение).
fn mark_packet_delivered(config: &Config, packet_id: &str) -> Result<()> {
    let db = proposals::proposals_db_path(&config.data_dir);
    if !db.exists() {
        return Ok(());
    }
    let conn = rusqlite::Connection::open(&db)?;
    conn.busy_timeout(proposals::PROPOSALS_BUSY_TIMEOUT)?;
    proposals::mark_delivered(&conn, packet_id)
}

// ── Генераторы (worker) ──────────────────────────────────────────────

/// Генератор «дорогих эпизодов»: свежие conversation/daily строки больше
/// порога — кандидаты на внимание оператора. Идемпотентно по source_hash;
/// уже предложенные ключи исключаются прямо в SQL — иначе каждый тик
/// упрётся в ON CONFLICT и будет жечь id AUTOINCREMENT впустую.
fn generate_expensive_episodes(conn: &rusqlite::Connection) -> Result<u32> {
    let mut stmt = conn.prepare(
        "SELECT key, length(content) FROM memories \
         WHERE category IN ('conversation', 'daily') \
           AND length(content) > ?1 AND superseded_by IS NULL \
           AND key NOT IN ( \
               SELECT r.ref_key FROM proposal_refs r \
               JOIN proposals p ON p.id = r.proposal_id \
               WHERE p.kind = ?2 \
           ) \
         ORDER BY length(content) DESC LIMIT 20",
    )?;
    let rows = stmt.query_map(
        rusqlite::params![
            EXPENSIVE_EPISODE_MIN_CHARS as i64,
            RatificationProposal::KIND_EXPENSIVE_EPISODE
        ],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
    )?;
    let mut proposed = 0u32;
    for row in rows {
        let (key, size) = row?;
        let proposal = RatificationProposal::new(
            RatificationProposal::KIND_EXPENSIVE_EPISODE,
            format!("Expensive episode {key} ({size} chars)"),
            RatificationProposal::PRIORITY_NORMAL,
            proposals::source_hash(&["expensive_episode", &key]),
        )
        .with_refs([key]);
        if proposals::propose(conn, &proposal)?.created {
            proposed += 1;
        }
    }
    Ok(proposed)
}

/// Молчаливый канал коррекций: файл изменён ПОЗЖЕ последнего журнального
/// факта агентской записи по этому пути — правил оператор. Сверка строго с
/// максимальной меткой по пути: иначе повторная агентская запись (t2 > t1)
/// для старого события t1 выглядела бы как «операторская правка».
fn generate_silent_corrections(conn: &rusqlite::Connection) -> Result<u32> {
    let events = proposals::recent_file_events(conn, FILE_EVENT_SCAN_LIMIT)?;
    // Последнее известное событие по каждому пути (events — по id, старые первыми).
    let mut latest: std::collections::HashMap<String, &proposals::FileEvent> =
        std::collections::HashMap::new();
    for event in &events {
        latest.insert(event.path.clone(), event);
    }
    let skew = chrono::Duration::from_std(OPERATOR_EDIT_SKEW).unwrap_or_default();
    let mut proposed = 0u32;
    for event in latest.values() {
        let path = Path::new(&event.path);
        if !path.is_absolute() || !path.exists() {
            continue;
        }
        let Ok(modified) = std::fs::metadata(path).and_then(|meta| meta.modified()) else {
            continue;
        };
        let Ok(written) = chrono::DateTime::parse_from_rfc3339(&event.created_at) else {
            continue;
        };
        let written = written.with_timezone(&chrono::Local);
        let event_secs = (written + skew).timestamp();
        let modified_secs = modified
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        if modified_secs <= event_secs {
            continue;
        }
        let turn = event.turn_id.clone().unwrap_or_default();
        let proposal = RatificationProposal::new(
            RatificationProposal::KIND_CORRECTION,
            format!(
                "Operator edited {} after agent turn {turn} (silent channel)",
                event.path
            ),
            RatificationProposal::PRIORITY_HIGHEST,
            proposals::source_hash(&["file-event-correction", &event.path, &turn]),
        );
        if proposals::propose(conn, &proposal)?.created {
            proposed += 1;
        }
    }
    Ok(proposed)
}

// ── Триггеры ─────────────────────────────────────────────────────────

/// Собрать пакет из кандидатов и подготовить доставку (общий код триггеров).
fn assemble_delivery(
    config: &Config,
    conn: &rusqlite::Connection,
    candidates: &[i64],
    min_packet: u32,
) -> Result<Option<PendingDelivery>> {
    if candidates.len() < min_packet as usize {
        return Ok(None);
    }
    let Some(packet) = proposals::assemble_packet(
        conn,
        &uuid::Uuid::new_v4().to_string(),
        candidates,
        config.memory.ratification.weekly_budget,
    )?
    else {
        return Ok(None);
    };
    if let Some(delivery) = pending_for_packet(config, &packet) {
        return Ok(Some(delivery));
    }
    ::zeroclaw_log::record!(
        WARN,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
            .with_attrs(::serde_json::json!({
                "transport": config.memory.ratification.principal_transport,
            })),
        "ratification delivery refused: transport not configured"
    );
    Ok(None)
}

/// Календарный триггер: pending старше packet_trigger_days, не реже раза
/// в сутки (state-file, паттерн hygiene).
fn calendar_due(config: &Config) -> bool {
    let state_path = config.data_dir.join(WORKER_STATE_FILE);
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let last: Option<String> = std::fs::read_to_string(&state_path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|value| value["last_calendar_run"].as_str().map(str::to_string));
    if last.as_deref() == Some(today.as_str()) {
        return false;
    }
    let state = serde_json::json!({ "last_calendar_run": today });
    if let Err(e) = std::fs::write(&state_path, state.to_string()) {
        ::zeroclaw_log::record!(
            DEBUG,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({"error": format!("{e}")})),
            "ratification worker state write failed"
        );
    }
    true
}

/// Синхронный прогон цикла worker'а: вся работа с БД (без await). Возвращает
/// подготовленные доставки. Две фазы: календарная (раз в сутки: затухание,
/// генераторы, пакет по созревшим) и событийная (каждый тик: чистые SELECT'ы
/// по неупакованным коррекциям и retry). Генераторы сознательно не каждый
/// тик: их входы растут медленно, а идемпотентные propose-попытки на каждом
/// тике жгут id AUTOINCREMENT впустую (конфликт source_hash жжёт id ещё до
/// отката INSERT'а).
pub(crate) fn worker_tick_sync(
    config: &Config,
    conn: &rusqlite::Connection,
) -> Result<Vec<PendingDelivery>> {
    let ratification = &config.memory.ratification;
    let mut pending = Vec::new();

    // Снимок недоставленных пакетов ДО сборки новых: retry касается только
    // пакетов прошлых тиков — иначе только что собранный пакет попадал бы
    // в список дважды (сборка + retry в том же тике).
    let retry_packets = proposals::undelivered_packets(conn)?;

    // Календарная фаза (state-file, паттерн hygiene).
    if calendar_due(config) {
        // Затухание-с-аудитом: старые без подписи (не коррекции) → expired.
        proposals::expire_stale(conn, ratification.packet_trigger_days)?;
        // Генераторы.
        let _episodes = generate_expensive_episodes(conn)?;
        let _silent = generate_silent_corrections(conn)?;
        // Фидер графа знаний (9-5): accepted-исходы → узлы/рёбра. Момент —
        // self-review-пакет; ноль LLM. Гейт: knowledge.auto_capture (worker
        // и так живёт под memory.ratification.enabled).
        drain_graph_feeder(config, conn);
        // Пакет из предложений старше порога.
        let aged = proposals::aged_pending(conn, ratification.packet_trigger_days)?;
        if let Some(delivery) = assemble_delivery(config, conn, &aged, ratification.min_packet)? {
            pending.push(delivery);
        }
    }

    // Событийная фаза: коррекции без пакета → немедленная сборка.
    let corrections = proposals::unpacked_corrections(conn)?;
    if !corrections.is_empty()
        && let Some(delivery) = assemble_delivery(config, conn, &corrections, 1)?
    {
        pending.push(delivery);
    }

    // Retry: назначенные, но не доставленные пакеты прошлых тиков.
    for packet_id in retry_packets {
        if let Some(packet) = proposals::packet_by_id(conn, &packet_id)?
            && let Some(delivery) = pending_for_packet(config, &packet)
        {
            pending.push(delivery);
        }
    }
    Ok(pending)
}

/// Фидер графа знаний (9-5): accepted-исходы ратификации → узлы/рёбра
/// knowledge.db. Best-effort: любая ошибка (нет knowledge.db, занят файл,
/// ошибка drain) — WARN/DEBUG и пропуск; worker не падает. Метку дельты
/// продвигает только drain (внутри, за обработанные исходы).
fn drain_graph_feeder(config: &Config, brain: &rusqlite::Connection) {
    if !config.knowledge.auto_capture {
        return;
    }
    let graph = match zeroclaw_memory::knowledge_graph::KnowledgeGraph::new(
        &config.knowledge.resolved_db_path(),
        config.knowledge.max_nodes,
    ) {
        Ok(graph) => graph,
        Err(e) => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"error": format!("{e}")})),
                "graph feeder: knowledge graph unavailable, skipping drain"
            );
            return;
        }
    };
    match zeroclaw_memory::graph_feeder::drain(
        brain,
        &graph,
        &config.data_dir,
        config.knowledge.max_nodes,
    ) {
        Ok(report) => {
            if report.nodes_added > 0 || report.refused_max_nodes > 0 {
                ::zeroclaw_log::record!(
                    DEBUG,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_attrs(::serde_json::json!({
                            "nodes_added": report.nodes_added,
                            "edges_added": report.edges_added,
                            "duplicates_skipped": report.duplicates_skipped,
                            "refused_max_nodes": report.refused_max_nodes,
                        })),
                    "graph feeder drain completed"
                );
            }
        }
        Err(e) => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"error": format!("{e}")})),
                "graph feeder drain failed"
            );
        }
    }
}

/// Рабочий компонент ratification-worker (daemon supervisor): календарный
/// триггер (packet_trigger_days) + событийный канал (коррекция → сборка).
pub async fn run_ratification_worker(
    config: Config,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    let db = proposals::proposals_db_path(&config.data_dir);
    let deliver = production_sink(&config);
    let mut ticker = tokio::time::interval(WORKER_TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return Ok(()),
            _ = ticker.tick() => {}
        }
        if !db.exists() {
            continue;
        }
        // Синхронный тик на короткоживущем соединении; conn закрывается
        // до первого await.
        let pending = {
            let Ok(conn) = rusqlite::Connection::open(&db) else {
                continue;
            };
            if conn
                .busy_timeout(proposals::PROPOSALS_BUSY_TIMEOUT)
                .is_err()
            {
                continue;
            }
            match worker_tick_sync(&config, &conn) {
                Ok(pending) => pending,
                Err(e) => {
                    ::zeroclaw_log::record!(
                        DEBUG,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({"error": format!("{e}")})),
                        "ratification worker tick failed"
                    );
                    continue;
                }
            }
        };
        flush_deliveries(&config, pending, &*deliver).await;
    }
}

/// Событийный триггер после консолидации turn'а (orchestrator): есть
/// неупакованные коррекции — немедленная попытка сборки, не дожидаясь тика
/// worker'а. Флаг выкл → мгновенный no-op.
pub async fn event_tick_after_consolidation(config: &Config) {
    let ratification = &config.memory.ratification;
    if !ratification.enabled {
        return;
    }
    let db = proposals::proposals_db_path(&config.data_dir);
    let Some(conn) = open_db(&db) else {
        return;
    };
    let Ok(corrections) = proposals::unpacked_corrections(&conn) else {
        return;
    };
    if corrections.is_empty() {
        return;
    }
    let Ok(Some(delivery)) = assemble_delivery(config, &conn, &corrections, 1) else {
        return;
    };
    drop(conn);
    let deliver = production_sink(config);
    flush_deliveries(config, vec![delivery], &*deliver).await;
}

// ── Тесты ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use zeroclaw_config::schema::{Config, MemoryConfig, MemoryRatificationConfig};

    fn ratification_on() -> MemoryRatificationConfig {
        MemoryRatificationConfig {
            enabled: true,
            principal: "operator-1".into(),
            ..MemoryRatificationConfig::default()
        }
    }

    fn config_with(tmp: &tempfile::TempDir, ratification: MemoryRatificationConfig) -> Config {
        let mut config = Config::default();
        config.data_dir = tmp.path().to_path_buf();
        config.memory = MemoryConfig {
            ratification,
            ..MemoryConfig::default()
        };
        config
    }

    fn db_conn(tmp: &tempfile::TempDir) -> rusqlite::Connection {
        // SqliteMemory прогоняет DDL (включая таблицы proposals).
        let mem = zeroclaw_memory::sqlite::SqliteMemory::new("test", tmp.path()).unwrap();
        drop(mem);
        let db = proposals::proposals_db_path(tmp.path());
        rusqlite::Connection::open(db).unwrap()
    }

    #[test]
    fn parse_signature_accepts_reject_forms() {
        let all = parse_signature("утверждаю").unwrap();
        assert!(all.accept);
        assert_eq!(all.items, None);

        let listed = parse_signature("утверждаю 1, 3").unwrap();
        assert!(listed.accept);
        assert_eq!(listed.items, Some(vec![1, 3]));

        let reject = parse_signature("отклоняю 2: устарело, деплой по вторникам").unwrap();
        assert!(!reject.accept);
        assert_eq!(reject.items, Some(vec![2]));
        assert_eq!(
            reject.reason.as_deref(),
            Some("устарело, деплой по вторникам")
        );

        let reject_all = parse_signature("отклоняю: весь пакет мимо").unwrap();
        assert!(!reject_all.accept);
        assert_eq!(reject_all.items, None);
    }

    #[test]
    fn parse_signature_rejects_non_signatures() {
        assert!(parse_signature("привет, как дела").is_none());
        assert!(parse_signature("утверждаю, что это хорошая идея").is_none());
        assert!(parse_signature("").is_none());
        assert!(parse_signature("утверждаю ноль").is_none());
    }

    #[test]
    fn intercept_disabled_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config_with(&tmp, MemoryRatificationConfig::default());
        assert_eq!(
            intercept_operator_signature(&config, "operator-1", "утверждаю"),
            None
        );
    }

    #[test]
    fn intercept_non_principal_falls_through_to_normal_turn() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config_with(&tmp, ratification_on());
        assert_eq!(
            intercept_operator_signature(&config, "someone-else", "утверждаю"),
            None
        );
    }

    #[test]
    fn intercept_without_active_packet_refuses() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config_with(&tmp, ratification_on());
        let reply = intercept_operator_signature(&config, "operator-1", "утверждаю");
        let reply = reply.unwrap_or_default();
        assert!(
            !reply.is_empty(),
            "principal signature must get a refusal reply"
        );
    }

    #[test]
    fn intercept_full_cycle_accept_and_partial_reject() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config_with(&tmp, ratification_on());
        let conn = db_conn(&tmp);

        // Пакет из двух предложений, доставлен.
        let a = RatificationProposal::new(
            "trend",
            "trend A",
            "normal",
            proposals::source_hash(&["t", "A"]),
        );
        let b = RatificationProposal::new(
            "trend",
            "trend B",
            "normal",
            proposals::source_hash(&["t", "B"]),
        );
        let id_a = proposals::propose(&conn, &a).unwrap().proposal_id;
        let id_b = proposals::propose(&conn, &b).unwrap().proposal_id;
        let packet = proposals::assemble_packet(&conn, "pkt-1", &[id_a, id_b], 20)
            .unwrap()
            .unwrap();
        proposals::mark_delivered(&conn, "pkt-1").unwrap();
        drop(conn);

        // «утверждаю» от principal → всё принято, receipt есть.
        let reply = intercept_operator_signature(&config, "operator-1", "утверждаю").unwrap();
        assert!(reply.contains("pkt-1"), "reply names the packet: {reply}");
        let conn = db_conn(&tmp);
        let stats = proposals::counts(&conn).unwrap();
        assert_eq!(stats.accepted, 2);
        assert_eq!(proposals::receipts(&conn).unwrap().len(), 1);

        // Второй пакет, частичный отказ.
        let c = RatificationProposal::new(
            "trend",
            "trend C",
            "normal",
            proposals::source_hash(&["t", "C"]),
        );
        let d = RatificationProposal::new(
            "trend",
            "trend D",
            "normal",
            proposals::source_hash(&["t", "D"]),
        );
        let id_c = proposals::propose(&conn, &c).unwrap().proposal_id;
        let id_d = proposals::propose(&conn, &d).unwrap().proposal_id;
        let packet2 = proposals::assemble_packet(&conn, "pkt-2", &[id_c, id_d], 20)
            .unwrap()
            .unwrap();
        proposals::mark_delivered(&conn, "pkt-2").unwrap();
        drop(conn);

        let reply =
            intercept_operator_signature(&config, "operator-1", "отклоняю 2: не подтверждаю")
                .unwrap();
        assert!(!reply.is_empty());
        let conn = db_conn(&tmp);
        let stats = proposals::counts(&conn).unwrap();
        // «отклоняю 2» — только позиция 2 в архив; позиция 1 остаётся pending
        // (спека: частичный отказ → остаток pending, авто-принятия нет).
        assert_eq!(stats.accepted, 2);
        assert_eq!(stats.rejected, 1);
        assert_eq!(stats.pending, 1);
        // Цепочка receipt'ов сходится.
        let chain = proposals::receipts(&conn).unwrap();
        assert_eq!(chain.len(), 2);
        assert!(proposals::verify_chain(&chain));
        let _ = packet;
        let _ = packet2;
    }

    #[test]
    fn resolve_delivery_target_dotted_and_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = config_with(&tmp, ratification_on());
        config.memory.ratification.principal_transport = "telegram.work".into();
        assert_eq!(
            resolve_delivery_target(&config)
                .map(|(channel, target)| (channel, target.as_str().to_string())),
            Some(("telegram.work".to_string(), "operator-1".to_string())),
            "dotted transport resolves verbatim"
        );
        config.memory.ratification.principal_transport = "telegram".into();
        config.channels.telegram.clear();
        assert!(
            resolve_delivery_target(&config).is_none(),
            "bare transport without configured alias refuses delivery"
        );
        config.channels.telegram.insert(
            "work".into(),
            zeroclaw_config::schema::TelegramConfig::default(),
        );
        assert_eq!(
            resolve_delivery_target(&config).map(|(channel, _)| channel),
            Some("telegram.work".to_string())
        );
    }

    #[tokio::test]
    async fn worker_tick_assembles_correction_packet_immediately() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = config_with(&tmp, ratification_on());
        config.memory.ratification.principal_transport = "telegram.work".into();
        let conn = db_conn(&tmp);
        let corr = RatificationProposal::new(
            "correction",
            "operator correction",
            "highest",
            proposals::source_hash(&["c", "1"]),
        );
        proposals::propose(&conn, &corr).unwrap();

        let delivered = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = recording_sink(delivered.clone());
        let pending = worker_tick_sync(&config, &conn).unwrap();
        assert_eq!(pending.len(), 1, "correction packet assembled immediately");
        flush_deliveries(&config, pending, &*sink).await;

        // Сборка и доставка состоялись немедленно (событийный триггер).
        let texts = delivered.lock().unwrap();
        assert_eq!(texts.len(), 1, "correction packet delivered immediately");
        assert!(texts[0].contains("operator correction"));
        assert!(
            texts[0].contains("утверждаю"),
            "packet carries the sign instruction"
        );
        drop(texts);
        assert!(
            proposals::undelivered_packets(&conn).unwrap().is_empty(),
            "delivered packet must not be pending for retry"
        );
    }

    #[tokio::test]
    async fn worker_tick_delivery_failure_leaves_packet_pending_for_retry() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = config_with(&tmp, ratification_on());
        config.memory.ratification.principal_transport = "telegram.work".into();
        let conn = db_conn(&tmp);
        let corr = RatificationProposal::new(
            "correction",
            "operator correction",
            "highest",
            proposals::source_hash(&["c", "retry"]),
        );
        proposals::propose(&conn, &corr).unwrap();

        // Sink, который падает: пакет собран, но остаётся pending.
        let sink: std::sync::Arc<DeliverSink> = std::sync::Arc::new(
            |_: &str, _: &str, _: &str| -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<()>> + Send>,
            > { Box::pin(async { anyhow::bail!("synthetic delivery failure") }) },
        );
        let pending = worker_tick_sync(&config, &conn).unwrap();
        assert_eq!(pending.len(), 1);
        flush_deliveries(&config, pending, &*sink).await;

        assert_eq!(
            proposals::undelivered_packets(&conn).unwrap().len(),
            1,
            "failed delivery stays pending for scheduled retry"
        );
    }

    #[tokio::test]
    async fn worker_tick_calendar_phase_drains_graph_feeder() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = config_with(&tmp, ratification_on());
        config.knowledge.enabled = true;
        config.knowledge.auto_capture = true;
        config.knowledge.db_path = tmp.path().join("knowledge.db").display().to_string();
        let conn = db_conn(&tmp);

        // Один accepted-исход: proposal → пакет → доставка → подпись.
        let outcome = RatificationProposal::new(
            "trend",
            "deploys drift to Tuesdays",
            "normal",
            proposals::source_hash(&["g", "1"]),
        );
        let id = proposals::propose(&conn, &outcome).unwrap().proposal_id;
        let packet = proposals::assemble_packet(&conn, "pkt-g", &[id], 20)
            .unwrap()
            .unwrap();
        proposals::mark_delivered(&conn, "pkt-g").unwrap();
        proposals::apply_signed(
            &conn,
            &config.memory.ratification,
            &packet,
            "operator-1",
            &[],
            None,
        )
        .unwrap();

        let pending = worker_tick_sync(&config, &conn).unwrap();
        assert!(pending.is_empty(), "no deliveries without transport");

        // Фидер отработал в календарной фазе: узел в графе, метка продвинута.
        let graph = zeroclaw_memory::knowledge_graph::KnowledgeGraph::new(
            &config.knowledge.resolved_db_path(),
            config.knowledge.max_nodes,
        )
        .unwrap();
        let stats = graph.stats().unwrap();
        assert_eq!(stats.total_nodes, 1, "accepted outcome must feed one node");
        assert_eq!(
            zeroclaw_memory::graph_feeder::stats_at(tmp.path()).last_id,
            id,
            "feeder watermark advanced past the accepted outcome"
        );
    }

    #[tokio::test]
    async fn worker_tick_skips_feeder_when_auto_capture_off() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config_with(&tmp, ratification_on());
        // auto_capture выключен (дефолт): фидер молчит даже с accepted.
        let conn = db_conn(&tmp);
        let outcome = RatificationProposal::new(
            "trend",
            "some ratified fact",
            "normal",
            proposals::source_hash(&["g", "off"]),
        );
        let id = proposals::propose(&conn, &outcome).unwrap().proposal_id;
        let packet = proposals::assemble_packet(&conn, "pkt-go", &[id], 20)
            .unwrap()
            .unwrap();
        proposals::mark_delivered(&conn, "pkt-go").unwrap();
        proposals::apply_signed(
            &conn,
            &config.memory.ratification,
            &packet,
            "operator-1",
            &[],
            None,
        )
        .unwrap();

        let _ = worker_tick_sync(&config, &conn).unwrap();
        let db = tmp.path().join("knowledge.db");
        assert!(
            !db.exists(),
            "feeder state/db must not appear with auto_capture off"
        );
    }

    #[test]
    fn delivery_includes_graph_mirror_context_for_revision_packet() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = config_with(&tmp, ratification_on());
        config.memory.ratification.principal_transport = "telegram.work".into();
        config.knowledge.auto_capture = true;
        config.knowledge.db_path = tmp.path().join("knowledge.db").display().to_string();

        // Узел графа, резолвленный из target'а revision-операции.
        let graph = zeroclaw_memory::knowledge_graph::KnowledgeGraph::new(
            &config.knowledge.resolved_db_path(),
            config.knowledge.max_nodes,
        )
        .unwrap();
        let node_id = graph
            .add_node_feed(
                zeroclaw_memory::knowledge_graph::NodeType::Lesson,
                "deploys happen on Mondays",
                "old ratified fact",
                &["ratified".to_string()],
                Some("operator-1"),
                "mirror-test-hash",
            )
            .unwrap()
            .unwrap();
        graph
            .add_provenance(&node_id, "memory", "core_target")
            .unwrap();

        let packet = Packet {
            packet_id: "pkt-mirror".into(),
            items: vec![proposals::PacketItem {
                proposal_id: 7,
                position: 1,
                kind: "revision".into(),
                // Payload — строгий JSON revision-операций (массив, форма apply_signed).
                payload: serde_json::json!([{
                    "action": "supersede",
                    "target": "core_target",
                    "content": "deploys happen on Tuesdays",
                    "rationale": "two fresh episodes contradict Monday",
                    "refs": ["daily_x"],
                }])
                .to_string(),
                priority: "normal".into(),
            }],
            throttled: 0,
        };
        let delivery = pending_for_packet(&config, &packet)
            .expect("dotted transport resolves without aliases");
        let header = crate::i18n::get_required_cli_string("cli-ratification-graph-mirror-header");
        assert!(
            delivery.text.contains(&header),
            "revision packet must carry the touched subgraph, got:\n{}",
            delivery.text
        );
        assert!(
            delivery.text.contains("deploys happen on Mondays"),
            "mirror must name the affected node"
        );
    }

    #[test]
    fn delivery_omits_graph_mirror_context_when_auto_capture_off() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = config_with(&tmp, ratification_on());
        config.memory.ratification.principal_transport = "telegram.work".into();
        // auto_capture выключен (дефолт): байт-в-байт — секции зеркала нет,
        // knowledge.db не создаётся.
        config.knowledge.db_path = tmp.path().join("knowledge.db").display().to_string();
        let packet = Packet {
            packet_id: "pkt-mirror-off".into(),
            items: vec![proposals::PacketItem {
                proposal_id: 7,
                position: 1,
                kind: "revision".into(),
                payload: serde_json::json!([{
                    "action": "supersede",
                    "target": "core_target",
                    "content": "v2",
                    "rationale": "r",
                    "refs": [],
                }])
                .to_string(),
                priority: "normal".into(),
            }],
            throttled: 0,
        };
        let delivery = pending_for_packet(&config, &packet)
            .expect("dotted transport resolves without aliases");
        let header = crate::i18n::get_required_cli_string("cli-ratification-graph-mirror-header");
        assert!(
            !delivery.text.contains(&header),
            "mirror section must be absent with auto_capture off"
        );
        assert!(!tmp.path().join("knowledge.db").exists());
    }

    #[tokio::test]
    async fn worker_tick_empty_queue_assigns_no_packets() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config_with(&tmp, ratification_on());
        let conn = db_conn(&tmp);
        let delivered = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = recording_sink(delivered.clone());
        let pending = worker_tick_sync(&config, &conn).unwrap();
        assert!(pending.is_empty(), "no packets without candidates");
        flush_deliveries(&config, pending, &*sink).await;
        assert!(
            delivered.lock().unwrap().is_empty(),
            "empty calendar packet must not be delivered"
        );
    }

    fn recording_sink(
        hit: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) -> std::sync::Arc<DeliverSink> {
        std::sync::Arc::new(
            move |_: &str, _: &str, text: &str| -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<()>> + Send>,
            > {
                let hit = hit.clone();
                let text = text.to_string();
                Box::pin(async move {
                    hit.lock().unwrap().push(text);
                    Ok(())
                })
            },
        )
    }

    #[test]
    fn render_packet_lists_items_with_sign_instruction() {
        let packet = Packet {
            packet_id: "pkt-render".into(),
            items: vec![
                proposals::PacketItem {
                    proposal_id: 1,
                    position: 1,
                    kind: "trend".into(),
                    payload: "deploys drift to Mondays".into(),
                    priority: "normal".into(),
                },
                proposals::PacketItem {
                    proposal_id: 2,
                    position: 2,
                    kind: "correction".into(),
                    payload: "Operator correction: неправильно".into(),
                    priority: "highest".into(),
                },
            ],
            throttled: 1,
        };
        let text = render_packet(&packet);
        assert!(text.contains("pkt-render"));
        assert!(text.contains("1. [trend] deploys drift to Mondays"));
        assert!(text.contains("2. [correction] Operator correction: неправильно"));
        assert!(text.contains("утверждаю"), "sign instruction present");
        assert!(
            text.contains("Held back by weekly budget"),
            "throttled report present"
        );
    }

    #[tokio::test]
    async fn worker_tick_silent_correction_from_file_event() {
        let tmp = tempfile::tempdir().unwrap();
        let conn = db_conn(&tmp);

        // Файл, изменённый ПОСЛЕ журнальной метки агентской записи.
        let file_path = tmp.path().join("SOUL.md");
        std::fs::write(&file_path, "agent version").unwrap();
        proposals::record_file_event(
            &conn,
            file_path.to_string_lossy().as_ref(),
            Some("agent-a"),
            Some("turn-1"),
        )
        .unwrap();
        // mtime новее метки события → «операторская» правка (правка ПОСЛЕ
        // агентской записи; filetime допускает будущее время).
        let future = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
        filetime::set_file_mtime(&file_path, filetime::FileTime::from_system_time(future)).unwrap();

        let stats_before = proposals::counts(&conn).unwrap();
        let proposed = generate_silent_corrections(&conn).unwrap();
        assert_eq!(proposed, 1);
        let _ = stats_before;
        let stats = proposals::counts(&conn).unwrap();
        assert_eq!(stats.pending, 1);
        let kind: String = conn
            .query_row("SELECT kind FROM proposals", [], |row| row.get(0))
            .unwrap();
        assert_eq!(kind, "correction");
    }

    #[tokio::test]
    async fn silent_correction_ignores_repeated_agent_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let conn = db_conn(&tmp);

        let file_path = tmp.path().join("SOUL.md");
        std::fs::write(&file_path, "agent v1").unwrap();
        proposals::record_file_event(
            &conn,
            file_path.to_string_lossy().as_ref(),
            Some("agent-a"),
            Some("turn-1"),
        )
        .unwrap();
        // Вторая агентская запись: mtime обновился, но это НЕ правка оператора.
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(&file_path, "agent v2").unwrap();
        proposals::record_file_event(
            &conn,
            file_path.to_string_lossy().as_ref(),
            Some("agent-a"),
            Some("turn-2"),
        )
        .unwrap();

        let proposed = generate_silent_corrections(&conn).unwrap();
        assert_eq!(
            proposed, 0,
            "repeated agent writes must not look like operator edits"
        );
    }

    #[test]
    fn journal_agent_file_write_without_registration_is_noop() {
        // Без регистрации (фича выкл) — no-op, файл не создаётся.
        journal_agent_file_write("some/path.md", "turn-x", Some("a"));
    }
}
