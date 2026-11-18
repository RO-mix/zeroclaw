//! Ночной цикл сна — LLM-переоценка суждений (epic 9-6, контур RAM).
//!
//! Per-agent прогон переоценки по дельте. Трёхвентильный триггер
//! (время × объём × lock, из Auto Dream): окно открывается только когда все
//! три вентиля открыты. Входы материализуются БЕЗ модели (контестед-кластеры
//! SQL, нерассмотренные proposals, свежие эпизоды окна, очередь ревалидации
//! принятых фактов), регидратируются из живых строк memories (уставшие и
//! удалённые сниппеты пропускаются — OpenClaw-паттерн), проходят пред-гейт
//! подтверждающих сигналов (minRecallCount-аналог) — и только затем модель
//! агента ЕДИНСТВЕННЫЙ раз за прогон переоценивает отобранное и возвращает
//! СТРОГИЙ JSON операций (8 revision-действий, не прозу). Писатель здесь
//! не исполняет операции: revision-предложения уходят в очередь ратификации
//! 9-4 (reuse таблиц/packets/receipts); в долговременную память всё
//! попадает только через пакетную ратификацию оператором.
//!
//! Гарды: прогон pinned-промптом с пустым toolset (`allowed_tools = []`) и
//! `memory_free` — file tools недоступны, прямых записей в memories из сна
//! нет по построению; предложение правки осевого файла остаётся текстом
//! в payload (спека: matrix «модель предлагает правку осевого файла»).
//!
//! Antidivergence-пробник после каждой применённой пачки: дымовые запросы
//! по proposal_refs применённых пакетов до/после (снапшот прошлого прогона
//! против текущей выдачи); регресс → drift_events + WARN, пакет помечается
//! contested в отчёте (не блокирует — данные для оператора).
//!
//! Всё за `[memory.dream].enabled` (дефолт выкл, байт-в-байт): worker не
//! стартует, гейт проверяет флаг на каждом тике (live-config flip), пустой
//! прогон пишет завершение в state без побочных эффектов, упавший прогон
//! не продвигает дельту (seen/pending/last_run не двигаются, retry окном).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use zeroclaw_api::memory_traits::RatificationProposal;
use zeroclaw_config::schema::Config;
use zeroclaw_memory::conflict;
use zeroclaw_memory::proposals;

use crate::agent::loop_::AgentRunOverrides;

/// Тик worker'а: пробник + вентили на каждом тике; прогоны редки
/// (min_interval_days), поэтому тик может быть ленивым.
const WORKER_TICK: Duration = Duration::from_secs(900);
/// State-file (паттерн ratification/hygiene): счётчики, content-hash дельта,
/// probe-снапшот, отчёт с категориями отказов.
const DREAM_STATE_FILE: &str = "dream_worker_state.json";
/// Lock против параллельных прогонов/пайплайнов (владелец — внутри файла).
const DREAM_LOCK_FILE: &str = "dream_worker.lock";
/// Пауза между попытками после упавшего прогона (state не продвигается,
/// но бесконечный ретрай подряд модель не дёргает).
const FAILURE_BACKOFF: Duration = Duration::from_secs(3600);
/// Потолок seen/content-hash списка (FIFO-выталкивание).
const SEEN_HASH_CAP: usize = 1000;
/// Длина регидратированного сниппета одной входной позиции.
const HYDRATE_EXCERPT_CHARS: usize = 500;
/// Сколько hit'ов хранит один дымовой запрос пробника.
const SMOKE_QUERY_LIMIT: i64 = 5;
/// Сколько кандидатов каждого класса входит в материализацию до лимита.
const CANDIDATE_PER_CLASS: usize = 20;

// ── Состояние (state-file, паттерн ratification worker) ──────────────

/// Счётчики сна (расширение `memory stats`, паттерн 9-3/9-4).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DreamCounters {
    pub revisions_proposed: u64,
    pub revisions_accepted: u64,
    pub revalidations: u64,
    pub drift_events: u64,
}

/// Отчёт одного прогона: категории отказов пред-фильтра (объяснимость
/// «почему не предложено», DREAMS.md-аналог нашего аудит-следа).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DreamReport {
    pub run_at: String,
    pub proposed: u64,
    pub skipped_seen: u64,
    pub skipped_dead_refs: u64,
    pub skipped_below_gate: u64,
    pub over_limit: u64,
    pub invalid_output: bool,
    pub drift: u64,
}

/// Per-agent состояние сна.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AgentDreamState {
    pub last_run_at: Option<String>,
    pub last_attempt_at: Option<String>,
    pub run_count: u64,
    /// Content-hash уже предъявленных входов (инкрементальная дельта).
    pub seen_hashes: Vec<String>,
    /// Входы, не попавшие под лимит позиций — остаются в дельте.
    pub pending_hashes: Vec<String>,
    /// Antidivergence-базис: дымовой запрос → выдача прошлого прогона.
    pub probe_snapshot: std::collections::BTreeMap<String, Vec<String>>,
    pub last_probe_at: Option<String>,
    pub counters: DreamCounters,
    pub last_report: Option<DreamReport>,
}

/// Состояние worker'а: одна запись на агента (per-agent scope).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DreamState {
    pub agents: std::collections::BTreeMap<String, AgentDreamState>,
}

fn state_path(data_dir: &Path) -> PathBuf {
    data_dir.join(DREAM_STATE_FILE)
}

fn lock_path(data_dir: &Path) -> PathBuf {
    data_dir.join(DREAM_LOCK_FILE)
}

/// Загрузить состояние; файла нет/мусор — пустое (первый прогон строит
/// базис пробника, ничего не сравнивая).
pub fn load_state(data_dir: &Path) -> DreamState {
    std::fs::read_to_string(state_path(data_dir))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_state(data_dir: &Path, state: &DreamState) -> Result<()> {
    let path = state_path(data_dir);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string(state)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Сводка для `memory stats` (CLI читает state-file напрямую).
#[must_use]
pub fn stats_at(data_dir: &Path) -> DreamState {
    load_state(data_dir)
}

// ── Lock (третий вентиль) ────────────────────────────────────────────

/// Guard lock-файла: держится от вентиля до конца прогона (включая await
/// модели), освобождается в Drop при любом исходе.
struct DreamLockGuard {
    path: PathBuf,
}

impl DreamLockGuard {
    /// Взять lock; занят — None (владелец залогирован caller'ом через owner()).
    fn acquire(data_dir: &Path) -> Option<Self> {
        let path = lock_path(data_dir);
        let owner = format!(
            "pid={} since={}",
            std::process::id(),
            chrono::Local::now().to_rfc3339()
        );
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                use std::io::Write;
                let _ = file.write_all(owner.as_bytes());
                Some(Self { path })
            }
            Err(_) => None,
        }
    }

    fn owner(data_dir: &Path) -> Option<String> {
        std::fs::read_to_string(lock_path(data_dir)).ok()
    }
}

