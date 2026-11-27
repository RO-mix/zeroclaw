use super::traits::{
    Memory, MemoryCategory, MemoryEntry, MemoryKind, StoreOptions, is_recent_recall_query,
};
use async_trait::async_trait;
use chrono::{DateTime, FixedOffset, Local, NaiveDate, Utc};
use std::path::{Path, PathBuf};
use tokio::fs;

fn entry_in_window(
    timestamp: &str,
    since: Option<&DateTime<FixedOffset>>,
    until: Option<&DateTime<FixedOffset>>,
) -> bool {
    if let Ok(ts) = DateTime::parse_from_rfc3339(timestamp) {
        if let Some(s) = since
            && ts < *s
        {
            return false;
        }
        if let Some(u) = until
            && ts > *u
        {
            return false;
        }
        return true;
    }
    if let Ok(date) = NaiveDate::parse_from_str(timestamp, "%Y-%m-%d") {
        if let Some(s) = since
            && date < s.date_naive()
        {
            return false;
        }
        if let Some(u) = until
            && date > u.date_naive()
        {
            return false;
        }
        return true;
    }
    // Non-date stems (e.g. MEMORY.md) are evergreen; never window-filtered.
    true
}

pub struct MarkdownMemory {
    alias: String,
    workspace_dir: PathBuf,
}

impl MarkdownMemory {
    pub fn new(alias: &str, workspace_dir: &Path) -> Self {
        Self {
            alias: alias.to_string(),
            workspace_dir: workspace_dir.to_path_buf(),
        }
    }

    fn memory_dir(&self) -> PathBuf {
        self.workspace_dir.join("memory")
    }

    fn core_path(&self) -> PathBuf {
        self.workspace_dir.join("MEMORY.md")
    }

    fn daily_path(&self) -> PathBuf {
        let date = Local::now().format("%Y-%m-%d").to_string();
        self.memory_dir().join(format!("{date}.md"))
    }

    /// Epic 9-1: markdown has no typed row surface, so the narrative
    /// distillate (the single pinned `MemoryKind::Narrative` record) persists
    /// as a dedicated overwrite-semantics file in the workspace. This is the
    /// same contract sqlite implements as a typed row, in flat-file shape.
    fn narrative_path(&self) -> PathBuf {
        self.workspace_dir.join("NARRATIVE.md")
    }

    /// Shared body of `store_with_options` / `store_with_options_and_agent`.
    async fn store_options_inner(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        options: StoreOptions,
    ) -> anyhow::Result<()> {
        if options.kind == Some(MemoryKind::Narrative) {
            fs::write(self.narrative_path(), content).await?;
            return Ok(());
        }
        if options.requires_full_options_storage() {
            anyhow::bail!(
                "memory backend 'markdown' cannot persist StoreOptions kind/pinned/tenant_id; \
                 only the narrative distillate (kind=narrative) supports pinned storage"
            );
        }
        self.store_with_metadata(
            key,
            content,
            category,
            session_id,
            options.namespace.as_deref(),
            options.importance,
        )
        .await
    }

    async fn ensure_dirs(&self) -> anyhow::Result<()> {
        fs::create_dir_all(self.memory_dir()).await?;
        Ok(())
    }

    async fn append_to_file(&self, path: &Path, content: &str) -> anyhow::Result<()> {
        self.ensure_dirs().await?;

        let existing = if path.exists() {
            fs::read_to_string(path).await.unwrap_or_default()
        } else {
            String::new()
        };

        let updated = if existing.is_empty() {
            let header = if path == self.core_path() {
                "# Long-Term Memory\n\n"
            } else {
                let date = Local::now().format("%Y-%m-%d").to_string();
                &format!("# Daily Log — {date}\n\n")
            };
            format!("{header}{content}\n")
        } else {
            format!("{existing}\n{content}\n")
        };

        fs::write(path, updated).await?;
        Ok(())
    }

