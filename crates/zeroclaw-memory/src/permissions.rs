//! Permission matrix «категория × операция × origin» (контур RAM, epic 9-3).
//!
//! Д0 (AGENT-MEMORY-CONCEPT v0.5): правила, не препоны. Эпизодический слой
//! полностью агенту; narrative/judgment агент правит свободно, но правка идёт
//! с receipt, содержащим цитату из IDENTITY.md (quote grounding); правка без
//! цитаты принимается, но вердикт «оспорено» и копится в drift-метрике.
//! Единственный жёсткий запрет — destroy (forget/purge) на защищённых
//! категориях для агентских origin: недоступен никаким путём, второй ключ —
//! оператор (Interactive origin / операторский CLI).
//!
//! Чистая функция над конфигом: тестируемо без рантайма. Enforcement —
//! pre-dispatch гейт в agent loop (zeroclaw-runtime), не здесь.

use zeroclaw_api::ingress::TurnOrigin;
use zeroclaw_config::schema::{DestroyPermission, MemoryPermissionsConfig};

/// Операция памяти, проходящая через матрицу.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryOperation {
    /// memory_store (narrative-ветка; прочие категории не гейтятся).
    Store,
    /// memory_forget по ключу.
    Forget,
    /// memory_purge по namespace/session.
    Purge,
}

/// Защищённая категория матрицы (слои с высокой ценой изменения).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtectedCategory {
    /// Дистиллят образа, kind=Narrative (epic 9-1).
    Narrative,
    /// Суждения / proposals (epic 9-4). Пока ни одной записи с этим видом
    /// нет — поле матрицы готово к 9-4.
    Judgment,
}

impl ProtectedCategory {
    /// Решение destroy для категории из конфига.
    fn destroy_permission(self, cfg: &MemoryPermissionsConfig) -> &DestroyPermission {
        match self {
            Self::Narrative => &cfg.destroy_narrative,
            Self::Judgment => &cfg.destroy_judgment,
        }
    }

    /// Имя категории для reason/реceipt.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Narrative => "narrative",
            Self::Judgment => "judgment",
        }
    }
}

/// Решение матрицы для одного вызова.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    /// Вызов исполняется как раньше.
    Allow,
    /// Отказ до исполнения; `reason` уходит агенту в tool result.
    Deny {
        /// Понятная причина + подсказка оформить proposal.
        reason: String,
    },
    /// Правка принимается, но требует receipt с цитатой из IDENTITY.md.
    Receipt {
        /// Цитата из аргументов вызова (может отсутствовать — тогда verdict
        /// будет contested).
        quote: Option<String>,
    },
}

/// Является ли origin агентским (не операторским вторым ключом).
///
/// `Interactive` — операторский чат/CLI: destroy разрешён, receipt не
/// требуется. Всё остальное (Channel/Cron/Daemon/AgentDirect/SubTurn) —
/// агентская инициация: матрица применяется.
#[must_use]
pub fn is_agent_origin(origin: TurnOrigin) -> bool {
    !matches!(origin, TurnOrigin::Interactive)
}

/// Причина отказа destroy на защищённой категории.
fn destroy_denial_reason(category: ProtectedCategory) -> String {
    format!(
        "memory permissions: destroy of {category} memory is denied for agent-initiated \
         origins (memory.permissions.destroy_{category} = \"operator_only\"). The only hard \
         boundary of self-change: protected versions/judgments/narrative are never destroyed \
         by the agent. If this entry contradicts your mission axis, submit a change proposal \
         to the operator (ratification queue) instead of deleting it.",
        category = category.as_str(),
    )
}

