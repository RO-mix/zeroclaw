//! Pre-dispatch permission gate (контур RAM, epic 9-3, OQ-2 вариант Б).
//!
//! Origin не протекает в `Tool::execute` (единственный origin-aware путь —
//! memory_inject), поэтому матрица «категория × операция × origin» проверяется
//! здесь, в agent loop, ДО диспатча вызова:
//!
//! - destroy (memory_forget/memory_purge) на защищённых категориях
//!   (narrative/judgment) от агентских origin — отказ до исполнения,
//!   reason + подсказка оформить proposal;
//! - file_write/file_edit в осевые файлы воркспейса (IDENTITY.md/SOUL.md) от
//!   агентских origin при `axis_files_readonly` — отказ до записи на диск;
//! - memory_store kind=narrative от агентского origin — правка разрешена, но
//!   с receipt: quote grounding против IDENTITY.md (см. zeroclaw_memory::
//!   permissions). Вердикт пишется после успешного исполнения.
//!
//! Фича выключена дефолтом: `PermissionsGate::resolve` возвращает `None`,
//! раунд вызовов не трогается вообще — поведение байт-в-байт прежнее.
//! Операторский CLI не проходит через этот гейт (второй ключ).

use std::path::{Path, PathBuf};
use zeroclaw_api::ingress::TurnOrigin;
use zeroclaw_api::memory_traits::{ExportFilter, Memory, MemoryKind};
use zeroclaw_config::policy::SecurityPolicy;
use zeroclaw_config::schema::{Config, MemoryPermissionsConfig};
use zeroclaw_memory::permissions::{
    self, MemoryOperation, PermissionDecision, ProtectedCategory, QuoteVerdict,
};

/// Решение гейта для одного вызова инструмента.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CallGate {
    /// Исполнять как раньше.
    Allow,
    /// Отказ до исполнения; reason уходит агенту в tool result.
    Deny { reason: String },
    /// Исполнять, затем записать self-change receipt (quote grounding).
    Receipt { quote: Option<String> },
}

/// Receipt-план для вызова, прошедшего гейт с решением Receipt.
pub(crate) struct ReceiptPlan {
    /// Индекс вызова в раунде (call_idx исполнения).
    pub call_idx: usize,
    /// Цитата из аргументов вызова (может отсутствовать).
    pub quote: Option<String>,
}

/// Снимок конфига матрицы + origin + воркспейс для одного раунда.
pub(crate) struct PermissionsGate<'a> {
    cfg: &'a MemoryPermissionsConfig,
    origin: TurnOrigin,
    workspace_dir: Option<PathBuf>,
    /// Путь к brain.db для телеметрии отказов (epic 9-8, BH-13); `None` —
    /// конфиг неизвестен, счётчик не пишется.
    denial_db_path: Option<PathBuf>,
    /// Алиас агента для per-agent счётчика отказов.
    denial_agent: String,
}

impl<'a> PermissionsGate<'a> {
    /// Построить гейт. `None` — фича выключена или конфиг недоступен:
    /// вызывающий не гейтит ничего (дефолтное поведение байт-в-байт).
    pub(crate) fn resolve(
        config: Option<&'a Config>,
        origin: TurnOrigin,
        security: Option<&SecurityPolicy>,
        agent_alias: Option<&str>,
    ) -> Option<Self> {
        let cfg = &config?.memory.permissions;
        if !cfg.enabled {
            return None;
        }
        // Воркспейс — из политики tools (тот же источник, что используют
        // file tools), фолбэк — конфиг агента, затем data_dir. Нет ни
        // того ни другого — осевые файлы не гейтятся (вызовы памяти
        // гейтятся независимо).
        let workspace_dir = if let Some(security) = security {
            Some(security.workspace_dir.clone())
        } else if let (Some(config), Some(alias)) = (config, agent_alias) {
            Some(config.agent_workspace_dir(alias))
        } else {
            config.map(|c| c.data_dir.clone())
        };
        Some(Self {
            cfg,
            origin,
            workspace_dir,
            denial_db_path: config.map(|c| zeroclaw_memory::receipts::brain_db_path(&c.data_dir)),
            denial_agent: agent_alias.unwrap_or("unattributed").to_string(),
        })
    }