impl Drop for DreamLockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

// ── Входные позиции и материализация ─────────────────────────────────

/// Одна материализованная входная позиция прогона.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DreamInput {
    /// Класс источника: contested | proposal | episode | revalidate.
    pub source: &'static str,
    /// Ключ-идентификатор позиции (для ссылок и снапшота).
    pub key: String,
    /// Content-hash дельты: уже предъявленные позиции пропускаются.
    pub hash: String,
    /// Регидратированный сниппет (из живой строки memories).
    pub excerpt: String,
    /// Durable-ссылки позиции (перед подачей мёртвые вычищены).
    pub refs: Vec<String>,
}

/// План прогона: всё синхронное уже сделано, осталось позвать модель
/// (единственный await) и применить вывод.
pub(crate) struct DreamPlan {
    pub agent: String,
    pub prompt: String,
    /// Лимит позиций прогона (парсинг вывода модели тем же потолком).
    pub max_ops: usize,
    /// Предъявленные входы → в seen_hashes после успеха.
    pub input_hashes: Vec<String>,
    /// Остаток под лимитом → в pending_hashes (остаётся в дельте).
    pub leftover_hashes: Vec<String>,
    pub report: DreamReport,
    /// Guard lock-файла: держит третий вентиль закрытым от взятия до конца
    /// прогона (Drop освобождает при любом исходе).
    _lock: DreamLockGuard,
}

pub(crate) enum TickOutcome {
    Idle,
    Run(DreamPlan),
}

/// Сигнальные счётчики дельты: новые строки с последнего прогона
/// (memories + proposals per-agent, receipts — глобальные события плоскости).
fn delta_signals(conn: &rusqlite::Connection, agent: &str, since: &str) -> Result<u64> {
    let memories: i64 = conn.query_row(
        "SELECT COUNT(*) FROM memories WHERE created_at > ?1 \
         AND (SELECT alias FROM agents WHERE agents.id = memories.agent_id) = ?2",
        rusqlite::params![since, agent],
        |row| row.get(0),
    )?;
    let proposals: i64 = conn.query_row(
        "SELECT COUNT(*) FROM proposals WHERE created_at > ?1 AND (agent = ?2 OR agent IS NULL)",
        rusqlite::params![since, agent],
        |row| row.get(0),
    )?;
    let receipts: i64 = conn.query_row(
        "SELECT COUNT(*) FROM ratification_receipts WHERE created_at > ?1",
        rusqlite::params![since],
        |row| row.get(0),
    )?;
    Ok(u64::try_from(memories.max(0) + proposals.max(0) + receipts.max(0)).unwrap_or(u64::MAX))
}

/// Подтверждающие сигналы входа (пред-гейт до модели): сама позиция (1) +
/// живые durable-ссылки на неё из memory_refs/proposal_refs. Минимум
/// `min_confirming_signals` — иначе позиция даже не предлагается (OQ-4).
fn confirming_signals(conn: &rusqlite::Connection, keys: &[String]) -> Result<u32> {
    let mut signals = 0u32;
    for key in keys {
        let live: i64 = conn.query_row(
            "SELECT COUNT(*) FROM memories WHERE key = ?1 AND superseded_by IS NULL",
            rusqlite::params![key],
            |row| row.get(0),
        )?;
        if live == 0 {
            continue;
        }
        signals += 1;
        let refs: i64 = conn.query_row(
            "SELECT (SELECT COUNT(*) FROM memory_refs WHERE target_key = ?1) \
                   + (SELECT COUNT(*) FROM proposal_refs WHERE ref_key = ?1)",
            rusqlite::params![key],
            |row| row.get(0),
        )?;
        signals += u32::try_from(refs.max(0)).unwrap_or(u32::MAX);
    }
    Ok(signals)
}

/// Регидратация ссылки: живая (не-суперседнутая) строка по ключу; сниппет
/// ограничен. Уставшие/удалённые ссылки возвращают None и вычитаются.
fn hydrate_ref(conn: &rusqlite::Connection, key: &str) -> Result<Option<String>> {
    let row: Option<(String,)> = conn
        .query_row(
            "SELECT content FROM memories WHERE key = ?1 AND superseded_by IS NULL LIMIT 1",
            rusqlite::params![key],
            |row| Ok((row.get(0)?,)),
        )
        .ok();
    Ok(row.map(|(content,)| content.chars().take(HYDRATE_EXCERPT_CHARS).collect()))
}

/// Кандидаты до фильтров: (source, key, hash, refs).
struct Candidate {
    source: &'static str,
    key: String,
    hash: String,
    refs: Vec<String>,
}