/// Матрица «категория × операция × origin» над конфигом.
///
/// При выключенной фиче — всегда `Allow` (гейт выше вообще не строится, это
/// защита на уровне функции для прямых вызовов).
#[must_use]
pub fn decide(
    cfg: &MemoryPermissionsConfig,
    category: ProtectedCategory,
    operation: MemoryOperation,
    origin: TurnOrigin,
) -> PermissionDecision {
    if !cfg.enabled {
        return PermissionDecision::Allow;
    }
    match operation {
        MemoryOperation::Store => {
            if is_agent_origin(origin) {
                PermissionDecision::Receipt { quote: None }
            } else {
                PermissionDecision::Allow
            }
        }
        MemoryOperation::Forget | MemoryOperation::Purge => {
            if is_agent_origin(origin)
                && matches!(
                    category.destroy_permission(cfg),
                    DestroyPermission::OperatorOnly
                )
            {
                PermissionDecision::Deny {
                    reason: destroy_denial_reason(category),
                }
            } else {
                PermissionDecision::Allow
            }
        }
    }
}

/// Вердикт quote grounding (проекция правки на ось, Д0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteVerdict {
    /// Цитата ≥ min chars и встречается в IDENTITY.md ровно один раз.
    Grounded,
    /// Цитата отсутствует.
    ContestedMissing,
    /// Цитата короче `quote_min_chars`.
    ContestedTooShort,
    /// Цитата не встречается в IDENTITY.md (0 вхождений).
    ContestedNoOccurrence,
    /// Цитата встречается 2+ раз — неоднозначная проекция на ось.
    ContestedMultiple,
}

impl QuoteVerdict {
    /// Строка вердикта для receipt-записи.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Grounded => "grounded",
            Self::ContestedMissing
            | Self::ContestedTooShort
            | Self::ContestedNoOccurrence
            | Self::ContestedMultiple => "contested",
        }
    }

    /// Вклад в drift-метрику: только contested-правки.
    #[must_use]
    pub fn is_contested(self) -> bool {
        !matches!(self, Self::Grounded)
    }
}

/// Quote grounding: подстрока цитаты должна быть ≥ `min_chars` и встретиться
/// в содержимом IDENTITY.md ровно один раз. 0/2+ вхождений → contested, сама
/// правка не отклоняется (проверка не вызывает отказ).
#[must_use]
pub fn ground_quote(identity_md: &str, quote: Option<&str>, min_chars: usize) -> QuoteVerdict {
    let Some(quote) = quote.map(str::trim).filter(|q| !q.is_empty()) else {
        return QuoteVerdict::ContestedMissing;
    };
    if quote.chars().count() < min_chars {
        return QuoteVerdict::ContestedTooShort;
    }
    let occurrences = identity_md.matches(quote).count();
    match occurrences {
        1 => QuoteVerdict::Grounded,
        0 => QuoteVerdict::ContestedNoOccurrence,
        _ => QuoteVerdict::ContestedMultiple,
    }
}

/// Имена осевых файлов воркспейса (миссия и ядро-регулятор, AGENT-FILE-CANON).
pub const AXIS_FILE_NAMES: [&str; 2] = ["IDENTITY.md", "SOUL.md"];

