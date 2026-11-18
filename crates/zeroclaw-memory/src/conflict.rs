//! Conflict resolution for memory entries.

use super::traits::{Memory, MemoryCategory, MemoryEntry};

/// Check for conflicting memories and mark old ones as superseded.
/// Returns the list of entry IDs that were superseded.
pub async fn check_and_resolve_conflicts(
    memory: &dyn Memory,
    key: &str,
    content: &str,
    category: &MemoryCategory,
    threshold: f64,
) -> anyhow::Result<Vec<String>> {
    // Only check conflicts for Core memories
    if !matches!(category, MemoryCategory::Core) {
        return Ok(Vec::new());
    }

    // Search for similar existing entries
    let candidates = memory.recall(content, 10, None, None, None).await?;

    let mut superseded = Vec::new();
    for candidate in &candidates {
        if candidate.key == key {
            continue; // Same key = update, not conflict
        }
        if !matches!(candidate.category, MemoryCategory::Core) {
            continue;
        }
        if let Some(score) = candidate.score
            && score > threshold
            && candidate.content != content
        {
            superseded.push(candidate.id.clone());
        }
    }

    Ok(superseded)
}

/// Mark entries as superseded in SQLite by setting their `superseded_by` column.
pub fn mark_superseded(
    conn: &rusqlite::Connection,
    superseded_ids: &[String],
    new_id: &str,
) -> anyhow::Result<()> {
    if superseded_ids.is_empty() {
        return Ok(());
    }

    for id in superseded_ids {
        conn.execute(
            "UPDATE memories SET superseded_by = ?1 WHERE id = ?2",
            rusqlite::params![new_id, id],
        )?;
    }

    Ok(())
}

/// Simple text-based conflict detection without embeddings.
/// Uses token overlap (Jaccard similarity) as a fast approximation
/// when vector embeddings are unavailable.
pub fn jaccard_similarity(a: &str, b: &str) -> f64 {
    let words_a: std::collections::HashSet<&str> = a.split_whitespace().collect();
    let words_b: std::collections::HashSet<&str> = b.split_whitespace().collect();

    if words_a.is_empty() && words_b.is_empty() {
        return 1.0;
    }
    if words_a.is_empty() || words_b.is_empty() {
        return 0.0;
    }

    let intersection = words_a.intersection(&words_b).count();
    let union = words_a.union(&words_b).count();

    if union == 0 {
        0.0
    } else {
        intersection as f64 / union as f64
    }
}

/// Find potentially conflicting entries using text similarity when embeddings
/// are not available. Returns entries above the threshold.
pub fn find_text_conflicts(
    entries: &[MemoryEntry],
    new_content: &str,
    threshold: f64,
) -> Vec<String> {
    entries
        .iter()
        .filter(|e| {
            matches!(e.category, MemoryCategory::Core)
                && e.superseded_by.is_none()
                && jaccard_similarity(&e.content, new_content) > threshold
                && e.content != new_content
        })
        .map(|e| e.id.clone())
        .collect()
}

/// Явные фразы коррекции оператора (epic 9-4, генератор «коррекции»):
/// сообщение, матчащее одну из фраз, порождает proposal kind=correction
/// с priority=highest — сильнейший сигнал слоя суждений.
pub const OPERATOR_CORRECTION_PHRASES: &[&str] = &[
    "неправильно",
    "не так",
    "запомни",
    "запомнить",
    "перестань",
    "исправь",
    "that's wrong",
    "remember this",
];

/// Матчит ли текст оператора фразы коррекции (case-insensitive, подстрока).
pub fn detect_operator_correction(text: &str) -> bool {
    let lower = text.to_lowercase();
    OPERATOR_CORRECTION_PHRASES
        .iter()
        .any(|phrase| lower.contains(phrase))
}