fn collect_candidates(
    conn: &rusqlite::Connection,
    config: &Config,
    agent: &str,
) -> Result<Vec<Candidate>> {
    let dream = &config.memory.dream;
    let mut candidates = Vec::new();

    // 1. Контестед-кластеры (SQL + детерминированная кластеризация).
    for cluster in conflict::contested_clusters(conn, Some(agent), CANDIDATE_PER_CLASS)? {
        let hash = proposals::source_hash(&["contested", &cluster.keys.join("|")]);
        candidates.push(Candidate {
            source: "contested",
            key: cluster.keys.join("+"),
            hash,
            refs: cluster.keys,
        });
    }

    // 2. Нерассмотренные proposals (своя плоскость + shared NULL; чужой
    //    agent-scope не читаем по построению SQL).
    {
        let mut stmt = conn.prepare(
            "SELECT source_hash, COALESCE(kind || ': ' || substr(payload, 1, 200), kind) \
             FROM proposals WHERE status = 'pending' AND (agent = ?1 OR agent IS NULL) \
             ORDER BY id LIMIT ?2",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![agent, CANDIDATE_PER_CLASS as i64],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?;
        for row in rows {
            let (source_hash, preview) = row?;
            candidates.push(Candidate {
                source: "proposal",
                key: preview,
                hash: proposals::source_hash(&["proposal", &source_hash]),
                refs: Vec::new(),
            });
        }
    }

    // 3. Свежие эпизоды окна (daily/conversation за min_interval_days).
    {
        let cutoff = (chrono::Local::now()
            - chrono::Duration::days(i64::from(dream.min_interval_days.max(1))))
        .to_rfc3339();
        let mut stmt = conn.prepare(
            "SELECT key, content FROM memories \
             WHERE category IN ('daily', 'conversation') AND created_at > ?1 \
             AND (SELECT alias FROM agents WHERE agents.id = memories.agent_id) = ?2 \
             ORDER BY created_at DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![cutoff, agent, CANDIDATE_PER_CLASS as i64],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?;
        for (key, content) in rows.collect::<Result<Vec<_>, _>>()? {
            let hash = proposals::source_hash(&["episode", &key]);
            let excerpt: String = content.chars().take(HYDRATE_EXCERPT_CHARS).collect();
            // Эпизод — сам себе позиция: excerpt подставляется при сборке.
            candidates.push(Candidate {
                source: "episode",
                key: format!("{key}\u{1}{excerpt}"),
                hash,
                refs: vec![key],
            });
        }
    }

    // 4. Очередь ревалидации принятых фактов (VALIDATE/REFUTE с равным
    //    статусом; non-destructive версионность применяется в apply_signed).
    for key in proposals::revalidate_queue(
        conn,
        dream.revalidate_days,
        Some(agent),
        CANDIDATE_PER_CLASS,
    )? {
        let hash = proposals::source_hash(&["revalidate", &key]);
        candidates.push(Candidate {
            source: "revalidate",
            key: key.clone(),
            hash,
            refs: vec![key],
        });
    }
    Ok(candidates)
}

/// Материализация входов с фильтрами: content-hash дельта (уже освещён —
/// skip), регидратация (мёртвые ссылки вычищаются; позиция без живых ссылок
/// Материализация входов с фильтрами: content-hash дельта (уже освещён —
/// skip), регидратация (мёртвые ссылки вычищаются; позиция без живых ссылок
/// пропускается), пред-гейт сигналов, лимит позиций (остаток в дельту).
fn materialize_inputs(
    conn: &rusqlite::Connection,
    config: &Config,
    agent: &str,
    agent_state: &AgentDreamState,
    report: &mut DreamReport,
) -> Result<(Vec<DreamInput>, Vec<String>)> {
    let dream = &config.memory.dream;
    let mut inputs = Vec::new();
    let mut leftover = Vec::new();

    for candidate in collect_candidates(conn, config, agent)? {
        // Content-hash дельта: предъявленное (seen или pending) не предъявляем.
        if agent_state.seen_hashes.contains(&candidate.hash)
            || agent_state.pending_hashes.contains(&candidate.hash)
        {
            report.skipped_seen += 1;
            continue;
        }

        // Регидратация refs из живых строк; мёртвые ссылки вычитаются.
        let mut live_refs = Vec::new();
        let mut excerpt_parts: Vec<String> = Vec::new();
        for reference in &candidate.refs {
            if let Some(snippet) = hydrate_ref(conn, reference)? {
                live_refs.push(reference.clone());
                excerpt_parts.push(format!("{reference}: {snippet}"));
            }
        }
        if !candidate.refs.is_empty() && live_refs.is_empty() {
            report.skipped_dead_refs += 1;
            continue;
        }

        // Пред-гейт подтверждающих сигналов (revalidate — факт уже ратифицирован
        // оператором, даём 2 сигнала по построению).
        let signals = if candidate.source == "revalidate" {
            u32::max(2, dream.min_confirming_signals)
        } else if candidate.source == "episode" {
            // Эпизод — сам себе позиция: ключ без префикса-сниппета.
            let (key, _) = candidate
                .key
                .split_once('\u{1}')
                .unwrap_or((candidate.key.as_str(), ""));
            confirming_signals(conn, &[key.to_string()])?
        } else {
            confirming_signals(conn, &live_refs)?
        };
        if signals < dream.min_confirming_signals {
            report.skipped_below_gate += 1;
            continue;
        }

        // Лимит позиций: остаток входов остаётся в дельте (pending_hashes).
        if inputs.len() >= dream.max_revisions_per_run as usize {
            leftover.push(candidate.hash);
            continue;
        }

        let (key, excerpt) = if candidate.source == "episode" {
            // Эпизод несёт собственный сниппет (см. collect_candidates).
            let (key, excerpt) = candidate
                .key
                .split_once('\u{1}')
                .map(|(k, e)| (k.to_string(), e.to_string()))
                .unwrap_or_else(|| (candidate.key.clone(), String::new()));
            (key, excerpt)
        } else {
            (candidate.key.clone(), String::new())
        };
        if !excerpt.is_empty() {
            excerpt_parts.push(excerpt);
        }
        inputs.push(DreamInput {
            source: candidate.source,
            key,
            hash: candidate.hash,
            excerpt: excerpt_parts.join("\n---\n"),
            refs: live_refs,
        });
    }
    report.over_limit = leftover.len() as u64;
    Ok((inputs, leftover))
}
// ── Pinned-промпт ────────────────────────────────────────────────────

/// Собрать pinned-промпт прогона: материал + строгий JSON-контракт.
/// Модель переоценивает отобранное один раз; операции, не проза.
fn build_prompt(config: &Config, agent: &str, inputs: &[DreamInput]) -> String {
    let dream = &config.memory.dream;
    let mut material = String::new();
    for input in inputs {
        material.push_str(&format!(
            "\n## [{}] {}\nrefs: {}\n{}\n",
            input.source,
            input.key,
            input.refs.join(", "),
            input.excerpt
        ));
    }
    format!(
        "[dream:{agent}] You are the nightly memory re-evaluation pass of this agent. \
Re-evaluate ONLY the material below and return a strict JSON array of revision \
operations — operations, not prose.\n\
MATERIAL:{material}\n\
OUTPUT CONTRACT — return ONLY a JSON array (no markdown fences, no commentary) \
of at most {max} objects, each exactly:\n\
{{\"action\":\"<one of: promote|demote|link|archive|supersede|merge|validate|refute>\",\
\"target\":\"<memory key of an existing judgment; required except promote/link>\",\
\"content\":\"<new version text; required for promote/supersede/merge/refute>\",\
\"rationale\":\"<required justification grounded in the material excerpts>\",\
\"refs\":[\"<keys of the supporting excerpts>\"]}}\n\
RULES:\n\
- validate: fresh episodes confirm the targeted fact — extends it.\n\
- refute: fresh episodes contradict it — new version via non-destructive supersession.\n\
- supersede/merge/archive are reversible: the old version is superseded, never deleted.\n\
- promote raises a confirmed judgment into durable form; demote lowers its weight.\n\
- IDENTITY.md/SOUL.md are mission-axis files: read-only. If a judgment contradicts \
the axis, express it as a refute/supersede operation on the judgment itself — \
never as a file edit.\n\
- Every operation MUST cite refs from the material and a concrete rationale.\n\
- Keep the whole answer within ~{budget} tokens.",
        max = dream.max_revisions_per_run,
        budget = dream.token_budget,
    )
}

// ── Antidivergence-пробник ───────────────────────────────────────────

/// Дымовой запрос: живая выдача по ключу (exact key + content match).
fn smoke_query(conn: &rusqlite::Connection, key: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT key FROM memories \
         WHERE superseded_by IS NULL AND (key = ?1 OR content LIKE ('%' || ?1 || '%')) \
         ORDER BY updated_at DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![key, SMOKE_QUERY_LIMIT], |row| row.get(0))?;
    Ok(rows.collect::<Result<Vec<String>, _>>()?)
}

/// Пробник применённых пачек: пакеты, подписанные с прошлого прогона,
/// дымовые запросы по proposal_refs до/после (снапшот против текущей
/// выдачи). Регресс → drift_events += 1, пакет contested в отчёте.
/// Не блокирует: данные для оператора (исследование, предсказание В6).
fn probe_accepted_batches(
    conn: &rusqlite::Connection,
    agent: &str,
    agent_state: &mut AgentDreamState,
) -> Result<u64> {
    let initializing = agent_state.last_probe_at.is_none();
    let since = agent_state
        .last_probe_at
        .clone()
        .unwrap_or_else(|| "1970-01-01T00:00:00+00:00".to_string());
    let mut stmt = conn.prepare(
        "SELECT DISTINCT p.id FROM proposals p \
         WHERE p.status = 'accepted' AND p.resolved_at > ?1 AND (p.agent = ?2 OR p.agent IS NULL)",
    )?;
    let proposal_ids: Vec<i64> = stmt
        .query_map(rusqlite::params![since, agent], |row| row.get(0))?
        .collect::<Result<Vec<_>, _>>()?;

    let mut drift = 0u64;
    let mut accepted_revisions = 0u64;
    let mut new_snapshot = agent_state.probe_snapshot.clone();
    if initializing {
        // Базис «до»: дымовые запросы по всем живым core-ключам —
        // без этого регресс первой же применённой пачки не с чем сравнивать.
        let mut keys_stmt = conn.prepare(
            "SELECT key FROM memories WHERE superseded_by IS NULL \
             ORDER BY updated_at DESC LIMIT 500",
        )?;
        let keys: Vec<String> = keys_stmt
            .query_map([], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        for key in keys {
            new_snapshot.insert(key.clone(), smoke_query(conn, &key)?);
        }
    }
    for id in proposal_ids {
        let kind: String = conn.query_row(
            "SELECT kind FROM proposals WHERE id = ?1",
            rusqlite::params![id],
            |row| row.get(0),
        )?;
        if kind == RatificationProposal::KIND_REVISION && !initializing {
            accepted_revisions += 1;
        }
        let mut refs_stmt =
            conn.prepare("SELECT ref_key FROM proposal_refs WHERE proposal_id = ?1")?;
        let refs: Vec<String> = refs_stmt
            .query_map(rusqlite::params![id], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        for reference in refs {
            let hits = smoke_query(conn, &reference)?;
            // Инициализация базиса: только снапшот, сравнения нет (иначе
            // исторические пачки дали бы ложный дрейф «до» отсутствует).
            if !initializing && let Some(before) = new_snapshot.get(&reference) {
                let regression: Vec<String> = before
                    .iter()
                    .filter(|key| !hits.contains(key))
                    .cloned()
                    .collect();
                if !regression.is_empty() {
                    drift += 1;
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({
                                "agent": agent,
                                "proposal": id,
                                "ref": reference,
                                "regressed_keys": regression.join(", "),
                            })),
                        "dream antidivergence probe: recall regression after ratified batch; batch contested"
                    );
                }
            }
            new_snapshot.insert(reference, hits);
        }
    }
    agent_state.probe_snapshot = new_snapshot;
    agent_state.last_probe_at = Some(chrono::Local::now().to_rfc3339());
    agent_state.counters.revisions_accepted += accepted_revisions;
    agent_state.counters.drift_events += drift;
    Ok(drift)
}

// ── Тик worker'а (синхронный разрез) ─────────────────────────────────

/// Проверить вентили: время × объём × lock. Возвращает Some(guard), когда
/// окно открыто.
fn valves_open(
    config: &Config,
    conn: &rusqlite::Connection,
    agent: &str,
    agent_state: &AgentDreamState,
) -> Result<Option<DreamLockGuard>> {
    let dream = &config.memory.dream;
    // Вентиль 1: минимальный интервал с последнего УСПЕШНОГО прогона.
    if let Some(last_run) = &agent_state.last_run_at {
        let last = chrono::DateTime::parse_from_rfc3339(last_run)
            .map(|parsed| parsed.with_timezone(&chrono::Local))
            .unwrap_or_else(|_| {
                chrono::Local::now()
                    - chrono::Duration::days(i64::from(dream.min_interval_days) + 1)
            });
        if chrono::Local::now() - last < chrono::Duration::days(i64::from(dream.min_interval_days))
        {
            return Ok(None);
        }
    }
    // Пауза после упавшего прогона (retry окном, а не каждым тиком).
    // Успех после падения (last_run_at >= last_attempt_at) паузу снимает.
    if let (Some(_run), Some(last_attempt)) =
        (&agent_state.last_run_at, &agent_state.last_attempt_at)
        && agent_state.last_run_at.as_deref() < Some(last_attempt.as_str())
        && let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(last_attempt)
    {
        let elapsed = chrono::Local::now() - parsed.with_timezone(&chrono::Local);
        if elapsed < chrono::Duration::from_std(FAILURE_BACKOFF).unwrap_or_default() {
            return Ok(None);
        }
    }
    // Вентиль 2: объём дельты (новые сигналы + остаток прошлого прогона).
    let since = agent_state
        .last_run_at
        .clone()
        .unwrap_or_else(|| "1970-01-01T00:00:00+00:00".to_string());
    let signals = delta_signals(conn, agent, &since)? + agent_state.pending_hashes.len() as u64;
    if signals < u64::from(dream.delta_threshold) {
        return Ok(None);
    }
    // Вентиль 3: lock против параллельных прогонов/пайплайнов.
    let Some(guard) = DreamLockGuard::acquire(&config.data_dir) else {
        ::zeroclaw_log::record!(
            DEBUG,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(
                ::serde_json::json!({
                    "owner": DreamLockGuard::owner(&config.data_dir).unwrap_or_default(),
                })
            ),
            "dream lock held; skipping the window, retry later"
        );
        return Ok(None);
    };
    Ok(Some(guard))
}

/// Синхронный тик: пробник + вентили + материализация. Возвращает план
/// прогона (модель зовётся caller'ом — единственный раз за прогон) или
/// Idle. Пустой прогон (нет материала) — запись завершения в state,
/// ноль файлов/записей, Idle.
pub(crate) fn worker_tick_sync(
    config: &Config,
    conn: &rusqlite::Connection,
    agent: &str,
    state: &mut DreamState,
) -> Result<TickOutcome> {
    if !config.memory.dream.enabled {
        return Ok(TickOutcome::Idle);
    }
    let agent_state = state.agents.entry(agent.to_string()).or_default();
    let mut report = DreamReport {
        run_at: chrono::Local::now().to_rfc3339(),
        ..DreamReport::default()
    };

    // Фаза 1 (каждый тик, без модели): antidivergence-пробник применённых
    // пачек. Не блокирует прогон; регресс — WARN + счётчик.
    let drift = probe_accepted_batches(conn, agent, agent_state)?;
    report.drift = drift;

    // Фаза 2: вентили. Закрыты — Idle (снапшот пробника уже обновлён).
    let Some(lock) = valves_open(config, conn, agent, agent_state)? else {
        return Ok(TickOutcome::Idle);
    };

    // Фаза 3: материализация входов (model-free).
    let (inputs, leftover) = materialize_inputs(conn, config, agent, agent_state, &mut report)?;
    agent_state.counters.revalidations += inputs
        .iter()
        .filter(|input| input.source == "revalidate")
        .count() as u64;

    // Пустой прогон: запись завершения в state, ноль побочных эффектов.
    if inputs.is_empty() {
        agent_state.last_run_at = Some(report.run_at.clone());
        agent_state.run_count += 1;
        agent_state.last_report = Some(report);
        return Ok(TickOutcome::Idle);
    }

    let prompt = build_prompt(config, agent, &inputs);
    let input_hashes = inputs.iter().map(|input| input.hash.clone()).collect();
    Ok(TickOutcome::Run(DreamPlan {
        agent: agent.to_string(),
        prompt,
        max_ops: config.memory.dream.max_revisions_per_run as usize,
        input_hashes,
        leftover_hashes: leftover,
        report,
        _lock: lock,
    }))
}

/// Применить вывод модели: разбор строгого JSON, propose revision-операций
/// в очередь 9-4, продвижение state (только после успеха — упавший прогон
/// сюда не доходит и дельту не снимает).
pub(crate) fn apply_dream_output(
    conn: &rusqlite::Connection,
    plan: &DreamPlan,
    model_output: &str,
    state: &mut DreamState,
) -> Result<u32> {
    let agent_state = state.agents.entry(plan.agent.clone()).or_default();
    let run_id = format!("{}:{}", plan.agent, plan.report.run_at);
    let mut proposed = 0u32;
    match proposals::parse_revision_ops(model_output, plan.max_ops) {
        Ok(ops) => {
            for (idx, op) in ops.iter().enumerate() {
                let proposal =
                    proposals::revision_proposal(Some(&plan.agent), op, &format!("{run_id}#{idx}"));
                if proposals::propose(conn, &proposal)?.created {
                    proposed += 1;
                }
            }
        }
        Err(e) => {
            // Категория отказа: невалидный вывод. Прогон засчитан (дельта
            // предъявлена модели), операций ноль.
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"error": format!("{e}")})),
                "dream run returned no valid JSON operations"
            );
            agent_state.last_report = Some(DreamReport {
                invalid_output: true,
                ..plan.report.clone()
            });
        }
    }
    agent_state
        .seen_hashes
        .extend(plan.input_hashes.iter().cloned());
    if agent_state.seen_hashes.len() > SEEN_HASH_CAP {
        let excess = agent_state.seen_hashes.len() - SEEN_HASH_CAP;
        agent_state.seen_hashes.drain(0..excess);
    }
    agent_state.pending_hashes = plan.leftover_hashes.clone();
    agent_state.last_run_at = Some(plan.report.run_at.clone());
    agent_state.run_count += 1;
    agent_state.counters.revisions_proposed += u64::from(proposed);
    if !agent_state
        .last_report
        .as_ref()
        .is_some_and(|report| report.invalid_output)
    {
        agent_state.last_report = Some(DreamReport {
            proposed: u64::from(proposed),
            ..plan.report.clone()
        });
    }
    Ok(proposed)
}