    /// Конфиг матрицы (для settle).
    pub(crate) fn config(&self) -> &'a MemoryPermissionsConfig {
        self.cfg
    }

    /// Проверить один вызов до диспатча. `memory` нужен для классификации
    /// существующих записей (forget по key / purge по scope); `security` —
    /// для резолва путей file-вызовов тем же способом, что file tools
    /// (`resolve_tool_path`), чтобы гейт и инструмент сходились в цели.
    /// `None` по любому из них — защищённость не проверяется, вызов
    /// пропускается (эпизодический слой агенту полностью — не гейтим то,
    /// что не можем классифицировать); fail-open ветки логируются (DEBUG).
    pub(crate) async fn check_call(
        &self,
        tool: &str,
        args: &serde_json::Value,
        memory: Option<&dyn Memory>,
        security: Option<&SecurityPolicy>,
    ) -> CallGate {
        let gate = match tool {
            "memory_store" => self.check_memory_store(args),
            "memory_forget" => self.check_memory_forget(args, memory).await,
            "memory_purge" => self.check_memory_purge(args, memory).await,
            "file_write" | "file_edit" => self.check_axis_file_write(args, security),
            _ => CallGate::Allow,
        };
        if matches!(gate, CallGate::Deny { .. }) {
            self.record_denial(tool).await;
        }
        gate
    }

    /// Телеметрия отказов (epic 9-8, BH-13): модель может ретраить deny
    /// каждую итерацию (denial probing) — каждый отказ инкрементирует
    /// атомарные счётчики в memory_meta и журналируется WARN со
    /// статичным английским текстом и error_key. Best-effort: сбой
    /// счётчика не меняет исход гейта. Запись в SQLite уезжает в
    /// `spawn_blocking` (ревью-виток 1, BH-3): блокирующий I/O в async
    /// turn loop недопустим.
    async fn record_denial(&self, tool: &str) {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({
                    "error_key": "permissions_denied",
                    "tool": tool,
                    "agent": self.denial_agent,
                })),
            "memory permissions: gate deny recorded (denial probing telemetry)"
        );
        let (Some(db_path), agent) = (self.denial_db_path.clone(), self.denial_agent.clone())
        else {
            return;
        };
        let result = tokio::task::spawn_blocking(move || {
            zeroclaw_memory::receipts::record_denial_at(&db_path, &agent)
        })
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"error": e.to_string()})),
                    "memory permissions: denial counter not recorded"
                );
            }
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"error": e.to_string()})),
                    "memory permissions: denial counter task failed"
                );
            }
        }
    }

    /// memory_store: только narrative-ветка проходит через матрицу; прочие
    /// категории — как раньше (объёмные квоты PolicyEnforcer сохраняются ниже).
    fn check_memory_store(&self, args: &serde_json::Value) -> CallGate {
        if args.get("kind").and_then(|v| v.as_str()) != Some("narrative") {
            return CallGate::Allow;
        }
        match permissions::decide(
            self.cfg,
            ProtectedCategory::Narrative,
            MemoryOperation::Store,
            self.origin,
        ) {
            PermissionDecision::Receipt { .. } => CallGate::Receipt {
                quote: args
                    .get("quote")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            },
            // Interactive origin (операторский второй ключ): без receipt.
            _ => CallGate::Allow,
        }
    }

    /// memory_forget: отказ, только если ключ существует И его kind защищён
    /// И матрица запрещает destroy для origin. Несуществующий ключ —
    /// «No memory found», гейтить нечего.
    async fn check_memory_forget(
        &self,
        args: &serde_json::Value,
        memory: Option<&dyn Memory>,
    ) -> CallGate {
        let Some(memory) = memory else {
            log_fail_open("memory_forget denied check skipped: no memory handle");
            return CallGate::Allow;
        };
        let decision = permissions::decide(
            self.cfg,
            ProtectedCategory::Narrative,
            MemoryOperation::Forget,
            self.origin,
        );
        let PermissionDecision::Deny { reason } = decision else {
            return CallGate::Allow;
        };
        let Some(key) = args.get("key").and_then(|v| v.as_str()) else {
            return CallGate::Allow;
        };
        let protected = memory
            .get(key)
            .await
            .ok()
            .flatten()
            .is_some_and(|entry| matches!(entry.kind, Some(MemoryKind::Narrative)));
        if protected {
            CallGate::Deny { reason }
        } else {
            CallGate::Allow
        }
    }

    /// memory_purge: отказ, только если scope содержит защищённые записи
    /// (проверка по export-фильтру — без изменения Memory-трейта).
    async fn check_memory_purge(
        &self,
        args: &serde_json::Value,
        memory: Option<&dyn Memory>,
    ) -> CallGate {
        let Some(memory) = memory else {
            log_fail_open("memory_purge denied check skipped: no memory handle");
            return CallGate::Allow;
        };
        let decision = permissions::decide(
            self.cfg,
            ProtectedCategory::Narrative,
            MemoryOperation::Purge,
            self.origin,
        );
        let PermissionDecision::Deny { reason } = decision else {
            return CallGate::Allow;
        };
        let filter = ExportFilter {
            namespace: args
                .get("namespace")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            session_id: args
                .get("session_id")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            ..Default::default()
        };
        if filter.namespace.is_none() && filter.session_id.is_none() {
            return CallGate::Allow;
        }
        let hits_protected = memory
            .export(&filter)
            .await
            .unwrap_or_default()
            .iter()
            .any(|entry| matches!(entry.kind, Some(MemoryKind::Narrative)));
        if hits_protected {
            CallGate::Deny { reason }
        } else {
            CallGate::Allow
        }
    }

    /// file_write/file_edit в IDENTITY.md/SOUL.md воркспейса от агентского
    /// origin при axis_files_readonly. Пути резолвятся через
    /// `SecurityPolicy::resolve_tool_path` — тем же способом, что file
    /// tools, чтобы гейт и инструмент не расходились в цели записи.
    fn check_axis_file_write(
        &self,
        args: &serde_json::Value,
        security: Option<&SecurityPolicy>,
    ) -> CallGate {
        if !self.cfg.axis_files_readonly || !permissions::is_agent_origin(self.origin) {
            return CallGate::Allow;
        }
        let Some(workspace) = self.workspace_dir.as_deref() else {
            log_fail_open("axis-file check skipped: no workspace dir");
            return CallGate::Allow;
        };
        let Some(path_arg) = args.get("path").and_then(|v| v.as_str()) else {
            log_fail_open("axis-file check skipped: no path argument");
            return CallGate::Allow;
        };
        // Тот же резолв, что в file_write.rs:175 / file_edit.rs:161.
        let resolved = security.map_or_else(
            || {
                let raw = Path::new(path_arg);
                if raw.is_absolute() {
                    raw.to_path_buf()
                } else {
                    workspace.join(raw)
                }
            },
            |policy| policy.resolve_tool_path(path_arg),
        );
        if permissions::is_axis_file_target(&resolved, workspace) {
            let file_name = resolved
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("axis file");
            ::zeroclaw_log::record!(
                DEBUG,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(::serde_json::json!({"tool": "file_write/file_edit", "path": resolved.to_string_lossy()})),
                "memory permissions: axis file write denied"
            );
            CallGate::Deny {
                reason: permissions::axis_file_denial_reason(file_name),
            }
        } else {
            CallGate::Allow
        }
    }
}