/// Proposal kind=contradiction для факта разрешённого противоречия:
/// новая запись reversibly supersede'ит проигравшие строки.
pub fn contradiction_proposal(
    update: &str,
    superseded_ids: &[String],
) -> zeroclaw_api::memory_traits::RatificationProposal {
    use zeroclaw_api::memory_traits::RatificationProposal;
    let excerpt: String = update.chars().take(200).collect();
    RatificationProposal::new(
        RatificationProposal::KIND_CONTRADICTION,
        format!(
            "Resolved contradiction: new fact supersedes {} row(s): {excerpt}",
            superseded_ids.len()
        ),
        RatificationProposal::PRIORITY_NORMAL,
        crate::proposals::source_hash(&["contradiction", update, &superseded_ids.join(",")]),
    )
    .with_refs(superseded_ids.iter().cloned())
}

/// Proposal kind=correction из явной фразы оператора (priority=highest,
/// бессрочно: throttle и затухание её не трогают).
pub fn operator_correction_proposal(
    excerpt: &str,
) -> zeroclaw_api::memory_traits::RatificationProposal {
    use zeroclaw_api::memory_traits::RatificationProposal;
    RatificationProposal::new(
        RatificationProposal::KIND_CORRECTION,
        format!("Operator correction: {excerpt}"),
        RatificationProposal::PRIORITY_HIGHEST,
        crate::proposals::source_hash(&["correction", excerpt]),
    )
}

// ── Contested clusters (epic 9-6) ────────────────────────────────────

/// Один contested-кластер: группа ключей не-суперседнутых суждений,
/// похожих по содержанию, но расходящихся по тексту (противоречие).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContestedCluster {
    pub keys: Vec<String>,
}

/// Значимые токены для детерминированной кластеризации (модель не нужна).
fn cluster_tokens(content: &str) -> std::collections::HashSet<String> {
    const STOPWORDS: &[&str] = &[
        "the", "and", "for", "with", "that", "this", "from", "into", "are", "was", "were", "при",
        "для", "что", "это", "как", "не", "на", "в", "и", "с", "по", "от", "из",
    ];
    content
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .map(str::to_lowercase)
        .filter(|token| token.len() >= 4 && !STOPWORDS.contains(&token.as_str()))
        .collect()
}