/// Отметить упавший прогон: last_attempt для паузы ретрая; дельта
/// (seen/pending/last_run) не продвигается.
pub(crate) fn record_failed_attempt(state: &mut DreamState, agent: &str) {
    let agent_state = state.agents.entry(agent.to_string()).or_default();
    agent_state.last_attempt_at = Some(chrono::Local::now().to_rfc3339());
}

// ── Вызов модели (pinned-прогон, гард пустого toolset) ───────────────

/// Транспорт вызова модели: промпт → сырой вывод. Инъекция замыканием —
/// worker-тесты зовут пробник без оркестратора.
type DreamModel = dyn Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send>>
    + Send
    + Sync;

/// Гард прогона сна (matrix row «осевой файл»): пустой toolset — file tools
/// недоступны, memory-free — прямых записей в memories из сна нет; правки
/// осевых файлов остаются текстом в выводе. Отдельная функция — чтобы
/// тест мог зафиксировать гарды без запуска модели.
fn dream_run_overrides() -> AgentRunOverrides {
    AgentRunOverrides {
        security: None, // ставит production_model под политикой агента
        memory: None,
        is_subagent: false,
        suppress_memory_inject: true,
        suppress_memory_auto_save: true,
        memory_free: true,
        mcp_registry: None,
        execution_admission: None,
        internal_principal: Some(zeroclaw_api::ingress::InternalPrincipal::Cron {
            job_id: "dream".to_string(),
            job_name: None,
        }),
        sop_step_scope: None,
        ..AgentRunOverrides::default()
    }
}

