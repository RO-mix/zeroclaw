use super::traits::{Memory, MemoryCategory};
use super::{
    MemoryBackendKind, backend_kind_from_dotted, classify_memory_backend,
    create_memory_for_migration, create_memory_from_config,
};
use crate::config::Config;
use anyhow::{Result, bail};
use console::style;
#[cfg(feature = "agent-runtime")]
use zeroclaw_runtime::i18n;

/// Resolve a `cli-*` Fluent key for memory CLI output. Under `agent-runtime`
/// (default, and what CI/release build) this routes through Fluent; without it
/// the runtime i18n crate is absent, so the English `fallback` is used.
#[allow(unused_variables)]
fn mt(key: &str, fallback: &str) -> String {
    #[cfg(feature = "agent-runtime")]
    {
        i18n::get_required_cli_string(key)
    }
    #[cfg(not(feature = "agent-runtime"))]
    {
        fallback.to_string() // i18n-exempt: English fallback when Fluent (agent-runtime) is disabled
    }
}

/// `mt` with `{$name}` arguments.
#[allow(unused_variables)]
fn mt_args(key: &str, args: &[(&str, &str)], fallback: &str) -> String {
    #[cfg(feature = "agent-runtime")]
    {
        i18n::get_required_cli_string_with_args(key, args)
    }
    #[cfg(not(feature = "agent-runtime"))]
    {
        fallback.to_string() // i18n-exempt: English fallback when Fluent (agent-runtime) is disabled
    }
}

/// Handle `zeroclaw memory <subcommand>` CLI commands.
pub async fn handle_command(command: crate::MemoryCommands, config: &Config) -> Result<()> {
    match command {
        crate::MemoryCommands::List {
            category,
            session,
            limit,
            offset,
        } => handle_list(config, category, session, limit, offset).await,
        crate::MemoryCommands::Get { key } => handle_get(config, &key).await,
        crate::MemoryCommands::Stats => handle_stats(config).await,
        crate::MemoryCommands::Clear { key, category, yes } => {
            handle_clear(config, key, category, yes).await
        }
        crate::MemoryCommands::Reindex => handle_reindex(config).await,
    }
}

fn create_memory_with_embedder(config: &Config) -> Result<Box<dyn Memory>> {
    let backend = backend_kind_from_dotted(&config.memory.backend);
    if matches!(classify_memory_backend(&backend), MemoryBackendKind::None) {
        bail!("Memory backend is 'none' (disabled). No entries to manage.");
    }
    create_memory_from_config(config, None)
}

async fn handle_reindex(config: &Config) -> Result<()> {
    let mem = create_memory_with_embedder(config)?;
    println!(
        "{} {}",
        style("→").cyan(),
        mt("cli-memory-reindexing", "Reindexing memory backend...")
    );
    let count = mem.reindex().await?;
    if count == 0 {
        println!(
            "{} FTS rebuilt. No embeddings to fill in (either everything is already embedded or the backend has no embedder configured).",
            style("✓").green()
        );
    } else {
        println!(
            "{} FTS rebuilt. Re-embedded {count} {}.",
            style("✓").green(),
            if count == 1 { "entry" } else { "entries" }
        );
    }
    Ok(())
}

fn create_cli_memory(config: &Config) -> Result<Box<dyn Memory>> {
    let backend = backend_kind_from_dotted(&config.memory.backend);

    match classify_memory_backend(&backend) {
        MemoryBackendKind::None => {
            bail!("Memory backend is 'none' (disabled). No entries to manage.");
        }
        _ => create_memory_for_migration(config),
    }
}

