//! Narrative distillate (контур RAM, epic 9-1 / ADR Решение 1).
//!
//! Дистиллят образа — единственная pinned Core-запись с `MemoryKind::Narrative`
//! (ключ `NARRATIVE_KEY`) в базе памяти. Это единственный источник истины;
//! раздел между маркерами в SOUL.md воркспейса — генерируемая проекция,
//! переписываемая при каждом фазовом прогоне governance (DEPLOY). Ручные
//! правки раздела затираются следующим DEPLOY — направление одно: база → файл.
//!
//! Жёсткий cap на длину контента — механизм отбора, а не ограничение:
//! переполнение блокирует публикацию (контекст не засоряется) и журналируется
//! как задача консолидации (слой триггеров, epic 9-6).

use crate::traits::Memory;
use std::path::{Path, PathBuf};

/// Ключ записи дистиллята в базе памяти.
pub const NARRATIVE_KEY: &str = "narrative_distillate";

/// Маркеры генерируемого раздела в SOUL.md.
pub const SECTION_BEGIN: &str = "<!-- NARRATIVE:BEGIN -->";
pub const SECTION_END: &str = "<!-- NARRATIVE:END -->";

/// Cap по умолчанию, символов. Параллель: бюджет MEMORY.md в holographic (~2200).
pub const DEFAULT_NARRATIVE_CAP_CHARS: usize = 2200;

/// Причина отказа публикации при переполнении cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NarrativeReject {
    /// Контент длиннее cap: публикация отклонена, нужна консолидация.
    OverCap { len: usize, cap: usize },
    /// Записи дистиллята нет в базе.
    Missing,
}

impl std::fmt::Display for NarrativeReject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OverCap { len, cap } => {
                write!(f, "narrative distillate over cap: {len} chars > {cap}")
            }
            Self::Missing => write!(f, "narrative distillate record not found"),
        }
    }
}

/// Проверка cap. Ок — публиковать можно.
pub fn validate_cap(content: &str, cap_chars: usize) -> Result<(), NarrativeReject> {
    let len = content.chars().count();
    if len > cap_chars {
        return Err(NarrativeReject::OverCap { len, cap: cap_chars });
    }
    Ok(())
}

/// Переписать раздел дистиллята в SOUL.md воркспейса между маркерами.
/// Маркеров нет — раздел дописывается в конец файла. Идемпотентна.
/// Возвращает true, если файл изменился.
pub fn sync_soul_section(soul_path: &Path, content: &str) -> std::io::Result<bool> {
    let raw = std::fs::read_to_string(soul_path).unwrap_or_default();
    let section = format!("{SECTION_BEGIN}\n{content}\n{SECTION_END}");
    let new_raw = match (
        raw.find(SECTION_BEGIN),
        raw.find(SECTION_END),
    ) {
        (Some(begin), Some(end)) if begin <= end => {
            format!("{}{}{}", &raw[..begin], section, &raw[end + SECTION_END.len()..])
        }
        _ => {
            let sep = if raw.is_empty() || raw.ends_with('\n') { "" } else { "\n" };
            format!("{raw}{sep}\n## Дистиллят образа (генерируется, не править)\n\n{section}\n")
        }
    };
    if new_raw == raw {
        return Ok(false);
    }
    std::fs::write(soul_path, new_raw)?;
    Ok(true)
}

/// DEPLOY: прочитать дистиллят из памяти, проверить cap, переписать раздел
/// в SOUL.md воркспейса. Ошибка — публикация отклонена (файл не тронут).
pub async fn deploy(
    memory: &dyn Memory,
    workspace_dir: &Path,
    cap_chars: usize,
) -> Result<bool, NarrativeReject> {
    let entry = memory
        .get(NARRATIVE_KEY)
        .await
        .map_err(|_| NarrativeReject::Missing)?
        .ok_or(NarrativeReject::Missing)?;
    validate_cap(&entry.content, cap_chars)?;
    let soul = workspace_dir.join("SOUL.md");
    sync_soul_section(&soul, entry.content.trim()).map_err(|_| NarrativeReject::Missing)
}

/// Удобный путь к SOUL.md (для тестов и вызовов).
pub fn soul_path(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join("SOUL.md")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_ok_at_and_below_limit() {
        assert!(validate_cap("abc", 3).is_ok());
        assert!(validate_cap("abc", 10).is_ok());
    }

    #[test]
    fn cap_rejects_over_limit() {
        let err = validate_cap("abcd", 3).unwrap_err();
        assert_eq!(err, NarrativeReject::OverCap { len: 4, cap: 3 });
        assert!(err.to_string().contains("4 chars > 3"));
    }

    #[test]
    fn sync_appends_section_when_markers_missing() {
        let dir = tempfile::tempdir().unwrap();
        let soul = dir.path().join("SOUL.md");
        std::fs::write(&soul, "# SOUL\n").unwrap();
        assert!(sync_soul_section(&soul, "ОБРАЗ v1").unwrap());
        let raw = std::fs::read_to_string(&soul).unwrap();
        assert!(raw.contains(SECTION_BEGIN));
        assert!(raw.contains("ОБРАЗ v1"));
        assert!(raw.contains(SECTION_END));
    }

    #[test]
    fn sync_replaces_between_markers_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let soul = dir.path().join("SOUL.md");
        std::fs::write(
            &soul,
            format!("head\n{SECTION_BEGIN}\nOLD\n{SECTION_END}\ntail\n"),
        )
        .unwrap();
        assert!(sync_soul_section(&soul, "NEW").unwrap());
        let raw = std::fs::read_to_string(&soul).unwrap();
        assert!(raw.contains("head\n"));
        assert!(raw.contains("NEW"));
        assert!(!raw.contains("OLD"));
        assert!(raw.contains("tail"));
        // повторный DEPLOY с тем же контентом не меняет файл
        assert!(!sync_soul_section(&soul, "NEW").unwrap());
    }

    #[test]
    fn sync_preserves_surrounding_content() {
        let dir = tempfile::tempdir().unwrap();
        let soul = dir.path().join("SOUL.md");
        std::fs::write(&soul, "a\nb\nc\n").unwrap();
        sync_soul_section(&soul, "X").unwrap();
        let raw = std::fs::read_to_string(&soul).unwrap();
        assert!(raw.starts_with("a\nb\nc\n"));
    }
}