/// Пустой белый список инструментов прогона сна.
fn dream_allowed_tools() -> Option<Vec<String>> {
    Some(Vec::new())
}

/// Прод-реализация: один pinned-прогон агентом агента (континуитет, OQ-2 —
/// тот же агент/модель; отдельный sleep-агент исключён по построению).
/// Гарды — [`dream_run_overrides`] + [`dream_allowed_tools`].
fn production_model(
    config: Config,
    agent: String,
    execution_capability: Option<crate::live_config_authority::AgentExecutionCapability>,
) -> Arc<DreamModel> {
    Arc::new(move |prompt: String| {
        let config = config.clone();
        let agent = agent.clone();
        let execution_capability = execution_capability.clone();
        Box::pin(async move {
            // Живой конфиг capability (как scheduler): прогон видит
            // последние admitted-изменения, а не снапшот старта worker'а.
            let config = execution_capability
                .as_ref()
                .map(|capability| {
                    capability
                        .capture_selection()
                        .config_handle()
                        .read()
                        .clone()
                })
                .unwrap_or(config);
            let security = crate::security::SecurityPolicy::for_agent(&config, &agent)?;
            let mut run_overrides = dream_run_overrides();
            run_overrides.security = Some(Arc::new(security));
            if let Some(zeroclaw_api::ingress::InternalPrincipal::Cron { job_name, .. }) =
                run_overrides.internal_principal.as_mut()
            {
                *job_name = Some(format!("dream-{agent}"));
            }
            let session_path = PathBuf::from(format!("dream-{}", uuid::Uuid::new_v4()));
            let temperature = config
                .model_provider_for_agent(&agent)
                .and_then(|entry| entry.temperature);
            crate::agent::run(
                config,
                &agent,
                Some(prompt),
                None,
                None,
                temperature,
                vec![],
                false,
                Some(session_path),
                dream_allowed_tools(),
                zeroclaw_api::ingress::TurnOrigin::Cron,
                run_overrides,
            )
            .await
        })
    })
}

/// Агенты контура сна: настроенные агенты; пустой конфиг — default.
fn dream_agents(config: &Config) -> Vec<String> {
    let mut agents: Vec<String> = config.agents.keys().cloned().collect();
    if agents.is_empty() {
        agents.push("default".to_string());
    }
    agents.sort();
    agents
}

