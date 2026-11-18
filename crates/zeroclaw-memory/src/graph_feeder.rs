//! Граф-фидер (epic 9-5, контур RAM): наполнение графа знаний из
//! accepted-исходов ратификации 9-4 — без единого LLM-вызова.
//!
//! Единственный производитель узлов графа: accepted proposals → узлы
//! `decision`/`lesson` с provenance-ссылками на episode key; supersede-
//! цепочки исходов → рёбра `replaces` + temporal-инвалидация (non-destructive,
//! reuse би-темпоральной ДНК схемы). `knowledge.auto_capture` — главный ключ
//! фидера (ранее мёртвый конфиг-ключ, читается здесь впервые); gate полный:
//! `knowledge.auto_capture && ratification.enabled` (worker не стартует при
//! выключенной ратификации — байт-в-байт прежнее поведение).
//!
//! Момент записи — календарная фаза ratification-worker'а (батчем, ёмко),
//! не онлайн-путь поворота. Дельта по state-метке `graph_feeder_last_id`
//! (state-file в data_dir, паттерн ratification/dream worker'ов).
//! Идемпотентность двойная: метка дельты + UNIQUE source_hash узла.
//!
//! Запись только из подписанных исходов (status='accepted'): pending/rejected/
//! expired proposals не читаются. max_nodes — pre-check до записи: отказ с
//! WARN, ложного receipt нет, отказ копится в счётчике stats; метка НЕ
//! продвигается за отказ — необработанное доедет следующим прогоном.

use std::path::{Path, PathBuf};

use anyhow::Result;
use rusqlite::{Connection, params};
use zeroclaw_api::memory_traits::RatificationProposal;

use crate::knowledge_graph::{KnowledgeGraph, NodeType};
use crate::proposals::{self, STATUS_ACCEPTED, source_hash};

/// State-file фидера в data_dir (паттерн ratification/dream worker'ов).
pub const GRAPH_FEEDER_STATE_FILE: &str = "graph_feeder_state.json";

/// Длина title узла из payload (узел — карточка исхода, не простыня).
const TITLE_MAX_CHARS: usize = 80;

/// Состояние фидера: дельта-метка и накопленный счётчик отказов по max_nodes.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FeederState {
    /// Последний обработанный id accepted-предложения.
    pub last_id: i64,
    /// Сколько раз запись была отказана по max_nodes (накопительно).
    pub refused_max_nodes: u64,
}

fn state_path(data_dir: &Path) -> PathBuf {
    data_dir.join(GRAPH_FEEDER_STATE_FILE)
}