/// Contested-кластеры: entity-SQL (core, не-суперседнуто, per-agent scope)
/// + детерминированная union-find кластеризация по jaccard-токенам.
/// Возвращает до `limit` кластеров размера ≥2 (противоречащие друг другу
///   суждения — генератор противоречий без единого LLM-вызова).
pub fn contested_clusters(
    conn: &rusqlite::Connection,
    agent: Option<&str>,
    limit: usize,
) -> anyhow::Result<Vec<ContestedCluster>> {
    const CANDIDATE_LIMIT: usize = 500;
    const CLUSTER_JACCARD: f64 = 0.4;

    let agent_clause = if agent.is_some() {
        "AND (SELECT alias FROM agents WHERE agents.id = memories.agent_id) = ?1"
    } else {
        ""
    };
    let sql = format!(
        "SELECT key, content FROM memories \
         WHERE category = 'core' AND superseded_by IS NULL {agent_clause} \
         ORDER BY updated_at DESC LIMIT {CANDIDATE_LIMIT}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params![agent.unwrap_or("")], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let candidates: Vec<(String, std::collections::HashSet<String>)> = rows
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|(key, content)| (key, cluster_tokens(&content)))
        .filter(|(_, tokens)| !tokens.is_empty())
        .collect();

    // Union-find по парам с jaccard ≥ порога и разным содержимым.
    let mut parent: Vec<usize> = (0..candidates.len()).collect();
    fn find(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }
    let union = |parent: &mut Vec<usize>, a: usize, b: usize| {
        let (ra, rb) = (find(parent, a), find(parent, b));
        if ra != rb {
            parent[rb] = ra;
        }
    };
    for i in 0..candidates.len() {
        for j in (i + 1)..candidates.len() {
            let a = &candidates[i].1;
            let b = &candidates[j].1;
            let intersection = a.intersection(b).count();
            if intersection == 0 {
                continue;
            }
            let jaccard = intersection as f64 / a.union(b).count() as f64;
            if jaccard >= CLUSTER_JACCARD {
                union(&mut parent, i, j);
            }
        }
    }
    let mut groups: std::collections::HashMap<usize, Vec<String>> =
        std::collections::HashMap::new();
    for (idx, (key, _)) in candidates.iter().enumerate() {
        let root = find(&mut parent, idx);
        groups.entry(root).or_default().push(key.clone());
    }
    let mut clusters: Vec<ContestedCluster> = groups
        .into_values()
        .filter(|keys| keys.len() >= 2)
        .map(|mut keys| {
            keys.sort();
            ContestedCluster { keys }
        })
        .collect();
    clusters.sort_by_key(|cluster| std::cmp::Reverse(cluster.keys.len()));
    clusters.truncate(limit);
    Ok(clusters)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jaccard_identical_strings() {
        let sim = jaccard_similarity("hello world", "hello world");
        assert!((sim - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn jaccard_disjoint_strings() {
        let sim = jaccard_similarity("hello world", "foo bar");
        assert!(sim.abs() < f64::EPSILON);
    }

    #[test]
    fn jaccard_partial_overlap() {
        let sim = jaccard_similarity("the quick brown fox", "the slow brown dog");
        // overlap: "the", "brown" = 2; union: "the", "quick", "brown", "fox", "slow", "dog" = 6
        assert!((sim - 2.0 / 6.0).abs() < 0.01);
    }

    #[test]
    fn jaccard_empty_strings() {
        assert!((jaccard_similarity("", "") - 1.0).abs() < f64::EPSILON);
        assert!(jaccard_similarity("hello", "").abs() < f64::EPSILON);
        assert!(jaccard_similarity("", "hello").abs() < f64::EPSILON);
    }

    #[test]
    fn find_text_conflicts_filters_correctly() {
        let entries = vec![
            MemoryEntry {
                principal_id: None,
                id: "1".into(),
                key: "pref".into(),
                content: "User prefers Rust for systems work".into(),
                category: MemoryCategory::Core,
                timestamp: "now".into(),
                session_id: None,
                score: None,
                namespace: "default".into(),
                importance: Some(0.7),
                superseded_by: None,
                kind: None,
                pinned: false,
                tenant_id: None,
                agent_alias: None,
                agent_id: None,
            },
            MemoryEntry {
                principal_id: None,
                id: "2".into(),
                key: "daily1".into(),
                content: "User prefers Rust for systems work".into(),
                category: MemoryCategory::Daily,
                timestamp: "now".into(),
                session_id: None,
                score: None,
                namespace: "default".into(),
                importance: Some(0.3),
                superseded_by: None,
                kind: None,
                pinned: false,
                tenant_id: None,
                agent_alias: None,
                agent_id: None,
            },
        ];

        // Only Core entries should be flagged
        let conflicts = find_text_conflicts(&entries, "User now prefers Go for systems work", 0.3);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0], "1");
    }

    #[test]
    fn jaccard_deduplicates_repeated_words() {
        // Token sets ignore repeats: {a, b} vs {a, b} are identical.
        assert!((jaccard_similarity("a a b", "a b b") - 1.0).abs() < f64::EPSILON);
        // {a, b} vs {a} -> intersection 1 / union 2.
        assert!((jaccard_similarity("a a b", "a") - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn find_text_conflicts_skips_superseded_and_identical() {
        let entry = |id: &str, content: &str, superseded: Option<String>| MemoryEntry {
            principal_id: None,
            id: id.into(),
            key: "k".into(),
            content: content.into(),
            category: MemoryCategory::Core,
            timestamp: "now".into(),
            session_id: None,
            score: None,
            namespace: "default".into(),
            importance: Some(0.7),
            superseded_by: superseded,
            kind: None,
            pinned: false,
            tenant_id: None,
            agent_alias: None,
            agent_id: None,
        };
        let entries = vec![
            entry("active", "User prefers Rust for systems work", None),
            entry(
                "old",
                "User prefers Rust for systems work",
                Some("x".into()),
            ),
        ];

        // The already-superseded "old" entry is skipped; only "active" conflicts.
        let conflicts = find_text_conflicts(&entries, "User now prefers Go for systems work", 0.3);
        assert_eq!(conflicts, vec!["active".to_string()]);

        // An identical new_content is an update, not a conflict.
        let none = find_text_conflicts(&entries, "User prefers Rust for systems work", 0.3);
        assert!(none.is_empty());
    }

    // ── Contested clusters (epic 9-6) ───────────────────────────────

    #[test]
    fn contested_clusters_find_similar_contradicting_judgments() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = crate::sqlite::SqliteMemory::new("test", tmp.path()).unwrap();
        drop(mem);
        let db = crate::receipts::brain_db_path(tmp.path());
        let conn = rusqlite::Connection::open(db).unwrap();
        let now = chrono::Local::now().to_rfc3339();
        let insert = |id: &str, key: &str, content: &str| {
            conn.execute(
                "INSERT INTO memories (id, key, content, category, created_at, updated_at,
                    session_id, namespace, importance, agent_id, kind, pinned, tenant_id)
                 VALUES (?1, ?2, ?3, 'core', ?4, ?4, NULL, 'default', 0.5,
                         (SELECT id FROM agents WHERE alias = 'default' LIMIT 1),
                         NULL, 0, NULL)",
                rusqlite::params![id, key, content, now],
            )
            .unwrap();
        };
        // Два похожих, но расходящихся суждения — кластер противоречия
        // (3 общих токена из 7 ≥ порога 0.4).
        insert(
            "c1",
            "judg_a",
            "deploys drift to Mondays and CI breaks often",
        );
        insert(
            "c2",
            "judg_b",
            "deploys drift to Tuesdays and CI breaks rarely",
        );
        // Токенно-далёкая строка — не в кластере.
        insert("c3", "unrelated", "quantum chromodynamics notes");

        let clusters = contested_clusters(&conn, Some("default"), 10).unwrap();
        assert_eq!(clusters.len(), 1);
        assert_eq!(
            clusters[0].keys,
            vec!["judg_a".to_string(), "judg_b".to_string()]
        );

        // Суперседнутое суждение выходит из кластера (non-destructive).
        conn.execute(
            "UPDATE memories SET superseded_by = 'newer' WHERE key = 'judg_b'",
            [],
        )
        .unwrap();
        let after = contested_clusters(&conn, Some("default"), 10).unwrap();
        assert!(after.is_empty(), "superseded rows leave the cluster");
    }

    #[test]
    fn contested_clusters_respect_agent_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = crate::sqlite::SqliteMemory::new("test", tmp.path()).unwrap();
        drop(mem);
        let db = crate::receipts::brain_db_path(tmp.path());
        let conn = rusqlite::Connection::open(db).unwrap();
        let now = chrono::Local::now().to_rfc3339();
        conn.execute(
            "INSERT INTO agents (id, alias, created_at) VALUES ('agent-x', 'other', ?1)",
            rusqlite::params![now],
        )
        .unwrap();
        for (i, content) in [
            "deploys drift to Mondays and CI breaks often",
            "deploys drift to Tuesdays and CI breaks rarely",
        ]
        .iter()
        .enumerate()
        {
            conn.execute(
                "INSERT INTO memories (id, key, content, category, created_at, updated_at,
                    session_id, namespace, importance, agent_id, kind, pinned, tenant_id)
                 VALUES (?1, ?2, ?3, 'core', ?4, ?4, NULL, 'default', 0.5,
                         'agent-x', NULL, 0, NULL)",
                rusqlite::params![format!("x{i}"), format!("foreign_{i}"), content, now],
            )
            .unwrap();
        }
        assert!(
            contested_clusters(&conn, Some("default"), 10)
                .unwrap()
                .is_empty(),
            "own scope must not read foreign agent rows"
        );
        assert_eq!(
            contested_clusters(&conn, Some("other"), 10).unwrap().len(),
            1
        );
    }
}