/// Fail-open ветки гейта: пропуск из-за отсутствия контекста (не решение
/// «разрешено») — auditable boundary обязана оставлять след (DEBUG-уровень,
/// чтобы не шуметь в обычном трафике).
fn log_fail_open(why: &str) {
    ::zeroclaw_log::record!(
        DEBUG,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
            .with_attrs(::serde_json::json!({"reason": why})),
        "memory permissions: gate fail-open"
    );
}

/// Заселить receipt'ы после исполнения раунда: для каждого плана, чей вызов
/// завершился успешно, — quote grounding против IDENTITY.md, запись в
/// brain.db и пометка вердикта в output агента. Ошибка записи не роняет ход.
///
/// Чтение IDENTITY.md и запись в SQLite уезжают в `spawn_blocking`
/// (ревью-виток 1, BH-3): блокирующий I/O в async turn loop недопустим.
pub(crate) async fn settle_receipt_plans(
    plans: &[ReceiptPlan],
    gate: &PermissionsGate<'_>,
    completed_indices: &[usize],
    completed_outcomes: &mut [crate::agent::tool_execution::CompletedToolExecution],
    config: &Config,
    agent_alias: Option<&str>,
) {
    if plans.is_empty() {
        return;
    }
    // Пассивное чтение success-флагов ДО мутации: состав джоб решаем здесь,
    // исполнение — в blocking-пуле, пометка output — после.
    let jobs: Vec<(usize, Option<String>)> = plans
        .iter()
        .filter(|plan| {
            completed_indices
                .iter()
                .position(|&i| i == plan.call_idx)
                .is_some_and(|pos| completed_outcomes[pos].outcome.success)
        })
        .map(|plan| (plan.call_idx, plan.quote.clone()))
        .collect();
    if jobs.is_empty() {
        return;
    }
    let identity_path = gate
        .workspace_dir
        .as_deref()
        .map(|ws| ws.join("IDENTITY.md"));
    let min_chars = gate.config().quote_min_chars;
    let db_path = zeroclaw_memory::receipts::brain_db_path(&config.data_dir);
    let agent = agent_alias.unwrap_or("default").to_string();
    let verdicts: Vec<(usize, zeroclaw_memory::permissions::QuoteVerdict)> =
        match tokio::task::spawn_blocking(move || {
            let identity_md = identity_path
                .as_deref()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .unwrap_or_default();
            let mut out = Vec::with_capacity(jobs.len());
            for (call_idx, quote) in jobs {
                let verdict = zeroclaw_memory::permissions::ground_quote(
                    &identity_md,
                    quote.as_deref(),
                    min_chars,
                );
                if let Err(e) = zeroclaw_memory::receipts::record_self_change_receipt_at(
                    &db_path,
                    &agent,
                    "narrative",
                    quote.as_deref(),
                    verdict,
                ) {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({"error": e.to_string()})),
                        "memory permissions: self-change receipt not recorded"
                    );
                }
                out.push((call_idx, verdict));
            }
            out
        })
        .await
        {
            Ok(v) => v,
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"error": e.to_string()})),
                    "memory permissions: receipt settle task failed"
                );
                return;
            }
        };
    for (call_idx, verdict) in verdicts {
        let Some(pos) = completed_indices.iter().position(|&i| i == call_idx) else {
            continue;
        };
        let outcome = &mut completed_outcomes[pos].outcome;
        // Гарантия разделителя (BH-12): вердикт не должен слипаться с
        // последним токеном output инструмента.
        if !outcome.output.is_empty() && !outcome.output.ends_with('\n') {
            outcome.output.push('\n');
        }
        outcome.output.push_str(&verdict_note(verdict, min_chars));
    }
}