/// Загрузить состояние; файла нет/мусор — пустое (первый прогон начнёт с дельты 0).
#[must_use]
pub fn load_state(data_dir: &Path) -> FeederState {
    std::fs::read_to_string(state_path(data_dir))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_state(data_dir: &Path, state: &FeederState) -> Result<()> {
    let path = state_path(data_dir);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string(state)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Сводка для `memory stats` (CLI читает state-file напрямую).
#[must_use]
pub fn stats_at(data_dir: &Path) -> FeederState {
    load_state(data_dir)
}

/// Исход одного прогона фидера (честный отчёт: считается только реально
/// записанное — ложного receipt нет по построению).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FeedReport {
    /// Сколько accepted-исходов вошло в окно дельты.
    pub scanned: u64,
    /// Новые узлы (source_hash ранее не встречался).
    pub nodes_added: u64,
    /// Новые рёбра `replaces`.
    pub edges_added: u64,
    /// Исходы, уже представленные в графе (идемпотентный skip).
    pub duplicates_skipped: u64,
    /// Отказы по max_nodes за этот прогон.
    pub refused_max_nodes: u64,
    /// Метка дельты после прогона (продвинута только за обработанные).
    pub last_id: i64,
}

/// Одна accepted-запись окна дельты.
struct AcceptedOutcome {
    id: i64,
    agent: Option<String>,
    kind: String,
    payload: String,
    refs: Vec<String>,
}

/// Выборка accepted-исходов строго после метки (подписанное — единственное
/// сырьё фидера; pending/rejected/expired не читаются).
fn accepted_after(conn: &Connection, last_id: i64) -> Result<Vec<AcceptedOutcome>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent, kind, payload FROM proposals \
         WHERE status = ?1 AND id > ?2 ORDER BY id",
    )?;
    let rows = stmt
        .query_map(params![STATUS_ACCEPTED, last_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<Result<Vec<(i64, Option<String>, String, String)>, _>>()?;
    drop(stmt);

    let mut outcomes = Vec::with_capacity(rows.len());
    for (id, agent, kind, payload) in rows {
        let mut refs_stmt = conn.prepare(
            "SELECT ref_key FROM proposal_refs WHERE proposal_id = ?1 ORDER BY position, id",
        )?;
        let refs = refs_stmt
            .query_map(params![id], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        outcomes.push(AcceptedOutcome {
            id,
            agent,
            kind,
            payload,
            refs,
        });
    }
    Ok(outcomes)
}

/// Маппинг accepted-исхода в узел графа: kind=revision → `decision`
/// (payload — строгий JSON RevisionOp 9-6, target — supersede-цель),
/// остальные kind (trend/correction/contradiction/expensive_episode) →
/// `lesson`. source_hash детерминирован от (kind, payload) — повторная
/// подача того же исхода схлопывается в idempotent skip.
fn map_outcome(outcome: &AcceptedOutcome) -> (NodeType, String, String, Option<String>) {
    if outcome.kind == RatificationProposal::KIND_REVISION {
        match proposals::parse_revision_ops(&outcome.payload, 1) {
            Ok(mut ops) if !ops.is_empty() => {
                let op = ops.remove(0);
                let label = op.target.clone().unwrap_or_else(|| op.rationale.clone());
                let title = truncate_title(&format!("[{}] {}", op.action, label));
                let memory_key = format!("dream_{}_{}", op.action, outcome.id);
                (
                    NodeType::Decision,
                    title,
                    outcome.payload.clone(),
                    Some(memory_key),
                )
            }
            _ => (
                NodeType::Lesson,
                truncate_title(&outcome.payload),
                outcome.payload.clone(),
                None,
            ),
        }
    } else {
        (
            NodeType::Lesson,
            truncate_title(&outcome.payload),
            outcome.payload.clone(),
            None,
        )
    }
}

/// Supersede-цель revision-исхода (memory key затронутой core-записи).
fn revision_target(payload: &str) -> Option<String> {
    proposals::parse_revision_ops(payload, 1)
        .ok()
        .and_then(|mut ops| ops.pop())
        .and_then(|op| op.target)
}

fn truncate_title(text: &str) -> String {
    let single_line = text.lines().next().unwrap_or(text).trim();
    let mut title: String = single_line.chars().take(TITLE_MAX_CHARS).collect();
    if single_line.chars().count() > TITLE_MAX_CHARS {
        title.push('…');
    }
    title
}

/// Один прогон фидера: accepted-исходы после метки → узлы/рёбра графа,
/// метка и счётчик отказов — в state-file. Гейт флага — на caller'е
/// (worker проверяет `knowledge.auto_capture` до вызова; здесь флага нет —
/// чистая функция над данными, тестируемая без конфига).
pub fn drain(
    brain: &Connection,
    graph: &KnowledgeGraph,
    data_dir: &Path,
    max_nodes: usize,
) -> Result<FeedReport> {
    let mut state = load_state(data_dir);
    let window = accepted_after(brain, state.last_id)?;

    let mut report = FeedReport {
        scanned: window.len() as u64,
        last_id: state.last_id,
        ..FeedReport::default()
    };

    for outcome in &window {
        // max_nodes — pre-check ДО записи: отказ с WARN, метка не продвигается
        // за отказ, необработанное доедет следующим прогоном.
        if graph.node_count() >= max_nodes {
            report.refused_max_nodes += 1;
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({
                        "proposal_id": outcome.id,
                        "max_nodes": max_nodes,
                    })),
                "graph feeder refused write: max_nodes reached"
            );
            break;
        }

        let (node_type, title, content, memory_key) = map_outcome(outcome);
        let hash = source_hash(&["graph-feed", &outcome.kind, &outcome.payload]);
        let node_id = match graph.add_node_feed(
            node_type,
            &title,
            &content,
            &["ratified".to_string(), outcome.kind.clone()],
            outcome.agent.as_deref(),
            &hash,
        )? {
            Some(id) => id,
            None => {
                report.duplicates_skipped += 1;
                report.last_id = outcome.id;
                continue;
            }
        };
        report.nodes_added += 1;

        // Provenance: таблица ссылок, не json — episode key обязателен
        // (докопываемость узла → исход ратификации), дальше — следы.
        let episode_key = format!("ratification_accepted_{}", outcome.id);
        graph.add_provenance(&node_id, "episode", &episode_key)?;
        graph.add_provenance(&node_id, "proposal", &outcome.id.to_string())?;
        if let Some(key) = &memory_key {
            graph.add_provenance(&node_id, "memory", key)?;
        }
        for reference in &outcome.refs {
            graph.add_provenance(&node_id, "ref", reference)?;
        }

        // Supersession: revision-исход с target → ребро replaces к узлам,
        // затронутым target (resolution через provenance episode/memory).
        if let Some(target) = revision_target(&outcome.payload) {
            let mut old_ids = graph.nodes_by_provenance("episode", &target)?;
            old_ids.extend(graph.nodes_by_provenance("memory", &target)?);
            for old_id in old_ids {
                if old_id == node_id {
                    continue;
                }
                graph.add_replaces_edge(&node_id, &old_id)?;
                report.edges_added += 1;
            }
        }

        report.last_id = outcome.id;
    }

    state.last_id = report.last_id;
    state.refused_max_nodes += report.refused_max_nodes;
    save_state(data_dir, &state)?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge_graph::Relation;
    use tempfile::TempDir;

    /// brain.db с таблицами proposals (DDL через SqliteMemory); knowledge.db —
    /// отдельный файл в том же tempdir.
    fn fixture() -> (TempDir, Connection, KnowledgeGraph) {
        let tmp = TempDir::new().unwrap();
        let mem = crate::sqlite::SqliteMemory::new("test", tmp.path()).unwrap();
        drop(mem);
        let brain = Connection::open(crate::proposals::proposals_db_path(tmp.path())).unwrap();
        let graph = KnowledgeGraph::new(&tmp.path().join("knowledge.db"), 1000).unwrap();
        (tmp, brain, graph)
    }

    fn accept(brain: &Connection, kind: &str, payload: &str, agent: Option<&str>) -> i64 {
        brain
            .execute(
                "INSERT INTO proposals (agent, kind, payload, priority, status, source_hash, created_at, resolved_at) \
                 VALUES (?1, ?2, ?3, 'normal', ?4, ?5, ?6, ?6)",
                params![
                    agent,
                    kind,
                    payload,
                    STATUS_ACCEPTED,
                    proposals::source_hash(&["test", kind, payload]),
                    chrono::Local::now().to_rfc3339(),
                ],
            )
            .unwrap();
        brain.last_insert_rowid()
    }

    #[test]
    fn drain_maps_lesson_with_episode_provenance() {
        let (tmp, brain, graph) = fixture();
        let id = accept(&brain, "trend", "deploys drift to Mondays", Some("agent-a"));

        let report = drain(&brain, &graph, tmp.path(), 1000).unwrap();
        assert_eq!(report.scanned, 1);
        assert_eq!(report.nodes_added, 1);
        assert_eq!(report.last_id, id);

        let stats = graph.stats().unwrap();
        assert_eq!(stats.total_nodes, 1);
        assert_eq!(stats.nodes_by_type.get("lesson"), Some(&1));

        let nodes = graph.nodes_by_provenance("episode", &format!("ratification_accepted_{id}"));
        assert_eq!(nodes.unwrap().len(), 1);
        let node = graph
            .query_by_type(NodeType::Lesson, 10)
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(node.source_project.as_deref(), Some("agent-a"));
        assert!(node.tags.contains(&"ratified".to_string()));
    }

    #[test]
    fn drain_is_idempotent_by_source_hash() {
        let (tmp, brain, graph) = fixture();
        accept(&brain, "trend", "deploys drift to Mondays", None);

        let first = drain(&brain, &graph, tmp.path(), 1000).unwrap();
        assert_eq!(first.nodes_added, 1);

        // Сброс метки (restore/перенос) — source-hash всё равно схлопывает.
        save_state(tmp.path(), &FeederState::default()).unwrap();
        let second = drain(&brain, &graph, tmp.path(), 1000).unwrap();
        assert_eq!(second.scanned, 1);
        assert_eq!(second.nodes_added, 0);
        assert_eq!(second.duplicates_skipped, 1);
        assert_eq!(graph.stats().unwrap().total_nodes, 1);
    }

    #[test]
    fn drain_empty_queue_is_noop() {
        let (tmp, brain, graph) = fixture();
        let report = drain(&brain, &graph, tmp.path(), 1000).unwrap();
        assert_eq!(report.scanned, 0);
        assert_eq!(graph.stats().unwrap().total_nodes, 0);
        // State-file обновлён (пустой прогон — завершение, не побочных эффектов).
        assert!(state_path(tmp.path()).exists());
    }

    #[test]
    fn drain_builds_replaces_chain_for_revision_supersede() {
        let (tmp, brain, graph) = fixture();
        // Первый исход — обычный trend (узел-урок, episode key = его memory key).
        let first = accept(&brain, "trend", "deploys drift to Mondays", None);
        // Revision-исход суперседит episode key первого исхода (payload —
        // строгий JSON-список операций, форма apply_signed 9-6).
        let op = format!(
            r#"[{{"action":"supersede","target":"ratification_accepted_{first}","content":"deploys moved to Tuesdays","rationale":"two episodes contradict Monday deploys","refs":[]}}]"#
        );
        accept(&brain, "revision", &op, None);

        let report = drain(&brain, &graph, tmp.path(), 1000).unwrap();
        assert_eq!(report.nodes_added, 2);
        assert_eq!(report.edges_added, 1);

        // Ребро replaces от нового к старому; старый узел на месте.
        let stats = graph.stats().unwrap();
        assert_eq!(stats.total_nodes, 2);
        assert_eq!(stats.edges_by_relation.get("replaces"), Some(&1));
        assert_eq!(stats.replaces_chain_lengths, vec![2]);

        let old_nodes = graph
            .nodes_by_provenance("episode", &format!("ratification_accepted_{first}"))
            .unwrap();
        let inbound = graph.find_inbound(&old_nodes[0], 10).unwrap();
        assert!(inbound.iter().any(|(_, r)| *r == Relation::Replaces));
    }

    #[test]
    fn drain_prefers_decision_node_for_revision_kind() {
        let (tmp, brain, graph) = fixture();
        let op = r#"[{"action":"promote","content":"new ratified fact","rationale":"fresh episodes confirm","refs":["ep-1"]}]"#;
        accept(&brain, "revision", op, None);

        let report = drain(&brain, &graph, tmp.path(), 1000).unwrap();
        assert_eq!(report.nodes_added, 1);
        let stats = graph.stats().unwrap();
        assert_eq!(stats.nodes_by_type.get("decision"), Some(&1));

        // memory-key provenance: позиция адресуется последующими revision-операциями.
        let nodes = graph.nodes_by_provenance("memory", "dream_promote_1");
        assert_eq!(nodes.unwrap().len(), 1);
    }

    #[test]
    fn drain_refuses_past_max_nodes_without_false_receipt() {
        let (tmp, brain, graph) = fixture();
        accept(&brain, "trend", "first fact", None);
        accept(&brain, "trend", "second fact", None);

        // Лимит 1: первый узел влезает, второй — отказ до записи.
        let report = drain(&brain, &graph, tmp.path(), 1).unwrap();
        assert_eq!(report.nodes_added, 1);
        assert_eq!(report.refused_max_nodes, 1);
        assert_eq!(graph.stats().unwrap().total_nodes, 1);

        // Метка остановилась за отказом: поднятие лимита доедает остаток.
        let report = drain(&brain, &graph, tmp.path(), 10).unwrap();
        assert_eq!(report.nodes_added, 1);
        assert_eq!(graph.stats().unwrap().total_nodes, 2);

        // Счётчик отказов накопительный в state-file (сырьё stats).
        assert_eq!(load_state(tmp.path()).refused_max_nodes, 1);
    }

    #[test]
    fn drain_skips_pending_and_rejected_outcomes() {
        let (tmp, brain, graph) = fixture();
        brain
            .execute(
                "INSERT INTO proposals (kind, payload, priority, status, source_hash, created_at) \
                 VALUES ('trend', 'pending noise', 'normal', 'pending', 'h1', '2026-11-18T00:00:00Z')",
                [],
            )
            .unwrap();
        accept(&brain, "trend", "signed fact", None);
        brain
            .execute(
                "UPDATE proposals SET status = 'rejected' WHERE payload = 'signed fact'",
                [],
            )
            .unwrap();
        // Оба не accepted — в граф ничего не попадает.
        let report = drain(&brain, &graph, tmp.path(), 1000).unwrap();
        assert_eq!(report.scanned, 0);
        assert_eq!(graph.stats().unwrap().total_nodes, 0);
    }
}
