//! SQLite journal mode for the API workspace database.
//!
//! WAL by default. `SWITCHYARD_SQLITE_JOURNAL_MODE` overrides it (e.g. `TRUNCATE` when the
//! data directory is on a network filesystem, where WAL's shared-memory index is unsafe).
//! AgentOps detected network mounts with `statfs`; that needs `unsafe`, which this
//! workspace denies, so the override is manual here.

use std::path::Path;

/// The journal mode for a database at `db_path`.
pub fn resolve_journal_mode(_db_path: &Path) -> String {
    normalize_journal_mode(
        std::env::var("SWITCHYARD_SQLITE_JOURNAL_MODE")
            .ok()
            .as_deref(),
    )
    .unwrap_or_else(|| "WAL".to_string())
}

/// The `mmap_size` for a database at `db_path`: none when the journal was overridden away
/// from WAL (a sign of a network filesystem), else `ceiling`.
pub fn mmap_size_for(db_path: &Path, ceiling: i64) -> i64 {
    if resolve_journal_mode(db_path) == "WAL" {
        ceiling
    } else {
        0
    }
}

/// Validates and upper-cases a journal mode; `None` for anything unrecognised.
fn normalize_journal_mode(raw: Option<&str>) -> Option<String> {
    let up = raw?.trim().to_uppercase();
    match up.as_str() {
        "WAL" | "TRUNCATE" | "DELETE" | "PERSIST" | "MEMORY" | "OFF" => Some(up),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_are_validated() {
        assert_eq!(normalize_journal_mode(Some(" wal ")), Some("WAL".into()));
        assert_eq!(normalize_journal_mode(Some("bogus")), None);
        assert_eq!(normalize_journal_mode(None), None);
    }
}