async fn handle_list(
    config: &Config,
    category: Option<String>,
    session: Option<String>,
    limit: usize,
    offset: usize,
) -> Result<()> {
    let mem = create_cli_memory(config)?;
    let cat = category.as_deref().map(parse_category);
    let entries = mem.list(cat.as_ref(), session.as_deref()).await?;

    if entries.is_empty() {
        println!("{}", mt("cli-memory-none", "No memory entries found."));
        return Ok(());
    }

    let total = entries.len();
    let page: Vec<_> = entries.into_iter().skip(offset).take(limit).collect();

    if page.is_empty() {
        println!(
            "{}",
            mt_args(
                "cli-memory-none-at-offset",
                &[
                    ("offset", &offset.to_string()),
                    ("total", &total.to_string())
                ],
                "No entries at offset"
            )
        );
        return Ok(());
    }

    println!(
        "Memory entries ({total} total, showing {}-{}):\n",
        offset + 1,
        offset + page.len(),
    );

    for entry in &page {
        println!(
            "- {} [{}]",
            style(&entry.key).white().bold(),
            entry.category,
        );
        println!("    {}", truncate_content(&entry.content, 80));
    }

    if offset + page.len() < total {
        println!(
            "\n{}",
            mt_args(
                "cli-memory-next-page",
                &[("offset", &(offset + limit).to_string())],
                "Use --offset to see the next page"
            )
        );
    }

    Ok(())
}

async fn handle_get(config: &Config, key: &str) -> Result<()> {
    let mem = create_cli_memory(config)?;

    // Try exact match first.
    if let Some(entry) = mem.get(key).await? {
        print_entry(&entry);
        return Ok(());
    }

    // Fall back to prefix match so users can copy partial keys from `list`.
    let all = mem.list(None, None).await?;
    let matches: Vec<_> = all.iter().filter(|e| e.key.starts_with(key)).collect();

    match matches.len() {
        0 => println!(
            "{}",
            mt_args(
                "cli-memory-key-not-found",
                &[("key", key)],
                "No memory entry found for key"
            )
        ),
        1 => print_entry(matches[0]),
        n => {
            println!(
                "{}\n",
                mt_args(
                    "cli-memory-prefix-matched",
                    &[("key", key), ("n", &n.to_string())],
                    "Prefix matched entries"
                )
            );
            for entry in matches {
                println!(
                    "- {} [{}]",
                    style(&entry.key).white().bold(),
                    entry.category
                );
            }
            println!(
                "\n{}",
                mt(
                    "cli-memory-narrow-prefix",
                    "Specify a longer prefix to narrow the match."
                )
            );
        }
    }

    Ok(())
}

fn print_entry(entry: &super::traits::MemoryEntry) {
    println!(
        "{}",
        mt_args(
            "cli-memory-key",
            &[("value", &style(&entry.key).white().bold().to_string())],
            "Key"
        )
    );
    println!(
        "{}",
        mt_args(
            "cli-memory-category",
            &[("value", &entry.category.to_string())],
            "Category"
        )
    );
    println!(
        "{}",
        mt_args(
            "cli-memory-timestamp",
            &[("value", &entry.timestamp.to_string())],
            "Timestamp"
        )
    );
    if let Some(sid) = &entry.session_id {
        println!(
            "{}",
            mt_args("cli-memory-session", &[("value", sid)], "Session")
        );
    }
    println!("\n{}", entry.content);
}