    fn parse_entries_from_file(
        path: &Path,
        content: &str,
        category: &MemoryCategory,
    ) -> Vec<MemoryEntry> {
        let filename = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");

        content
            .lines()
            .filter(|line| {
                let trimmed = line.trim();
                !trimmed.is_empty() && !trimmed.starts_with('#')
            })
            .enumerate()
            .map(|(i, line)| {
                let trimmed = line.trim();
                let clean = trimmed.strip_prefix("- ").unwrap_or(trimmed);
                MemoryEntry {
                    principal_id: None,
                    id: format!("{filename}:{i}"),
                    key: format!("{filename}:{i}"),
                    content: clean.to_string(),
                    category: category.clone(),
                    timestamp: filename.to_string(),
                    session_id: None,
                    score: None,
                    namespace: "default".into(),
                    importance: None,
                    superseded_by: None,
                    kind: None,
                    pinned: false,
                    tenant_id: None,
                    agent_alias: None,
                    agent_id: None,
                }
            })
            .collect()
    }

    async fn read_all_entries(&self) -> anyhow::Result<Vec<MemoryEntry>> {
        let mut entries = Vec::new();

        // Read MEMORY.md (core)
        let core_path = self.core_path();
        if core_path.exists() {
            let content = fs::read_to_string(&core_path).await?;
            entries.extend(Self::parse_entries_from_file(
                &core_path,
                &content,
                &MemoryCategory::Core,
            ));
        }

        // Read daily logs
        let mem_dir = self.memory_dir();
        if mem_dir.exists() {
            let mut dir = fs::read_dir(&mem_dir).await?;
            while let Some(entry) = dir.next_entry().await? {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("md") {
                    let content = fs::read_to_string(&path).await?;
                    entries.extend(Self::parse_entries_from_file(
                        &path,
                        &content,
                        &MemoryCategory::Daily,
                    ));
                }
            }
        }

        // Epic 9-1: expose the narrative distillate file under NARRATIVE_KEY
        // with kind/pinned so `get`, recall, and deploy treat it like the
        // sqlite typed row.
        let narrative_path = self.narrative_path();
        if narrative_path.exists() {
            let content = fs::read_to_string(&narrative_path).await?;
            let timestamp = fs::metadata(&narrative_path)
                .await
                .and_then(|m| m.modified())
                .map(|t| DateTime::<Utc>::from(t).to_rfc3339())
                .unwrap_or_default();
            entries.push(MemoryEntry {
                principal_id: None,
                id: crate::narrative::NARRATIVE_KEY.to_string(),
                key: crate::narrative::NARRATIVE_KEY.to_string(),
                content,
                category: MemoryCategory::Core,
                timestamp,
                session_id: None,
                score: None,
                namespace: "default".into(),
                importance: None,
                superseded_by: None,
                kind: Some(MemoryKind::Narrative),
                pinned: true,
                tenant_id: None,
                agent_alias: None,
                agent_id: None,
            });
        }

        entries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        Ok(entries)
    }
}

#[async_trait]
impl Memory for MarkdownMemory {
    fn name(&self) -> &str {
        "markdown"
    }

    async fn store(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        _session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        let entry = format!("- **{key}**: {content}");
        let path = match category {
            MemoryCategory::Core => self.core_path(),
            _ => self.daily_path(),
        };
        self.append_to_file(&path, &entry).await
    }