/// Рабочий компонент dream-worker (daemon supervisor): тик — пробник +
/// вентили; прогон — ровно один вызов модели на агента.
pub async fn run_dream_worker(
    config: Config,
    cancel: tokio_util::sync::CancellationToken,
    execution_capability: Option<crate::live_config_authority::AgentExecutionCapability>,
) -> Result<()> {
    let agents = dream_agents(&config);
    let mut ticker = tokio::time::interval(WORKER_TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return Ok(()),
            _ = ticker.tick() => {}
        }
        // Гейт enabled на каждом тике (live-config flip, байт-в-байт при выкл).
        if !config.memory.dream.enabled {
            continue;
        }
        let db = proposals::proposals_db_path(&config.data_dir);
        if !db.exists() {
            continue;
        }
        for agent in &agents {
            let mut state = load_state(&config.data_dir);
            let plan = {
                let Ok(conn) = rusqlite::Connection::open(&db) else {
                    continue;
                };
                if conn
                    .busy_timeout(proposals::PROPOSALS_BUSY_TIMEOUT)
                    .is_err()
                {
                    continue;
                }
                match worker_tick_sync(&config, &conn, agent, &mut state) {
                    Ok(outcome) => outcome,
                    Err(e) => {
                        ::zeroclaw_log::record!(
                            DEBUG,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Note
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(
                                ::serde_json::json!({"agent": agent, "error": format!("{e}")})
                            ),
                            "dream worker tick failed"
                        );
                        continue;
                    }
                }
            };
            let TickOutcome::Run(plan) = plan else {
                if let Err(e) = save_state(&config.data_dir, &state) {
                    ::zeroclaw_log::record!(
                        DEBUG,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({"error": format!("{e}")})),
                        "dream state write failed"
                    );
                }
                continue;
            };
            // Ровно один вызов модели за прогон.
            let model =
                production_model(config.clone(), agent.clone(), execution_capability.clone());
            match (model)(plan.prompt.clone()).await {
                Ok(output) => {
                    let apply_result = {
                        let Ok(conn) = rusqlite::Connection::open(&db) else {
                            record_failed_attempt(&mut state, agent);
                            let _ = save_state(&config.data_dir, &state);
                            continue;
                        };
                        let _ = conn.busy_timeout(proposals::PROPOSALS_BUSY_TIMEOUT);
                        apply_dream_output(&conn, &plan, &output, &mut state)
                    };
                    match apply_result {
                        Ok(proposed) => {
                            ::zeroclaw_log::record!(
                                INFO,
                                ::zeroclaw_log::Event::new(
                                    module_path!(),
                                    ::zeroclaw_log::Action::Note
                                )
                                .with_attrs(
                                    ::serde_json::json!({"agent": agent, "proposed": proposed})
                                ),
                                "dream run completed"
                            );
                        }
                        Err(e) => {
                            ::zeroclaw_log::record!(
                                WARN,
                                ::zeroclaw_log::Event::new(
                                    module_path!(),
                                    ::zeroclaw_log::Action::Note
                                )
                                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                                .with_attrs(
                                    ::serde_json::json!({"agent": agent, "error": format!("{e}")})
                                ),
                                "dream run apply failed; delta not advanced"
                            );
                            record_failed_attempt(&mut state, agent);
                        }
                    }
                }
                Err(e) => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(
                                ::serde_json::json!({"agent": agent, "error": format!("{e}")})
                            ),
                        "dream run model call failed; delta not advanced"
                    );
                    record_failed_attempt(&mut state, agent);
                }
            }
            if let Err(e) = save_state(&config.data_dir, &state) {
                ::zeroclaw_log::record!(
                    DEBUG,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"error": format!("{e}")})),
                    "dream state write failed"
                );
            }
        }
    }
}