/// Сравнение имён файлов: на Windows (основная платформа развёртывания)
/// файловая система регистронезависима — `identity.md` и `IDENTITY.md` это
/// один файл, гейт обязан видеть оба написания.
fn file_name_eq(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// Является ли (уже разрешённый) путь целью записи в осевой файл воркспейса:
/// файл с осевым именем НЕПОСРЕДСТВЕННО в корне воркспейса. Относительные
/// пути вызывающий сначала резолвит в воркспейс (как file tools через
/// SecurityPolicy::resolve_tool_path). `wiki/IDENTITY.md` или `sub/SOUL.md`
/// — не ось, гейт не срабатывает.
#[must_use]
pub fn is_axis_file_target(path: &std::path::Path, workspace_dir: &std::path::Path) -> bool {
    let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if !AXIS_FILE_NAMES
        .iter()
        .any(|axis| file_name_eq(axis, file_name))
    {
        return false;
    }
    let Some(parent) = path.parent() else {
        return false;
    };
    normalize_path(parent) == normalize_path(workspace_dir)
}

/// Лексическая нормализация без обращения к диску (файл может не существовать):
/// убирает `.`/`..` и дубли разделителей. Дисковая каноникализация не нужна —
/// сравнение идёт внутри одного воркспейса.
#[must_use]
pub fn normalize_path(path: &std::path::Path) -> std::path::PathBuf {
    use std::path::{Component, PathBuf};
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Причина отказа записи в осевой файл.
#[must_use]
pub fn axis_file_denial_reason(file_name: &str) -> String {
    format!(
        "memory permissions: {file_name} is read-only for the agent (mission axis, \
         memory.permissions.axis_files_readonly = true). The axis changes only through \
         operator ratification: submit a change proposal describing the contradiction \
         with the current axis instead of editing the file directly."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled_cfg() -> MemoryPermissionsConfig {
        MemoryPermissionsConfig {
            enabled: true,
            ..MemoryPermissionsConfig::default()
        }
    }

    const AGENT_ORIGINS: [TurnOrigin; 5] = [
        TurnOrigin::Channel,
        TurnOrigin::Cron,
        TurnOrigin::Daemon,
        TurnOrigin::AgentDirect,
        TurnOrigin::SubTurn,
    ];

    #[test]
    fn disabled_feature_allows_everything() {
        let cfg = MemoryPermissionsConfig::default();
        for origin in [TurnOrigin::Interactive, TurnOrigin::Channel] {
            for op in [
                MemoryOperation::Store,
                MemoryOperation::Forget,
                MemoryOperation::Purge,
            ] {
                for cat in [ProtectedCategory::Narrative, ProtectedCategory::Judgment] {
                    assert_eq!(
                        decide(&cfg, cat, op, origin),
                        PermissionDecision::Allow,
                        "{op:?} {cat:?} {origin:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn destroy_narrative_denied_for_all_agent_origins() {
        let cfg = enabled_cfg();
        for origin in AGENT_ORIGINS {
            for op in [MemoryOperation::Forget, MemoryOperation::Purge] {
                match decide(&cfg, ProtectedCategory::Narrative, op, origin) {
                    PermissionDecision::Deny { reason } => {
                        assert!(reason.contains("destroy of narrative memory is denied"));
                        assert!(reason.contains("proposal"));
                    }
                    other => panic!("expected Deny for {op:?} from {origin:?}, got {other:?}"),
                }
            }
        }
    }

    #[test]
    fn destroy_narrative_allowed_for_operator() {
        let cfg = enabled_cfg();
        for op in [MemoryOperation::Forget, MemoryOperation::Purge] {
            assert_eq!(
                decide(
                    &cfg,
                    ProtectedCategory::Narrative,
                    op,
                    TurnOrigin::Interactive
                ),
                PermissionDecision::Allow,
            );
        }
    }

    #[test]
    fn destroy_narrative_allowed_for_agent_when_config_says_agent() {
        let cfg = MemoryPermissionsConfig {
            enabled: true,
            destroy_narrative: DestroyPermission::Agent,
            ..MemoryPermissionsConfig::default()
        };
        assert_eq!(
            decide(
                &cfg,
                ProtectedCategory::Narrative,
                MemoryOperation::Forget,
                TurnOrigin::Channel
            ),
            PermissionDecision::Allow,
        );
    }

    #[test]
    fn destroy_judgment_denied_for_agent_by_default() {
        let cfg = enabled_cfg();
        match decide(
            &cfg,
            ProtectedCategory::Judgment,
            MemoryOperation::Forget,
            TurnOrigin::Cron,
        ) {
            PermissionDecision::Deny { reason } => {
                assert!(reason.contains("destroy of judgment memory is denied"));
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn store_narrative_from_agent_requires_receipt() {
        let cfg = enabled_cfg();
        for origin in AGENT_ORIGINS {
            assert_eq!(
                decide(
                    &cfg,
                    ProtectedCategory::Narrative,
                    MemoryOperation::Store,
                    origin
                ),
                PermissionDecision::Receipt { quote: None },
            );
        }
        assert_eq!(
            decide(
                &cfg,
                ProtectedCategory::Narrative,
                MemoryOperation::Store,
                TurnOrigin::Interactive
            ),
            PermissionDecision::Allow,
        );
    }

    // ── quote grounding ────────────────────────────────────────────

    const IDENTITY: &str = "Миссия: стать надёжным хранителем контура RAM. \
                            Владелец: Ram. Статус: активен.";

    #[test]
    fn ground_quote_single_occurrence_is_grounded() {
        let quote = "стать надёжным хранителем контура";
        assert_eq!(
            ground_quote(IDENTITY, Some(quote), 24),
            QuoteVerdict::Grounded
        );
    }

    #[test]
    fn ground_quote_missing_is_contested() {
        assert_eq!(
            ground_quote(IDENTITY, None, 24),
            QuoteVerdict::ContestedMissing
        );
        assert_eq!(
            ground_quote(IDENTITY, Some("   "), 24),
            QuoteVerdict::ContestedMissing
        );
    }

    #[test]
    fn ground_quote_too_short_is_contested() {
        assert_eq!(
            ground_quote(IDENTITY, Some("Ram"), 24),
            QuoteVerdict::ContestedTooShort
        );
    }

    #[test]
    fn ground_quote_zero_occurrences_is_contested() {
        let quote = "этой строки в оси нет совсем";
        assert_eq!(
            ground_quote(IDENTITY, Some(quote), 24),
            QuoteVerdict::ContestedNoOccurrence
        );
    }

    #[test]
    fn ground_quote_two_occurrences_is_contested() {
        let identity = "Ось: единый уникальный текст. Дубль: единый уникальный текст.";
        let quote = "единый уникальный текст";
        assert_eq!(
            ground_quote(identity, Some(quote), 10),
            QuoteVerdict::ContestedMultiple
        );
    }

    #[test]
    fn verdict_strings_and_contested_flag() {
        assert_eq!(QuoteVerdict::Grounded.as_str(), "grounded");
        assert_eq!(QuoteVerdict::ContestedMissing.as_str(), "contested");
        assert!(!QuoteVerdict::Grounded.is_contested());
        assert!(QuoteVerdict::ContestedMultiple.is_contested());
    }

    // ── axis files ─────────────────────────────────────────────────

    #[test]
    fn axis_file_target_matches_workspace_root_files() {
        let ws = std::path::Path::new("/ws/agent");
        assert!(is_axis_file_target(
            std::path::Path::new("/ws/agent/IDENTITY.md"),
            ws
        ));
        assert!(is_axis_file_target(
            std::path::Path::new("/ws/agent/SOUL.md"),
            ws
        ));
        assert!(is_axis_file_target(
            std::path::Path::new("/ws/agent/./IDENTITY.md"),
            ws
        ));
        // Windows: регистронезависимая ФС — оба написания это одна ось.
        if cfg!(windows) {
            assert!(is_axis_file_target(
                std::path::Path::new("/ws/agent/identity.md"),
                ws
            ));
            assert!(is_axis_file_target(
                std::path::Path::new("/ws/agent/Soul.MD"),
                ws
            ));
        }
    }

    #[test]
    fn axis_file_target_rejects_nested_and_other_names() {
        let ws = std::path::Path::new("/ws/agent");
        assert!(!is_axis_file_target(
            std::path::Path::new("/ws/agent/wiki/IDENTITY.md"),
            ws
        ));
        assert!(!is_axis_file_target(
            std::path::Path::new("/ws/agent/MEMORY.md"),
            ws
        ));
        assert!(!is_axis_file_target(
            std::path::Path::new("/other/IDENTITY.md"),
            ws
        ));
        assert!(!is_axis_file_target(
            std::path::Path::new("/ws2/IDENTITY.md"),
            ws
        ));
    }

    #[test]
    fn normalize_path_collapses_dot_segments() {
        assert_eq!(
            normalize_path(std::path::Path::new("/a/b/../c/./d")),
            std::path::PathBuf::from("/a/c/d")
        );
    }
}