    async fn recall(
        &self,
        query: &str,
        limit: usize,
        _session_id: Option<&str>,
        since: Option<&str>,
        until: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let since_dt = since
            .map(chrono::DateTime::parse_from_rfc3339)
            .transpose()
            .map_err(|e| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(
                            ::serde_json::json!({"field": "since", "error": format!("{}", e)})
                        ),
                    "recall window bound rejected"
                );
                anyhow::Error::msg(format!("invalid 'since' date (expected RFC 3339): {e}"))
            })?;
        let until_dt = until
            .map(chrono::DateTime::parse_from_rfc3339)
            .transpose()
            .map_err(|e| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(
                            ::serde_json::json!({"field": "until", "error": format!("{}", e)})
                        ),
                    "recall window bound rejected"
                );
                anyhow::Error::msg(format!("invalid 'until' date (expected RFC 3339): {e}"))
            })?;
        if let (Some(s), Some(u)) = (&since_dt, &until_dt)
            && s >= u
        {
            anyhow::bail!("'since' must be before 'until'");
        }

        let all = self.read_all_entries().await?;
        let keywords: Vec<String> = if is_recent_recall_query(query) {
            Vec::new()
        } else {
            query
                .to_lowercase()
                .split_whitespace()
                .map(str::to_string)
                .collect()
        };

        let mut scored: Vec<MemoryEntry> = all
            .into_iter()
            .filter_map(|mut entry| {
                if !entry_in_window(&entry.timestamp, since_dt.as_ref(), until_dt.as_ref()) {
                    return None;
                }
                if keywords.is_empty() {
                    entry.score = Some(1.0);
                    return Some(entry);
                }
                let content_lower = entry.content.to_lowercase();
                let matched = keywords
                    .iter()
                    .filter(|kw| content_lower.contains(kw.as_str()))
                    .count();
                if matched > 0 {
                    #[allow(clippy::cast_precision_loss)]
                    let score = matched as f64 / keywords.len() as f64;
                    entry.score = Some(score);
                    Some(entry)
                } else {
                    None
                }
            })
            .collect();

        scored.sort_by(|a, b| {
            if keywords.is_empty() {
                b.timestamp.as_str().cmp(a.timestamp.as_str())
            } else {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            }
        });
        scored.truncate(limit);
        Ok(scored)
    }

    async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        let all = self.read_all_entries().await?;
        Ok(all
            .into_iter()
            .find(|e| e.key == key || e.content.contains(key)))
    }

    async fn list(
        &self,
        category: Option<&MemoryCategory>,
        _session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let all = self.read_all_entries().await?;
        match category {
            Some(cat) => Ok(all.into_iter().filter(|e| &e.category == cat).collect()),
            None => Ok(all),
        }
    }

    async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
        // Markdown memory is append-only by design (audit trail)
        // Return false to indicate the entry wasn't removed
        Ok(false)
    }

    async fn forget_for_agent(&self, _key: &str, _agent_id: &str) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn count(&self) -> anyhow::Result<usize> {
        let all = self.read_all_entries().await?;
        Ok(all.len())
    }

    async fn health_check(&self) -> bool {
        self.workspace_dir.exists()
    }

    async fn store_with_agent(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        _namespace: Option<&str>,
        _importance: Option<f64>,
        _agent_id: Option<&str>,
    ) -> anyhow::Result<()> {
        self.store(key, content, category, session_id).await
    }

    async fn store_with_options(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        options: StoreOptions,
    ) -> anyhow::Result<()> {
        self.store_options_inner(key, content, category, session_id, options)
            .await
    }

    async fn store_with_options_and_agent(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        options: StoreOptions,
        _agent_id: Option<&str>,
    ) -> anyhow::Result<()> {
        // Markdown attribution lives on the on-disk path; the caller-supplied
        // agent_id is intentionally ignored (same contract as store_with_agent).
        self.store_options_inner(key, content, category, session_id, options)
            .await
    }

    async fn recall_for_agents(
        &self,
        _allowed_agent_ids: &[&str],
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        since: Option<&str>,
        until: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        self.recall(query, limit, session_id, since, until).await
    }
}