// ── Тесты ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use zeroclaw_config::schema::{Config, MemoryConfig, MemoryDreamConfig};

    fn dream_on() -> MemoryDreamConfig {
        MemoryDreamConfig {
            enabled: true,
            ..MemoryDreamConfig::default()
        }
    }

    fn config_with(tmp: &tempfile::TempDir, dream: MemoryDreamConfig) -> Config {
        let mut config = Config::default();
        config.data_dir = tmp.path().to_path_buf();
        config.memory = MemoryConfig {
            dream,
            ..MemoryConfig::default()
        };
        config
    }

    fn db_conn(tmp: &tempfile::TempDir) -> rusqlite::Connection {
        let mem = zeroclaw_memory::sqlite::SqliteMemory::new("test", tmp.path()).unwrap();
        drop(mem);
        let db = proposals::proposals_db_path(tmp.path());
        rusqlite::Connection::open(db).unwrap()
    }

    fn insert_memory(conn: &rusqlite::Connection, key: &str, content: &str, category: &str) {
        let now = chrono::Local::now().to_rfc3339();
        conn.execute(
            "INSERT INTO memories (
                id, key, content, category, created_at, updated_at,
                session_id, namespace, importance, agent_id, kind, pinned, tenant_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, 'default', 0.5,
                    (SELECT id FROM agents WHERE alias = 'default' LIMIT 1),
                    NULL, 0, NULL)",
            rusqlite::params![
                uuid::Uuid::new_v4().to_string(),
                key,
                content,
                category,
                now,
                now
            ],
        )
        .unwrap();
    }

    /// Свежая дельта выше порога: три эпизода, у двоих есть ссылки
    /// (подтверждающие сигналы ≥ 2), третий — без (пред-гейт отсечёт).
    fn seed_delta(conn: &rusqlite::Connection) {
        insert_memory(
            conn,
            "daily_a",
            "deploys drift to Mondays and CI breaks",
            "daily",
        );
        insert_memory(
            conn,
            "daily_b",
            "deploys drift to Tuesdays with green CI",
            "daily",
        );
        insert_memory(
            conn,
            "daily_c",
            "completely unrelated topic about keyboards",
            "daily",
        );
        // Подтверждающие ссылки на daily_a/daily_b (сигналы 2+).
        conn.execute(
            "INSERT INTO memory_refs (source_key, target_key, created_at) VALUES ('x', 'daily_a', ?1)",
            rusqlite::params![chrono::Local::now().to_rfc3339()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO memory_refs (source_key, target_key, created_at) VALUES ('x', 'daily_b', ?1)",
            rusqlite::params![chrono::Local::now().to_rfc3339()],
        )
        .unwrap();
    }

    const AGENT: &str = "default";

    #[test]
    fn disabled_is_byte_identical_idle() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config_with(&tmp, MemoryDreamConfig::default());
        let conn = db_conn(&tmp);
        seed_delta(&conn);
        let mut state = DreamState::default();
        let outcome = worker_tick_sync(&config, &conn, AGENT, &mut state).unwrap();
        assert!(matches!(outcome, TickOutcome::Idle));
        assert_eq!(state, DreamState::default(), "disabled: state untouched");
        assert!(!lock_path(&config.data_dir).exists(), "no lock side effect");
    }

    #[test]
    fn dream_run_guard_is_empty_toolset_and_memory_free() {
        let overrides = dream_run_overrides();
        assert!(overrides.memory_free, "dream run must not write memories");
        assert!(
            overrides.suppress_memory_inject && overrides.suppress_memory_auto_save,
            "dream run must not inject or autosave memory"
        );
        assert!(
            overrides.mcp_registry.is_none(),
            "dream run carries no MCP surface"
        );
        assert!(
            dream_allowed_tools().is_some_and(|tools| tools.is_empty()),
            "empty allowlist: no file tools reachable from the dream"
        );
    }

    #[test]
    fn delta_below_threshold_skips_without_side_effects() {
        let tmp = tempfile::tempdir().unwrap();
        let mut dream = dream_on();
        dream.delta_threshold = 10;
        let config = config_with(&tmp, dream);
        let conn = db_conn(&tmp);
        seed_delta(&conn); // 3 сигнала < 10
        let mut state = DreamState::default();
        let outcome = worker_tick_sync(&config, &conn, AGENT, &mut state).unwrap();
        assert!(matches!(outcome, TickOutcome::Idle));
        let agent_state = state.agents.get(AGENT).expect("agent entry created");
        assert_eq!(agent_state.run_count, 0, "no run below the volume valve");
        assert!(!lock_path(&config.data_dir).exists());
    }

    #[test]
    fn min_interval_postpones_the_window() {
        let tmp = tempfile::tempdir().unwrap();
        let mut dream = dream_on();
        dream.min_interval_days = 30;
        let config = config_with(&tmp, dream);
        let conn = db_conn(&tmp);
        seed_delta(&conn);
        let mut state = DreamState::default();
        state.agents.insert(
            AGENT.to_string(),
            AgentDreamState {
                last_run_at: Some(chrono::Local::now().to_rfc3339()),
                ..AgentDreamState::default()
            },
        );
        let outcome = worker_tick_sync(&config, &conn, AGENT, &mut state).unwrap();
        assert!(
            matches!(outcome, TickOutcome::Idle),
            "time valve closed: run postponed"
        );
    }

    #[test]
    fn held_lock_skips_the_window() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config_with(&tmp, dream_on());
        let conn = db_conn(&tmp);
        seed_delta(&conn);
        // Другой прогон/пайплайн держит lock.
        std::fs::write(lock_path(&config.data_dir), "other-run").unwrap();
        let mut state = DreamState::default();
        let outcome = worker_tick_sync(&config, &conn, AGENT, &mut state).unwrap();
        assert!(matches!(outcome, TickOutcome::Idle));
        let agent_state = state.agents.get(AGENT).unwrap();
        assert_eq!(agent_state.run_count, 0);
    }

    #[test]
    fn empty_run_records_completion_with_zero_side_effects() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config_with(&tmp, dream_on());
        let conn = db_conn(&tmp);
        // Дельта есть (три свежих memories), материала ноль: daily/conversation
        // отсутствуют — кандидатов нет (core-строки токенно не пересекаются,
        // contested-кластер не образуется).
        insert_memory(&conn, "core_x", "alpha bravo", "core");
        insert_memory(&conn, "core_y", "charlie delta", "core");
        insert_memory(&conn, "core_z", "echo foxtrot", "core");
        let mut state = DreamState::default();
        let outcome = worker_tick_sync(&config, &conn, AGENT, &mut state).unwrap();
        assert!(matches!(outcome, TickOutcome::Idle));
        let agent_state = state.agents.get(AGENT).unwrap();
        assert_eq!(agent_state.run_count, 1, "empty run records completion");
        assert!(agent_state.last_run_at.is_some());
        assert_eq!(agent_state.seen_hashes.len(), 0);
        let stats = proposals::counts(&conn).unwrap();
        assert_eq!(stats.total(), 0, "empty run proposes nothing");
    }

    #[test]
    fn open_window_materializes_inputs_and_limits_positions() {
        let tmp = tempfile::tempdir().unwrap();
        let mut dream = dream_on();
        dream.max_revisions_per_run = 2;
        let config = config_with(&tmp, dream);
        let conn = db_conn(&tmp);
        seed_delta(&conn);
        // Четыре живых позиции: две войдут, остаток уйдёт в pending.
        insert_memory(
            &conn,
            "daily_d",
            "another drift note about CI runs",
            "daily",
        );
        conn.execute(
            "INSERT INTO memory_refs (source_key, target_key, created_at) VALUES ('x', 'daily_d', ?1)",
            rusqlite::params![chrono::Local::now().to_rfc3339()],
        )
        .unwrap();
        insert_memory(
            &conn,
            "daily_e",
            "yet another CI drift observation",
            "daily",
        );
        conn.execute(
            "INSERT INTO memory_refs (source_key, target_key, created_at) VALUES ('x', 'daily_e', ?1)",
            rusqlite::params![chrono::Local::now().to_rfc3339()],
        )
        .unwrap();
        let mut state = DreamState::default();
        let outcome = worker_tick_sync(&config, &conn, AGENT, &mut state).unwrap();
        let TickOutcome::Run(plan) = outcome else {
            panic!("window must open with material above the threshold");
        };
        assert_eq!(plan.input_hashes.len(), 2, "positions limited to 2");
        assert_eq!(plan.leftover_hashes.len(), 2, "rest stays in the delta");
        assert!(plan.prompt.contains("OUTPUT CONTRACT"));
        assert!(plan.prompt.contains("[episode]"));
        // daily_c без ссылок — пред-гейт; проверим категорию в отчёте после apply.
        drop(plan._lock);
        assert!(
            !lock_path(&config.data_dir).exists(),
            "lock released in any outcome"
        );
    }

    #[test]
    fn apply_proposes_revision_ops_and_advances_state_once() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config_with(&tmp, dream_on());
        let conn = db_conn(&tmp);
        seed_delta(&conn);
        let mut state = DreamState::default();
        let outcome = worker_tick_sync(&config, &conn, AGENT, &mut state).unwrap();
        let TickOutcome::Run(plan) = outcome else {
            panic!("window must open");
        };
        let ops = r#"[
          {"action":"supersede","target":"daily_a","content":"deploys drift to Tuesdays","rationale":"two fresh episodes contradict Mondays","refs":["daily_a","daily_b"]},
          {"action":"validate","target":"daily_b","rationale":"fresh episodes confirm Tuesday deploys","refs":["daily_b"]}
        ]"#;
        let proposed = apply_dream_output(&conn, &plan, ops, &mut state).unwrap();
        assert_eq!(proposed, 2);
        let stats = proposals::counts(&conn).unwrap();
        assert_eq!(stats.pending, 2, "revision proposals queue for 9-4");
        let kind: String = conn
            .query_row("SELECT kind FROM proposals LIMIT 1", [], |row| row.get(0))
            .unwrap();
        assert_eq!(kind, "revision");
        let agent_state = state.agents.get(AGENT).unwrap();
        assert_eq!(agent_state.counters.revisions_proposed, 2);
        assert!(agent_state.last_run_at.is_some());
        // Повторный прогон: входы освещены (content-hash), предложений нет.
        let outcome2 = worker_tick_sync(&config, &conn, AGENT, &mut state).unwrap();
        assert!(
            matches!(outcome2, TickOutcome::Idle),
            "min_interval valve now postpones"
        );
    }

    #[test]
    fn invalid_model_output_marks_report_and_proposes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config_with(&tmp, dream_on());
        let conn = db_conn(&tmp);
        seed_delta(&conn);
        let mut state = DreamState::default();
        let TickOutcome::Run(plan) = worker_tick_sync(&config, &conn, AGENT, &mut state).unwrap()
        else {
            panic!("window must open");
        };
        let proposed = apply_dream_output(&conn, &plan, "проза вместо JSON", &mut state).unwrap();
        assert_eq!(proposed, 0);
        let stats = proposals::counts(&conn).unwrap();
        assert_eq!(stats.total(), 0);
        let agent_state = state.agents.get(AGENT).unwrap();
        assert!(
            agent_state
                .last_report
                .as_ref()
                .is_some_and(|report| report.invalid_output),
            "invalid output recorded as a refusal category"
        );
        // Дельта всё равно снята прогоном (модель была вызвана), но
        // повторный прогон закрыт вентилем интервала.
        assert!(agent_state.last_run_at.is_some());
    }

    #[test]
    fn failed_run_does_not_advance_delta() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config_with(&tmp, dream_on());
        let conn = db_conn(&tmp);
        seed_delta(&conn);
        let mut state = DreamState::default();
        let TickOutcome::Run(plan) = worker_tick_sync(&config, &conn, AGENT, &mut state).unwrap()
        else {
            panic!("window must open");
        };
        drop(plan._lock); // модель упала — lock отпущен, state не применён
        record_failed_attempt(&mut state, AGENT);
        let agent_state = state.agents.get(AGENT).unwrap();
        assert!(agent_state.last_run_at.is_none(), "last_run not advanced");
        assert!(
            agent_state.seen_hashes.is_empty(),
            "seen delta not consumed"
        );
        assert!(
            agent_state.last_attempt_at.is_some(),
            "retry backoff recorded"
        );
    }

    #[test]
    fn per_agent_scope_reads_only_own_plane() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config_with(&tmp, dream_on());
        let conn = db_conn(&tmp);
        // Чужой plane: другой агент + его память.
        conn.execute(
            "INSERT INTO agents (id, alias, created_at) VALUES ('agent-other', 'other', ?1)",
            rusqlite::params![chrono::Local::now().to_rfc3339()],
        )
        .unwrap();
        let now = chrono::Local::now().to_rfc3339();
        conn.execute(
            "INSERT INTO memories (id, key, content, category, created_at, updated_at,
                session_id, namespace, importance, agent_id, kind, pinned, tenant_id)
             VALUES ('m1', 'other_a', 'other agent fact', 'core', ?1, ?1, NULL, 'default', 0.5,
                     'agent-other', NULL, 0, NULL)",
            rusqlite::params![now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO memories (id, key, content, category, created_at, updated_at,
                session_id, namespace, importance, agent_id, kind, pinned, tenant_id)
             VALUES ('m2', 'other_b', 'other agent fact variant', 'core', ?1, ?1, NULL, 'default', 0.5,
                     'agent-other', NULL, 0, NULL)",
            rusqlite::params![now],
        )
        .unwrap();
        // Контестед-кластер — только в чужом scope: свой агент его не видит.
        let clusters_own =
            zeroclaw_memory::conflict::contested_clusters(&conn, Some(AGENT), 10).unwrap();
        let clusters_other =
            zeroclaw_memory::conflict::contested_clusters(&conn, Some("other"), 10).unwrap();
        assert!(clusters_own.is_empty(), "own scope reads no foreign rows");
        assert_eq!(clusters_other.len(), 1, "other agent sees its cluster");
        // Своя дельта ниже порога (чужие строки не считаются).
        let signals = delta_signals(&conn, AGENT, "1970-01-01T00:00:00+00:00").unwrap();
        assert_eq!(signals, 0, "foreign rows are not own delta signals");
    }

    #[test]
    fn probe_detects_drift_after_batch_apply() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config_with(&tmp, dream_on());
        let conn = db_conn(&tmp);
        insert_memory(&conn, "core_fact", "deploys happen on Mondays", "core");
        // Базис пробника: дымовой запрос видит core_fact.
        let mut state = DreamState::default();
        let drift0 = probe_accepted_batches(
            &conn,
            AGENT,
            state.agents.entry(AGENT.to_string()).or_default(),
        )
        .unwrap();
        assert_eq!(drift0, 0, "first probe initializes the baseline");
        // Пачка принята (proposal + ref на core_fact).
        let proposal = proposals::revision_proposal(
            Some(AGENT),
            &proposals::RevisionOp {
                action: "archive".to_string(),
                target: Some("core_fact".to_string()),
                content: None,
                rationale: "stale".to_string(),
                refs: vec!["core_fact".to_string()],
            },
            "probe-test#0",
        );
        let id = proposals::propose(&conn, &proposal).unwrap().proposal_id;
        conn.execute(
            "UPDATE proposals SET status = 'accepted', resolved_at = ?1 WHERE id = ?2",
            rusqlite::params![chrono::Local::now().to_rfc3339(), id],
        )
        .unwrap();
        // Применение (как apply_signed archive-операции): факт скрыт sentinel'ом.
        conn.execute(
            "UPDATE memories SET superseded_by = ?1 WHERE key = 'core_fact'",
            rusqlite::params![zeroclaw_memory::ARCHIVED_SENTINEL],
        )
        .unwrap();
        // Переносим last_probe_at назад, чтобы пачка попала в окно пробника.
        state.agents.get_mut(AGENT).unwrap().last_probe_at =
            Some("1970-01-01T00:00:00+00:00".to_string());
        let drift =
            probe_accepted_batches(&conn, AGENT, state.agents.get_mut(AGENT).unwrap()).unwrap();
        assert_eq!(
            drift, 1,
            "recall regression after the batch is a drift event"
        );
        let agent_state = state.agents.get(AGENT).unwrap();
        assert_eq!(agent_state.counters.drift_events, 1);
        assert_eq!(agent_state.counters.revisions_accepted, 1);
        // Повторный пробник: снапшот обновлён, регресса больше нет.
        let drift2 =
            probe_accepted_batches(&conn, AGENT, state.agents.get_mut(AGENT).unwrap()).unwrap();
        assert_eq!(drift2, 0);
    }
}