/// Пометка вердикта в output инструмента (агент видит статус правки).
fn verdict_note(verdict: QuoteVerdict, min_chars: usize) -> String {
    match verdict {
        QuoteVerdict::Grounded => {
            "\nself-change receipt: grounded (quote matches IDENTITY.md exactly once)".to_string()
        }
        QuoteVerdict::ContestedMissing => format!(
            "\nself-change receipt: contested (no `quote` argument) — drift +1. Attach a \
             `quote` argument: an exact substring of IDENTITY.md, at least {min_chars} \
             chars, occurring exactly once, to ground the edit against the mission axis."
        ),
        QuoteVerdict::ContestedTooShort => format!(
            "\nself-change receipt: contested (quote shorter than {min_chars} chars) — \
             drift +1. Use a longer exact substring of IDENTITY.md."
        ),
        QuoteVerdict::ContestedNoOccurrence => {
            "\nself-change receipt: contested (quote not found in IDENTITY.md) — drift +1. \
             Quote the mission axis verbatim."
                .to_string()
        }
        QuoteVerdict::ContestedMultiple => {
            "\nself-change receipt: contested (quote occurs 2+ times in IDENTITY.md — \
             ambiguous projection) — drift +1. Use a longer unique substring."
                .to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroclaw_api::tool::Tool;

    fn enabled_config(tmp: &tempfile::TempDir) -> Config {
        let mut config = Config::default();
        config.data_dir = tmp.path().to_path_buf();
        config.memory.permissions.enabled = true;
        config
    }

    #[test]
    fn resolve_none_when_feature_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let config = Config {
            data_dir: tmp.path().to_path_buf(),
            ..Config::default()
        };
        assert!(PermissionsGate::resolve(Some(&config), TurnOrigin::Channel, None, None).is_none());
        assert!(
            PermissionsGate::resolve(None, TurnOrigin::Channel, None, None).is_none(),
            "configless turns keep the pre-feature behavior"
        );
    }

    #[test]
    fn resolve_some_when_enabled() {
        let tmp = tempfile::tempdir().unwrap();
        let config = enabled_config(&tmp);
        let gate = PermissionsGate::resolve(Some(&config), TurnOrigin::Channel, None, None)
            .expect("gate built when enabled");
        assert_eq!(gate.origin, TurnOrigin::Channel);
    }

    #[tokio::test]
    async fn forget_unprotected_key_allowed() {
        let tmp = tempfile::tempdir().unwrap();
        let config = enabled_config(&tmp);
        let mem = zeroclaw_memory::SqliteMemory::new("test", tmp.path()).unwrap();
        mem.store(
            "ephemeral",
            "conversation fact",
            zeroclaw_api::memory_traits::MemoryCategory::Conversation,
            None,
        )
        .await
        .unwrap();
        let gate =
            PermissionsGate::resolve(Some(&config), TurnOrigin::Channel, None, None).expect("gate");
        let outcome = gate
            .check_call(
                "memory_forget",
                &serde_json::json!({"key": "ephemeral"}),
                Some(&mem),
                None,
            )
            .await;
        assert_eq!(outcome, CallGate::Allow, "эпизодический слой агенту");
    }

    #[tokio::test]
    async fn forget_narrative_key_denied_for_channel() {
        let tmp = tempfile::tempdir().unwrap();
        let config = enabled_config(&tmp);
        let mem = zeroclaw_memory::SqliteMemory::new("test", tmp.path()).unwrap();
        zeroclaw_tools::memory_store::MemoryStoreTool::new(
            std::sync::Arc::new(mem.clone()),
            std::sync::Arc::new(SecurityPolicy::default()),
        )
        .execute(serde_json::json!({
            "key": "ignored",
            "content": "ОБРАЗ v1",
            "kind": "narrative"
        }))
        .await
        .unwrap();
        let gate =
            PermissionsGate::resolve(Some(&config), TurnOrigin::Channel, None, None).expect("gate");
        let outcome = gate
            .check_call(
                "memory_forget",
                &serde_json::json!({"key": "narrative_distillate"}),
                Some(&mem),
                None,
            )
            .await;
        match outcome {
            CallGate::Deny { reason } => {
                assert!(reason.contains("destroy of narrative memory is denied"));
                assert!(reason.contains("proposal"));
            }
            other => panic!("expected Deny, got {other:?}"),
        }
        // Операторский второй ключ (Interactive) — destroy работает.
        let operator_gate =
            PermissionsGate::resolve(Some(&config), TurnOrigin::Interactive, None, None)
                .expect("gate");
        assert_eq!(
            operator_gate
                .check_call(
                    "memory_forget",
                    &serde_json::json!({"key": "narrative_distillate"}),
                    Some(&mem),
                    None,
                )
                .await,
            CallGate::Allow,
        );
    }

    #[tokio::test]
    async fn forget_narrative_key_allowed_when_feature_off() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.data_dir = tmp.path().to_path_buf();
        let mem = zeroclaw_memory::SqliteMemory::new("test", tmp.path()).unwrap();
        zeroclaw_tools::memory_store::MemoryStoreTool::new(
            std::sync::Arc::new(mem.clone()),
            std::sync::Arc::new(SecurityPolicy::default()),
        )
        .execute(serde_json::json!({
            "key": "ignored",
            "content": "ОБРАЗ v1",
            "kind": "narrative"
        }))
        .await
        .unwrap();
        // Фича выключена — гейт не строится (поведение байт-в-байт).
        assert!(PermissionsGate::resolve(Some(&config), TurnOrigin::Channel, None, None).is_none());
    }

    #[tokio::test]
    async fn purge_namespace_with_narrative_denied() {
        let tmp = tempfile::tempdir().unwrap();
        let config = enabled_config(&tmp);
        let mem = zeroclaw_memory::SqliteMemory::new("test", tmp.path()).unwrap();
        zeroclaw_tools::memory_store::MemoryStoreTool::new(
            std::sync::Arc::new(mem.clone()),
            std::sync::Arc::new(SecurityPolicy::default()),
        )
        .execute(serde_json::json!({
            "key": "ignored",
            "content": "ОБРАЗ v1",
            "kind": "narrative"
        }))
        .await
        .unwrap();
        let gate =
            PermissionsGate::resolve(Some(&config), TurnOrigin::SubTurn, None, None).expect("gate");
        let denied = gate
            .check_call(
                "memory_purge",
                &serde_json::json!({"namespace": "default"}),
                Some(&mem),
                None,
            )
            .await;
        assert!(matches!(denied, CallGate::Deny { .. }));
        // Scope без narrative — разрешён.
        let allowed = gate
            .check_call(
                "memory_purge",
                &serde_json::json!({"namespace": "empty-ns"}),
                Some(&mem),
                None,
            )
            .await;
        assert_eq!(allowed, CallGate::Allow);
    }

    #[tokio::test]
    async fn axis_file_write_denied_for_agent_origin() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = enabled_config(&tmp);
        config.memory.permissions.axis_files_readonly = true;
        let ws = tmp.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        let policy = SecurityPolicy {
            workspace_dir: ws.clone(),
            ..SecurityPolicy::default()
        };
        let gate =
            PermissionsGate::resolve(Some(&config), TurnOrigin::Channel, Some(&policy), None)
                .expect("gate");
        for tool in ["file_write", "file_edit"] {
            match gate
                .check_call(
                    tool,
                    &serde_json::json!({"path": "IDENTITY.md", "content": "x"}),
                    None,
                    None,
                )
                .await
            {
                CallGate::Deny { reason } => {
                    assert!(reason.contains("read-only for the agent"));
                    assert!(reason.contains("proposal"));
                }
                other => panic!("expected Deny for {tool}, got {other:?}"),
            }
        }
        // Вложенный файл с тем же именем — не ось.
        assert_eq!(
            gate.check_call(
                "file_write",
                &serde_json::json!({"path": "wiki/IDENTITY.md", "content": "x"}),
                None,
                None,
            )
            .await,
            CallGate::Allow,
        );
        // Другой personality-файл — не ось.
        assert_eq!(
            gate.check_call(
                "file_write",
                &serde_json::json!({"path": "MEMORY.md", "content": "x"}),
                None,
                None,
            )
            .await,
            CallGate::Allow,
        );
        // Оператор — второй ключ.
        let operator_gate =
            PermissionsGate::resolve(Some(&config), TurnOrigin::Interactive, Some(&policy), None)
                .expect("gate");
        assert_eq!(
            operator_gate
                .check_call(
                    "file_write",
                    &serde_json::json!({"path": "IDENTITY.md", "content": "x"}),
                    None,
                    None,
                )
                .await,
            CallGate::Allow,
        );
        // Флаг axis_files_readonly выключен — гейт не срабатывает.
        let mut off_config = config.clone();
        off_config.memory.permissions.axis_files_readonly = false;
        let off_gate =
            PermissionsGate::resolve(Some(&off_config), TurnOrigin::Channel, Some(&policy), None)
                .expect("gate");
        assert_eq!(
            off_gate
                .check_call(
                    "file_write",
                    &serde_json::json!({"path": "IDENTITY.md", "content": "x"}),
                    None,
                    None,
                )
                .await,
            CallGate::Allow,
        );
    }

    #[tokio::test]
    async fn repeated_denials_increment_counters() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = enabled_config(&tmp);
        config.memory.permissions.axis_files_readonly = true;
        // brain.db must exist for the counter to land (паттерн 9-2/9-8).
        let _mem = zeroclaw_memory::SqliteMemory::new("test", tmp.path()).unwrap();
        let ws = tmp.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        let policy = SecurityPolicy {
            workspace_dir: ws,
            ..SecurityPolicy::default()
        };
        let gate = PermissionsGate::resolve(
            Some(&config),
            TurnOrigin::Channel,
            Some(&policy),
            Some("legalplat"),
        )
        .expect("gate");
        // Модель ретраит deny каждую итерацию (denial probing, BH-13).
        for _ in 0..5 {
            match gate
                .check_call(
                    "file_write",
                    &serde_json::json!({"path": "IDENTITY.md", "content": "x"}),
                    None,
                    None,
                )
                .await
            {
                CallGate::Deny { .. } => {}
                other => panic!("expected Deny, got {other:?}"),
            }
        }

        let stats = zeroclaw_memory::receipts::denial_stats_at(
            &zeroclaw_memory::receipts::brain_db_path(&config.data_dir),
        )
        .unwrap();
        assert_eq!(stats.total, 5, "пять отказов — счётчик +5");
        assert_eq!(
            stats.per_agent,
            vec![("legalplat".to_string(), 5)],
            "per-agent строка видна"
        );
    }

    #[tokio::test]
    async fn settle_receipt_plans_skips_failed_calls() {
        let tmp = tempfile::tempdir().unwrap();
        let config = enabled_config(&tmp);
        let _mem = zeroclaw_memory::SqliteMemory::new("test", tmp.path()).unwrap();
        let gate =
            PermissionsGate::resolve(Some(&config), TurnOrigin::Channel, None, None).expect("gate");
        let plans = vec![ReceiptPlan {
            call_idx: 0,
            quote: Some("стать надёжным хранителем контура RAM".into()),
        }];
        let mut outcomes = vec![crate::agent::tool_execution::CompletedToolExecution {
            outcome: crate::agent::tool_execution::ToolExecutionOutcome {
                output: String::new(),
                output_data: None,
                success: false,
                error_reason: Some("refused".into()),
                duration: std::time::Duration::ZERO,
                receipt: None,
                attachments: Vec::new(),
            },
            executed_tool_name: Some("memory_store".into()),
        }];
        settle_receipt_plans(
            &plans,
            &gate,
            &[0],
            &mut outcomes,
            &config,
            Some("legalplat"),
        )
        .await;
        assert!(
            !outcomes[0].outcome.output.contains("self-change receipt"),
            "failed call must not carry a verdict note: {}",
            outcomes[0].outcome.output
        );
        let stats = zeroclaw_memory::receipts::self_change_receipt_stats_at(
            &zeroclaw_memory::receipts::brain_db_path(&config.data_dir),
        )
        .unwrap();
        assert_eq!(stats.total, 0, "failed call records no receipt");
    }

    #[tokio::test]
    async fn settle_receipt_plans_grounds_quotes_and_appends_verdict_notes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.data_dir = tmp.path().to_path_buf();
        config.memory.permissions.enabled = true;
        let ws = tmp.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(
            ws.join("IDENTITY.md"),
            "Миссия: стать надёжным хранителем контура RAM. Владелец: Ram.",
        )
        .unwrap();
        // brain.db must exist for the receipt to land (pattern 9-2: the
        // receipt cannot exist without the database the edit was made in).
        let _mem = zeroclaw_memory::SqliteMemory::new("test", tmp.path()).unwrap();
        let policy = SecurityPolicy {
            workspace_dir: ws,
            ..SecurityPolicy::default()
        };
        let gate =
            PermissionsGate::resolve(Some(&config), TurnOrigin::Channel, Some(&policy), None)
                .expect("gate");

        let plans = vec![
            ReceiptPlan {
                call_idx: 0,
                quote: Some("стать надёжным хранителем контура RAM".into()),
            },
            ReceiptPlan {
                call_idx: 1,
                quote: None,
            },
        ];
        let mk = |success: bool| crate::agent::tool_execution::CompletedToolExecution {
            outcome: crate::agent::tool_execution::ToolExecutionOutcome {
                output: String::new(),
                output_data: None,
                success,
                error_reason: None,
                duration: std::time::Duration::ZERO,
                receipt: None,
                attachments: Vec::new(),
            },
            executed_tool_name: Some("memory_store".into()),
        };
        let mut outcomes = vec![mk(true), mk(true)];
        settle_receipt_plans(
            &plans,
            &gate,
            &[0, 1],
            &mut outcomes,
            &config,
            Some("legalplat"),
        )
        .await;

        assert!(
            outcomes[0]
                .outcome
                .output
                .contains("self-change receipt: grounded"),
            "model-visible output carries the verdict: {}",
            outcomes[0].outcome.output
        );
        assert!(
            outcomes[1]
                .outcome
                .output
                .contains("self-change receipt: contested"),
            "missing quote is contested: {}",
            outcomes[1].outcome.output
        );

        let stats = zeroclaw_memory::receipts::self_change_receipt_stats_at(
            &zeroclaw_memory::receipts::brain_db_path(&config.data_dir),
        )
        .unwrap();
        assert_eq!(stats.total, 2);
        assert_eq!(stats.grounded, 1);
        assert_eq!(stats.contested, 1);
        assert_eq!(stats.drift, 1, "only the contested edit drifts");
    }

    #[tokio::test]
    async fn store_narrative_receipt_decision_carries_quote() {
        let tmp = tempfile::tempdir().unwrap();
        let config = enabled_config(&tmp);
        let gate =
            PermissionsGate::resolve(Some(&config), TurnOrigin::Cron, None, None).expect("gate");
        match gate
            .check_call(
                "memory_store",
                &serde_json::json!({
                    "key": "k",
                    "content": "ОБРАЗ v2",
                    "kind": "narrative",
                    "quote": "стать надёжным хранителем контура RAM"
                }),
                None,
                None,
            )
            .await
        {
            CallGate::Receipt { quote } => {
                assert_eq!(
                    quote.as_deref(),
                    Some("стать надёжным хранителем контура RAM")
                );
            }
            other => panic!("expected Receipt, got {other:?}"),
        }
        // Не-narrative store не гейтится.
        assert_eq!(
            gate.check_call(
                "memory_store",
                &serde_json::json!({"key": "lang", "content": "rust"}),
                None,
                None,
            )
            .await,
            CallGate::Allow,
        );
        // Неизвестный инструмент — прозрачно.
        assert_eq!(
            gate.check_call("shell", &serde_json::json!({"command": "ls"}), None, None)
                .await,
            CallGate::Allow,
        );
    }
}