impl ::zeroclaw_api::attribution::Attributable for MarkdownMemory {
    fn role(&self) -> ::zeroclaw_api::attribution::Role {
        ::zeroclaw_api::attribution::Role::Memory(::zeroclaw_api::attribution::MemoryKind::Markdown)
    }
    fn alias(&self) -> &str {
        &self.alias
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use tempfile::TempDir;

    fn temp_workspace() -> (TempDir, MarkdownMemory) {
        let tmp = TempDir::new().unwrap();
        let mem = MarkdownMemory::new("markdown", tmp.path());
        (tmp, mem)
    }

    #[tokio::test]
    async fn markdown_name() {
        let (_tmp, mem) = temp_workspace();
        assert_eq!(mem.name(), "markdown");
    }

    #[tokio::test]
    async fn markdown_health_check() {
        let (_tmp, mem) = temp_workspace();
        assert!(mem.health_check().await);
    }

    #[tokio::test]
    async fn markdown_store_core() {
        let (_tmp, mem) = temp_workspace();
        mem.store("pref", "User likes Rust", MemoryCategory::Core, None)
            .await
            .unwrap();
        let content = fs::read_to_string(mem.core_path()).await.unwrap();
        assert!(content.contains("User likes Rust"));
    }

    #[tokio::test]
    async fn markdown_store_daily() {
        let (_tmp, mem) = temp_workspace();
        mem.store("note", "Finished tests", MemoryCategory::Daily, None)
            .await
            .unwrap();
        let path = mem.daily_path();
        let content = fs::read_to_string(path).await.unwrap();
        assert!(content.contains("Finished tests"));
    }

    #[tokio::test]
    async fn markdown_recall_keyword() {
        let (_tmp, mem) = temp_workspace();
        mem.store("a", "Rust is fast", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("b", "Python is slow", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("c", "Rust and safety", MemoryCategory::Core, None)
            .await
            .unwrap();

        let results = mem.recall("Rust", 10, None, None, None).await.unwrap();
        assert!(results.len() >= 2);
        assert!(
            results
                .iter()
                .all(|r| r.content.to_lowercase().contains("rust"))
        );
    }

    #[tokio::test]
    async fn markdown_recall_no_match() {
        let (_tmp, mem) = temp_workspace();
        mem.store("a", "Rust is great", MemoryCategory::Core, None)
            .await
            .unwrap();
        let results = mem
            .recall("javascript", 10, None, None, None)
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn markdown_recall_star_query_returns_recent_entries() {
        let (_tmp, mem) = temp_workspace();
        mem.store("a", "first memory", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("b", "second memory", MemoryCategory::Daily, None)
            .await
            .unwrap();

        let results = mem.recall("*", 10, None, None, None).await.unwrap();
        assert_eq!(results.len(), 2);
        assert!(
            results
                .iter()
                .any(|entry| entry.content.contains("first memory"))
        );
        assert!(
            results
                .iter()
                .any(|entry| entry.content.contains("second memory"))
        );
    }

    #[tokio::test]
    async fn markdown_count() {
        let (_tmp, mem) = temp_workspace();
        mem.store("a", "first", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("b", "second", MemoryCategory::Core, None)
            .await
            .unwrap();
        let count = mem.count().await.unwrap();
        assert!(count >= 2);
    }

    #[tokio::test]
    async fn markdown_list_by_category() {
        let (_tmp, mem) = temp_workspace();
        mem.store("a", "core fact", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("b", "daily note", MemoryCategory::Daily, None)
            .await
            .unwrap();

        let core = mem.list(Some(&MemoryCategory::Core), None).await.unwrap();
        assert!(core.iter().all(|e| e.category == MemoryCategory::Core));

        let daily = mem.list(Some(&MemoryCategory::Daily), None).await.unwrap();
        assert!(daily.iter().all(|e| e.category == MemoryCategory::Daily));
    }

    #[tokio::test]
    async fn markdown_forget_is_noop() {
        let (_tmp, mem) = temp_workspace();
        mem.store("a", "permanent", MemoryCategory::Core, None)
            .await
            .unwrap();
        let removed = mem.forget("a").await.unwrap();
        assert!(!removed, "Markdown memory is append-only");
    }

    #[tokio::test]
    async fn markdown_empty_recall() {
        let (_tmp, mem) = temp_workspace();
        let results = mem.recall("anything", 10, None, None, None).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn markdown_empty_count() {
        let (_tmp, mem) = temp_workspace();
        assert_eq!(mem.count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn markdown_entries_carry_no_agent_attribution() {
        let (_tmp, mem) = temp_workspace();
        mem.store("k", "v", MemoryCategory::Core, None)
            .await
            .unwrap();
        let entry = mem.get("MEMORY.md:0").await.unwrap();
        if let Some(entry) = entry {
            assert!(
                entry.agent_alias.is_none(),
                "markdown rows must never claim an agent alias"
            );
            assert!(
                entry.agent_id.is_none(),
                "markdown rows must never claim a raw agent id either"
            );
        }
        // list path must show the same shape regardless of how a row is
        // surfaced (keyed lookup vs. enumeration).
        let rows = mem.list(None, None).await.unwrap();
        for row in rows {
            assert!(
                row.agent_alias.is_none(),
                "list path must not synthesize aliases"
            );
            assert!(row.agent_id.is_none(), "list path must not synthesize ids");
        }
    }

    #[tokio::test]
    async fn markdown_recall_since_until_filters_daily() {
        let (_tmp, mem) = temp_workspace();
        mem.store("today", "daily standup note", MemoryCategory::Daily, None)
            .await
            .unwrap();
        mem.store("core", "evergreen daily fact", MemoryCategory::Core, None)
            .await
            .unwrap();

        let today = Local::now().date_naive();
        let yesterday = (today - chrono::Duration::days(1))
            .and_hms_opt(23, 59, 59)
            .unwrap();
        let yesterday_rfc = Local.from_local_datetime(&yesterday).unwrap().to_rfc3339();
        let past = (today - chrono::Duration::days(7))
            .and_hms_opt(0, 0, 0)
            .unwrap();
        let past_rfc = Local.from_local_datetime(&past).unwrap().to_rfc3339();

        // until = yesterday: today's daily entry is outside the window and
        // must be dropped, but the evergreen MEMORY.md entry must survive.
        let bounded = mem
            .recall("daily", 10, None, None, Some(&yesterday_rfc))
            .await
            .unwrap();
        assert!(
            !bounded.iter().any(|e| e.content.contains("standup")),
            "today's daily entry must be excluded when until=yesterday"
        );
        assert!(
            bounded.iter().any(|e| e.content.contains("evergreen")),
            "evergreen MEMORY.md entry must not be window-filtered"
        );

        // since = a week ago: today's daily entry is inside the window.
        let recent = mem
            .recall("daily", 10, None, Some(&past_rfc), None)
            .await
            .unwrap();
        assert!(
            recent.iter().any(|e| e.content.contains("standup")),
            "today's daily entry must be included when since is in the past"
        );
    }

    #[tokio::test]
    async fn store_with_options_narrative_round_trips_as_file() {
        let (_tmp, mem) = temp_workspace();

        mem.store_with_options(
            crate::narrative::NARRATIVE_KEY,
            "distilled persona narrative",
            MemoryCategory::Core,
            None,
            StoreOptions {
                kind: Some(MemoryKind::Narrative),
                pinned: true,
                ..StoreOptions::default()
            },
        )
        .await
        .expect("narrative store must succeed on markdown");

        let entry = mem
            .get(crate::narrative::NARRATIVE_KEY)
            .await
            .unwrap()
            .expect("narrative entry must be readable back under NARRATIVE_KEY");
        assert_eq!(entry.content, "distilled persona narrative");
        assert_eq!(entry.kind, Some(MemoryKind::Narrative));
        assert!(entry.pinned);

        // Second store overwrites: the distillate is a single record.
        mem.store_with_options(
            crate::narrative::NARRATIVE_KEY,
            "consolidated persona narrative",
            MemoryCategory::Core,
            None,
            StoreOptions {
                kind: Some(MemoryKind::Narrative),
                pinned: true,
                ..StoreOptions::default()
            },
        )
        .await
        .unwrap();
        let entry = mem
            .get(crate::narrative::NARRATIVE_KEY)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entry.content, "consolidated persona narrative");
    }

    #[tokio::test]
    async fn store_with_options_full_non_narrative_bails() {
        let (_tmp, mem) = temp_workspace();
        let err = mem
            .store_with_options(
                "k",
                "v",
                MemoryCategory::Core,
                None,
                StoreOptions::default().pinned(true),
            )
            .await
            .expect_err("pinned non-narrative store must bail on markdown");
        assert!(
            err.to_string().contains("cannot persist StoreOptions"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn store_with_options_partial_falls_back_to_append() {
        let (_tmp, mem) = temp_workspace();
        mem.store_with_options(
            "pref",
            "namespace-only write",
            MemoryCategory::Core,
            None,
            StoreOptions {
                namespace: Some("ops".into()),
                ..StoreOptions::default()
            },
        )
        .await
        .expect("namespace-only options must keep the append-only behavior");
        let content = fs::read_to_string(mem.core_path()).await.unwrap();
        assert!(content.contains("namespace-only write"));
    }
}