async fn handle_stats(config: &Config) -> Result<()> {
    let mem = create_cli_memory(config)?;
    let healthy = mem.health_check().await;
    let total = mem.count().await.unwrap_or(0);

    println!("{}\n", mt("cli-memory-stats-header", "Memory Statistics:"));
    println!(
        "{}",
        mt_args(
            "cli-memory-backend",
            &[("value", &style(mem.name()).white().bold().to_string())],
            "Backend"
        )
    );
    println!(
        "  Health:   {}",
        if healthy {
            style("healthy").green().bold().to_string()
        } else {
            style("unhealthy").yellow().bold().to_string()
        }
    );
    println!(
        "{}",
        mt_args(
            "cli-memory-total",
            &[("value", &total.to_string())],
            "Total"
        )
    );

    let all = mem.list(None, None).await.unwrap_or_default();
    if !all.is_empty() {
        let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for entry in &all {
            *counts.entry(entry.category.to_string()).or_default() += 1;
        }

        println!("\n{}", mt("cli-memory-by-category", "  By category:"));
        let mut sorted: Vec<_> = counts.into_iter().collect();
        sorted.sort_by_key(|entry| std::cmp::Reverse(entry.1));
        for (cat, count) in sorted {
            println!("    {cat:<20} {count}");
        }
    }

    // Self-change receipts и drift-метрика (epic 9-3, OQ-3 A): сырьё Д0 —
    // правки narrative/judgment без проекции на ось. Только sqlite-бэкенд:
    // таблица self_change_receipts живёт в brain.db.
    if mem.name() == "sqlite" {
        let stats = zeroclaw_memory::receipts::self_change_receipt_stats_at(
            &zeroclaw_memory::receipts::brain_db_path(&config.data_dir),
        )?;
        let total = stats.total.to_string();
        let grounded = stats.grounded.to_string();
        let contested = stats.contested.to_string();
        let drift = stats.drift.to_string();
        let fallback_line = format!(
            "  Self-change receipts: {total} (grounded {grounded}, contested {contested}, drift {drift})"
        );
        println!(
            "{}",
            mt_args(
                "cli-memory-self-change",
                &[
                    ("total", total.as_str()),
                    ("grounded", grounded.as_str()),
                    ("contested", contested.as_str()),
                    ("drift", drift.as_str()),
                ],
                &fallback_line
            )
        );

        // Judgment-layer proposals (epic 9-4): очередь суждений по статусам.
        let proposals = zeroclaw_memory::proposals::counts_at(
            &zeroclaw_memory::receipts::brain_db_path(&config.data_dir),
        )?;
        let total = proposals.total().to_string();
        let pending = proposals.pending.to_string();
        let accepted = proposals.accepted.to_string();
        let rejected = proposals.rejected.to_string();
        let expired = proposals.expired.to_string();
        let fallback_line = format!(
            "  Proposals: {total} (pending {pending}, accepted {accepted}, rejected {rejected}, expired {expired})"
        );
        println!(
            "{}",
            mt_args(
                "cli-memory-proposals",
                &[
                    ("total", total.as_str()),
                    ("pending", pending.as_str()),
                    ("accepted", accepted.as_str()),
                    ("rejected", rejected.as_str()),
                    ("expired", expired.as_str()),
                ],
                &fallback_line
            )
        );

        // Телеметрия отказов гейта полномочий (epic 9-8, BH-13): denial
        // probing виден оператору. Счётчики производные (memory_meta), не
        // конфиг. Per-agent список отсортирован по убыванию count.
        let denials = zeroclaw_memory::receipts::denial_stats_at(
            &zeroclaw_memory::receipts::brain_db_path(&config.data_dir),
        )?;
        let total = denials.total.to_string();
        let agents = if denials.per_agent.is_empty() {
            "-".to_string()
        } else {
            denials
                .per_agent
                .iter()
                .map(|(alias, count)| format!("{alias}={count}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let fallback_line = format!("  Denials: {total} ({agents})");
        println!(
            "{}",
            mt_args(
                "cli-memory-denials",
                &[("total", total.as_str()), ("agents", agents.as_str())],
                &fallback_line
            )
        );

        // Dream cycle (epic 9-6): счётчики ночного прогона переоценки
        // суждений (per-agent; state-file читается напрямую).
        #[cfg(feature = "agent-runtime")]
        for (agent, agent_state) in
            &zeroclaw_runtime::agent::dream::stats_at(&config.data_dir).agents
        {
            let counters = &agent_state.counters;
            let runs = agent_state.run_count.to_string();
            let last_run = agent_state
                .last_run_at
                .clone()
                .unwrap_or_else(|| "-".to_string());
            let proposed = counters.revisions_proposed.to_string();
            let accepted = counters.revisions_accepted.to_string();
            let revalidations = counters.revalidations.to_string();
            let drift = counters.drift_events.to_string();
            let fallback_line = format!(
                "  Dream [{agent}]: runs {runs} last {last_run} (proposed {proposed}, accepted {accepted}, revalidations {revalidations}, drift {drift})"
            );
            println!(
                "{}",
                mt_args(
                    "cli-memory-dream",
                    &[
                        ("agent", agent.as_str()),
                        ("runs", runs.as_str()),
                        ("last_run", last_run.as_str()),
                        ("proposed", proposed.as_str()),
                        ("accepted", accepted.as_str()),
                        ("revalidations", revalidations.as_str()),
                        ("drift", drift.as_str()),
                    ],
                    &fallback_line
                )
            );
        }

        // Граф знаний (9-5): здоровье графа + метка фидера — материал
        // self-review (узлы/рёбра по типам, длины replaces-цепочек,
        // feed-отказы по max_nodes). Гейт — `knowledge.auto_capture`
        // (флаг выкл → секции нет, байт-в-байт); knowledge.db не
        // создаётся — без файла строка нулевая.
        if config.knowledge.auto_capture {
            let db_path = config.knowledge.resolved_db_path();
            let graph_line = if db_path.exists() {
                zeroclaw_memory::knowledge_graph::KnowledgeGraph::new(
                    &db_path,
                    config.knowledge.max_nodes,
                )
                .and_then(|graph| graph.stats())
                .map(|stats| {
                    let by_type = stats
                        .nodes_by_type
                        .iter()
                        .map(|(kind, count)| format!("{kind} {count}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let chains = if stats.replaces_chain_lengths.is_empty() {
                        "-".to_string()
                    } else {
                        stats
                            .replaces_chain_lengths
                            .iter()
                            .map(usize::to_string)
                            .collect::<Vec<_>>()
                            .join(",")
                    };
                    format!(
                        "{} nodes, {} edges ({by_type}); replaces chains: {chains}",
                        stats.total_nodes, stats.total_edges
                    )
                })
                .unwrap_or_else(|_| "unavailable".to_string())
            } else {
                "no knowledge.db yet".to_string()
            };
            let feeder = zeroclaw_memory::graph_feeder::stats_at(&config.data_dir);
            let feeder_line = format!(
                "feeder last accepted id {}, max_nodes refusals {}",
                feeder.last_id, feeder.refused_max_nodes
            );
            let fallback_line = format!("  Graph: {graph_line}; {feeder_line}");
            println!(
                "{}",
                mt_args(
                    "cli-memory-graph",
                    &[
                        ("graph", graph_line.as_str()),
                        ("feeder", feeder_line.as_str()),
                    ],
                    &fallback_line
                )
            );
        }
    }

    Ok(())
}

fn unsupported_clear_backend_message(backend: &str) -> String {
    #[cfg(feature = "agent-runtime")]
    {
        i18n::get_required_cli_string_with_args(
            "cli-memory-clear-unsupported-backend",
            &[("backend", backend)],
        )
    }

    #[cfg(not(feature = "agent-runtime"))]
    {
        format!(
            "memory clear is unsupported for append-only backend '{backend}'; switch to a deletable backend (sqlite, lucid, or postgres)"
        )
    }
}

async fn handle_clear(
    config: &Config,
    key: Option<String>,
    category: Option<String>,
    yes: bool,
) -> Result<()> {
    let backend = backend_kind_from_dotted(&config.memory.backend);
    if matches!(
        classify_memory_backend(&backend),
        MemoryBackendKind::Markdown | MemoryBackendKind::Qdrant
    ) {
        bail!(unsupported_clear_backend_message(&backend));
    }
    let mem = create_cli_memory(config)?;

    // Single-key deletion (exact or prefix match).
    if let Some(key) = key {
        return handle_clear_key(&*mem, &key, yes).await;
    }

    // Batch deletion by category (or all).
    let cat = category.as_deref().map(parse_category);
    let entries = mem.list(cat.as_ref(), None).await?;

    if entries.is_empty() {
        println!("{}", mt("cli-memory-none-to-clear", "No entries to clear."));
        return Ok(());
    }

    let scope = category.as_deref().unwrap_or("all categories");
    println!(
        "{}",
        mt_args(
            "cli-memory-found-in-scope",
            &[("count", &entries.len().to_string()), ("scope", scope)],
            "Found entries"
        )
    );

    if !yes {
        let confirmed = dialoguer::Confirm::new()
            .with_prompt(format!("  Delete {} entries?", entries.len()))
            .default(false)
            .interact()?;
        if !confirmed {
            println!("{}", mt("cli-memory-aborted", "Aborted."));
            return Ok(());
        }
    }

    let mut deleted = 0usize;
    for entry in &entries {
        if mem.forget(&entry.key).await? {
            deleted += 1;
        }
    }

    println!(
        "{} Cleared {deleted}/{} entries.",
        style("✓").green().bold(),
        entries.len(),
    );

    Ok(())
}

/// Delete a single entry by exact key or prefix match.
async fn handle_clear_key(mem: &dyn Memory, key: &str, yes: bool) -> Result<()> {
    // Resolve the target key (exact match or unique prefix).
    let target = if mem.get(key).await?.is_some() {
        key.to_string()
    } else {
        let all = mem.list(None, None).await?;
        let matches: Vec<_> = all.iter().filter(|e| e.key.starts_with(key)).collect();
        match matches.len() {
            0 => {
                println!(
                    "{}",
                    mt_args(
                        "cli-memory-key-not-found",
                        &[("key", key)],
                        "No memory entry found for key"
                    )
                );
                return Ok(());
            }
            1 => matches[0].key.clone(),
            n => {
                println!(
                    "{}\n",
                    mt_args(
                        "cli-memory-prefix-matched",
                        &[("key", key), ("n", &n.to_string())],
                        "Prefix matched entries"
                    )
                );
                for entry in matches {
                    println!(
                        "- {} [{}]",
                        style(&entry.key).white().bold(),
                        entry.category
                    );
                }
                println!(
                    "\n{}",
                    mt(
                        "cli-memory-narrow-prefix",
                        "Specify a longer prefix to narrow the match."
                    )
                );
                return Ok(());
            }
        }
    };

    if !yes {
        let confirmed = dialoguer::Confirm::new()
            .with_prompt(format!("  Delete '{target}'?"))
            .default(false)
            .interact()?;
        if !confirmed {
            println!("{}", mt("cli-memory-aborted", "Aborted."));
            return Ok(());
        }
    }

    if mem.forget(&target).await? {
        println!(
            "{} {}",
            style("✓").green().bold(),
            mt_args("cli-memory-deleted-key", &[("key", &target)], "Deleted key")
        );
    }

    Ok(())
}

fn parse_category(s: &str) -> MemoryCategory {
    match s.trim().to_ascii_lowercase().as_str() {
        "core" => MemoryCategory::Core,
        "daily" => MemoryCategory::Daily,
        "conversation" => MemoryCategory::Conversation,
        other => MemoryCategory::Custom(other.to_string()),
    }
}

fn truncate_content(s: &str, max_len: usize) -> String {
    let line = s.lines().next().unwrap_or(s);
    if line.len() <= max_len {
        return line.to_string();
    }
    let truncated: String = line.chars().take(max_len.saturating_sub(3)).collect();
    format!("{truncated}...")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[cfg(feature = "agent-runtime")]
    #[tokio::test]
    async fn list_does_not_run_hygiene_on_retention_protected_row() {
        use rusqlite::params;
        use zeroclaw_memory::SqliteMemory;

        let tmp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.data_dir = tmp.path().to_path_buf();
        config.memory.backend = "sqlite".into();
        config.memory.hygiene_enabled = true;
        config.memory.conversation_retention_days = 30;
        config
            .memory
            .policy
            .retention_days_by_category
            .insert("conversation".into(), 0);

        let seed = SqliteMemory::new("sqlite", tmp.path()).unwrap();
        seed.store(
            "protected",
            "an old conversation",
            MemoryCategory::Conversation,
            None,
        )
        .await
        .unwrap();
        drop(seed);
        let db_path = tmp.path().join("memory/brain.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "UPDATE memories SET updated_at = ?1 WHERE key = ?2",
            params!["2020-01-01T00:00:00+00:00", "protected"],
        )
        .unwrap();
        drop(conn);

        handle_list(&config, None, None, 10, 0).await.unwrap();

        let reopened = SqliteMemory::new("sqlite", tmp.path()).unwrap();
        assert!(reopened.get("protected").await.unwrap().is_some());
        assert!(
            !tmp.path().join("state/memory_hygiene_state.json").exists(),
            "CLI inspection must not start runtime hygiene"
        );
    }

    #[test]
    fn parse_category_known_variants() {
        assert_eq!(parse_category("core"), MemoryCategory::Core);
        assert_eq!(parse_category("daily"), MemoryCategory::Daily);
        assert_eq!(parse_category("conversation"), MemoryCategory::Conversation);
        assert_eq!(parse_category("CORE"), MemoryCategory::Core);
        assert_eq!(parse_category("  Daily  "), MemoryCategory::Daily);
    }

    #[test]
    fn parse_category_custom_fallback() {
        assert_eq!(
            parse_category("project_notes"),
            MemoryCategory::Custom("project_notes".into())
        );
    }

    #[test]
    fn truncate_content_short_text_unchanged() {
        assert_eq!(truncate_content("hello", 10), "hello");
    }

    #[test]
    fn truncate_content_long_text_truncated() {
        let result = truncate_content("this is a very long string", 10);
        assert!(result.ends_with("..."));
        assert!(result.chars().count() <= 10);
    }

    #[test]
    fn truncate_content_multiline_uses_first_line() {
        assert_eq!(truncate_content("first\nsecond", 20), "first");
    }

    #[test]
    fn truncate_content_empty_string() {
        assert_eq!(truncate_content("", 10), "");
    }

    #[cfg(feature = "agent-runtime")]
    #[tokio::test]
    async fn clear_yes_deletes_protected_narrative_with_permissions_enabled() {
        // Epic 9-3 I/O matrix: operator CLI is the second key. Even with the
        // permission matrix on, `memory clear <narrative key> --yes` destroys
        // the protected record — the gate never sees operator CLI calls.
        use zeroclaw_memory::SqliteMemory;
        use zeroclaw_memory::narrative::NARRATIVE_KEY;
        use zeroclaw_memory::traits::{MemoryKind, StoreOptions};

        let tmp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.data_dir = tmp.path().to_path_buf();
        config.memory.backend = "sqlite".into();
        config.memory.permissions.enabled = true;

        let seed = SqliteMemory::new("sqlite", tmp.path()).unwrap();
        seed.store_with_options(
            NARRATIVE_KEY,
            "ОБРАЗ v1",
            MemoryCategory::Core,
            None,
            StoreOptions {
                kind: Some(MemoryKind::Narrative),
                pinned: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        drop(seed);

        handle_command(
            crate::MemoryCommands::Clear {
                key: Some(NARRATIVE_KEY.into()),
                category: None,
                yes: true,
            },
            &config,
        )
        .await
        .unwrap();

        let reopened = SqliteMemory::new("sqlite", tmp.path()).unwrap();
        assert!(
            reopened.get(NARRATIVE_KEY).await.unwrap().is_none(),
            "operator CLI must destroy the protected narrative even with the matrix enabled"
        );
    }

    #[tokio::test]
    async fn clear_rejects_append_only_markdown_backend() {
        let tmp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.data_dir = tmp.path().to_path_buf();
        config.memory.backend = "markdown".into();

        let err = handle_command(
            crate::MemoryCommands::Clear {
                key: None,
                category: None,
                yes: true,
            },
            &config,
        )
        .await
        .unwrap_err();

        let msg = err.to_string();
        // The backend name is interpolated verbatim into the (localized) error,
        // so assert on the locale-stable name rather than the translated prose.
        assert!(msg.contains("'markdown'"), "got: {msg}");
    }

    #[tokio::test]
    async fn clear_rejects_qdrant_backend_constructed_as_markdown() {
        let tmp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.data_dir = tmp.path().to_path_buf();
        config.memory.backend = "qdrant".into();

        let err = handle_command(
            crate::MemoryCommands::Clear {
                key: None,
                category: None,
                yes: true,
            },
            &config,
        )
        .await
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("'qdrant'"), "got: {msg}");
    }

    #[tokio::test]
    async fn clear_rejects_dotted_qdrant_backend() {
        let tmp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.data_dir = tmp.path().to_path_buf();
        config.memory.backend = "qdrant.default".into();

        let err = handle_command(
            crate::MemoryCommands::Clear {
                key: None,
                category: None,
                yes: true,
            },
            &config,
        )
        .await
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("'qdrant'"), "got: {msg}");
    }

    #[cfg(feature = "agent-runtime")]
    #[tokio::test]
    async fn stats_prints_self_change_receipts_for_sqlite() {
        use zeroclaw_memory::SqliteMemory;
        use zeroclaw_memory::permissions::QuoteVerdict;
        use zeroclaw_memory::receipts::{
            brain_db_path, record_self_change_receipt_at, self_change_receipt_stats_at,
        };

        let tmp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.data_dir = tmp.path().to_path_buf();
        config.memory.backend = "sqlite".into();

        let seed = SqliteMemory::new("sqlite", tmp.path()).unwrap();
        seed.store("narrative", "ОБРАЗ v1", MemoryCategory::Core, None)
            .await
            .unwrap();
        drop(seed);

        let db = brain_db_path(tmp.path());
        record_self_change_receipt_at(
            &db,
            "test",
            "narrative",
            Some("ОБРАЗ"),
            QuoteVerdict::Grounded,
        )
        .unwrap();
        record_self_change_receipt_at(
            &db,
            "test",
            "narrative",
            None,
            QuoteVerdict::ContestedMissing,
        )
        .unwrap();

        handle_stats(&config).await.unwrap();

        // Соседние тесты stdout не захватывают; числа, которые печатает
        // ветка stats, проверяем косвенно через ту же сводку.
        let stats = self_change_receipt_stats_at(&db).unwrap();
        assert_eq!(stats.total, 2);
        assert_eq!(stats.grounded, 1);
        assert_eq!(stats.contested, 1);
        assert_eq!(stats.drift, 1, "drift копит contested-правки");
    }

    #[cfg(feature = "agent-runtime")]
    #[tokio::test]
    async fn stats_prints_proposals_for_sqlite() {
        use zeroclaw_memory::SqliteMemory;
        use zeroclaw_memory::proposals::{self, counts_at};
        use zeroclaw_memory::receipts::brain_db_path;

        let tmp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.data_dir = tmp.path().to_path_buf();
        config.memory.backend = "sqlite".into();

        let seed = SqliteMemory::new("sqlite", tmp.path()).unwrap();
        seed.store("seed", "seed fact", MemoryCategory::Core, None)
            .await
            .unwrap();
        drop(seed);

        let db = brain_db_path(tmp.path());
        let conn = rusqlite::Connection::open(&db).unwrap();
        proposals::propose(
            &conn,
            &zeroclaw_api::memory_traits::RatificationProposal::new(
                "trend",
                "deploys drift to Mondays",
                "normal",
                proposals::source_hash(&["trend", "x"]),
            ),
        )
        .unwrap();
        drop(conn);

        handle_stats(&config).await.unwrap();

        // Та же сводка, что печатает ветка stats: одно pending-предложение.
        let stats = counts_at(&db).unwrap();
        assert_eq!(stats.pending, 1);
        assert_eq!(stats.total(), 1);
    }

    #[cfg(feature = "agent-runtime")]
    #[tokio::test]
    async fn stats_prints_denials_for_sqlite() {
        use zeroclaw_memory::SqliteMemory;
        use zeroclaw_memory::receipts::{brain_db_path, denial_stats_at, record_denial_at};

        let tmp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.data_dir = tmp.path().to_path_buf();
        config.memory.backend = "sqlite".into();

        let seed = SqliteMemory::new("sqlite", tmp.path()).unwrap();
        seed.store("seed", "seed fact", MemoryCategory::Core, None)
            .await
            .unwrap();
        drop(seed);

        let db = brain_db_path(tmp.path());
        record_denial_at(&db, "test").unwrap();
        record_denial_at(&db, "test").unwrap();
        record_denial_at(&db, "other").unwrap();

        handle_stats(&config).await.unwrap();

        // Соседние тесты stdout не захватывают; сводка, которую печатает
        // ветка stats, проверяем через ту же функцию.
        let stats = denial_stats_at(&db).unwrap();
        assert_eq!(stats.total, 3);
        assert_eq!(
            stats.per_agent,
            vec![("test".to_string(), 2), ("other".to_string(), 1)]
        );
    }

    #[cfg(feature = "agent-runtime")]
    #[tokio::test]
    async fn stats_prints_graph_counters_when_auto_capture_on() {
        use zeroclaw_memory::SqliteMemory;
        use zeroclaw_memory::graph_feeder;
        use zeroclaw_memory::knowledge_graph::{KnowledgeGraph, NodeType};

        let tmp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.data_dir = tmp.path().to_path_buf();
        config.memory.backend = "sqlite".into();
        config.knowledge.auto_capture = true;
        config.knowledge.db_path = tmp.path().join("knowledge.db").display().to_string();

        let seed = SqliteMemory::new("sqlite", tmp.path()).unwrap();
        drop(seed);

        // Один fed-узел с replaces-цепочкой длины 2 + отказ фидера по лимиту.
        let graph = KnowledgeGraph::new(
            &config.knowledge.resolved_db_path(),
            config.knowledge.max_nodes,
        )
        .unwrap();
        let old = graph
            .add_node_feed(NodeType::Lesson, "v1", "c", &[], None, "h1")
            .unwrap()
            .unwrap();
        let new = graph
            .add_node_feed(NodeType::Lesson, "v2", "c", &[], None, "h2")
            .unwrap()
            .unwrap();
        graph.add_replaces_edge(&new, &old).unwrap();
        drop(graph);
        std::fs::write(
            tmp.path().join(graph_feeder::GRAPH_FEEDER_STATE_FILE),
            serde_json::json!({"last_id": 7, "refused_max_nodes": 3}).to_string(),
        )
        .unwrap();

        handle_stats(&config).await.unwrap();

        // Сводка, которую печатает ветка stats: цепочка 2 видна в stats графа.
        let stats = KnowledgeGraph::new(
            &config.knowledge.resolved_db_path(),
            config.knowledge.max_nodes,
        )
        .unwrap()
        .stats()
        .unwrap();
        assert_eq!(stats.total_nodes, 2);
        assert_eq!(stats.replaces_chain_lengths, vec![2]);
        assert_eq!(graph_feeder::stats_at(tmp.path()).refused_max_nodes, 3);
    }

    #[cfg(feature = "agent-runtime")]
    #[tokio::test]
    async fn stats_skips_graph_counters_when_auto_capture_off() {
        use zeroclaw_memory::SqliteMemory;

        let tmp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.data_dir = tmp.path().to_path_buf();
        config.memory.backend = "sqlite".into();
        // auto_capture выключен (дефолт): stats не создаёт knowledge.db.
        config.knowledge.db_path = tmp.path().join("knowledge.db").display().to_string();

        let seed = SqliteMemory::new("sqlite", tmp.path()).unwrap();
        drop(seed);

        handle_stats(&config).await.unwrap();

        assert!(
            !tmp.path().join("knowledge.db").exists(),
            "memory stats must not create knowledge.db with auto_capture off"
        );
    }
}
