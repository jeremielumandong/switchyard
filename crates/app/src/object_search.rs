//! Global object search for the schema explorer filter (DBX-1d).
//!
//! When the filter has [`MIN_CHARS`] or more characters, the sidebar waits [`DEBOUNCE`]
//! and sends an [`IntrospectScope::Search`] on the catalog session. Server hits are merged
//! with the local fuzzy matches from already-loaded folders and shown as a flat list.
//! Results are kept only for the pattern they answer; a reply for an older pattern is
//! dropped. Nothing here is written to the schema cache.

use std::collections::HashSet;
use std::time::Duration;

use switchyard_core::db::{IntrospectScope, ObjectInfo, ObjectKind};

/// Shortest filter that triggers a server search.
pub const MIN_CHARS: usize = 2;
/// Quiet time after the last keystroke before searching.
pub const DEBOUNCE: Duration = Duration::from_millis(250);
/// Most server hits per search.
pub const LIMIT: u32 = 200;

/// Server answer for the current pattern.
#[derive(Clone, Debug, Default)]
pub enum SearchState {
    /// No search for this pattern (too short, or not sent yet).
    #[default]
    Idle,
    /// Sent, waiting for the answer.
    Loading,
    /// Server hits.
    Done(Vec<ObjectInfo>),
    /// The search failed; the local filter is shown.
    Failed(String),
}

/// Debounced server search state of the schema explorer.
#[derive(Debug, Default)]
pub struct ObjectSearch {
    /// Pattern the state belongs to (trimmed filter text).
    pattern: String,
    /// Bumped on every filter change; a debounce timer only fires for its own ticket.
    ticket: u64,
    /// Answer for `pattern`.
    pub state: SearchState,
}

impl ObjectSearch {
    /// Whether `filter` is long enough for a server search (and the flat result list).
    pub fn applies(filter: &str) -> bool {
        filter.chars().count() >= MIN_CHARS
    }

    /// The filter changed. Returns a ticket to pass to [`Self::due`] after [`DEBOUNCE`]
    /// when a server search should follow, or `None` when the filter is too short.
    pub fn changed(&mut self, filter: &str) -> Option<u64> {
        self.ticket = self.ticket.wrapping_add(1);
        if filter == self.pattern && !matches!(self.state, SearchState::Idle) {
            return None;
        }
        self.pattern = filter.to_owned();
        self.state = SearchState::Idle;
        Self::applies(filter).then_some(self.ticket)
    }

    /// The debounce for `ticket` elapsed: the scope to send, unless the filter changed since.
    pub fn due(&mut self, ticket: u64) -> Option<IntrospectScope> {
        if ticket != self.ticket || !Self::applies(&self.pattern) {
            return None;
        }
        self.state = SearchState::Loading;
        Some(IntrospectScope::Search {
            pattern: self.pattern.clone(),
            limit: LIMIT,
            include_system: false,
        })
    }

    /// A search answer arrived. Answers for any other pattern are stale and dropped.
    pub fn on_result(&mut self, pattern: &str, result: Result<Vec<ObjectInfo>, String>) {
        if pattern != self.pattern {
            return;
        }
        self.state = match result {
            Ok(hits) => SearchState::Done(hits),
            Err(e) => SearchState::Failed(e),
        };
    }

    /// Server hits for the current pattern, if they arrived.
    pub fn hits(&self) -> &[ObjectInfo] {
        match &self.state {
            SearchState::Done(h) => h,
            _ => &[],
        }
    }
}

/// How well `name` matches `filter`: exact, prefix, substring, then fuzzy-only.
fn rank(filter: &str, name: &str) -> u8 {
    let (f, n) = (filter.to_lowercase(), name.to_lowercase());
    if n == f {
        0
    } else if n.starts_with(&f) {
        1
    } else if n.contains(&f) {
        2
    } else {
        3
    }
}

/// Merge local fuzzy matches with server hits: one entry per (schema, name, kind),
/// keeping the local one (it carries row counts and signatures), best matches first.
pub fn merge<'a>(
    filter: &str,
    local: impl IntoIterator<Item = &'a ObjectInfo>,
    server: &'a [ObjectInfo],
) -> Vec<&'a ObjectInfo> {
    let mut seen: HashSet<(&str, &str, ObjectKind)> = HashSet::new();
    let mut out: Vec<&ObjectInfo> = local
        .into_iter()
        .chain(server)
        .filter(|o| seen.insert((o.schema.as_str(), o.name.as_str(), o.kind)))
        .collect();
    out.sort_by(|a, b| {
        rank(filter, &a.name)
            .cmp(&rank(filter, &b.name))
            .then(a.name.len().cmp(&b.name.len()))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.schema.cmp(&b.schema))
            .then(a.kind.cmp(&b.kind))
    });
    out
}

/// Lower-case kind name shown next to a search hit.
pub fn kind_label(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Table => "table",
        ObjectKind::View => "view",
        ObjectKind::MaterializedView => "materialized view",
        ObjectKind::Function => "function",
        ObjectKind::Procedure => "procedure",
        ObjectKind::Sequence => "sequence",
        ObjectKind::Type => "type",
        ObjectKind::Synonym => "synonym",
        ObjectKind::Role => "user / role",
        ObjectKind::Job => "agent job",
        ObjectKind::Extension => "extension",
        ObjectKind::Package => "package",
        ObjectKind::Stage => "stage",
        ObjectKind::Task => "task",
        ObjectKind::Pipe => "pipe",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(schema: &str, name: &str, kind: ObjectKind, rows: Option<i64>) -> ObjectInfo {
        ObjectInfo {
            schema: schema.into(),
            name: name.into(),
            kind,
            estimated_rows: rows,
            detail: None,
        }
    }

    #[test]
    fn merge_dedups_and_prefers_local() {
        let local = vec![
            obj("public", "orders", ObjectKind::Table, Some(10)),
            obj("public", "o_r_d", ObjectKind::Table, None),
        ];
        let server = vec![
            obj("public", "orders", ObjectKind::Table, None),
            obj("sales", "orders", ObjectKind::Table, None),
            obj("public", "orders", ObjectKind::View, None),
            obj("public", "order_items", ObjectKind::Table, None),
            obj("public", "big_orders", ObjectKind::Table, None),
        ];
        let merged = merge("order", &local, &server);
        let names: Vec<String> = merged
            .iter()
            .map(|o| format!("{}.{} {:?}", o.schema, o.name, o.kind))
            .collect();
        assert_eq!(
            names,
            [
                "public.orders Table",
                "public.orders View",
                "sales.orders Table",
                "public.order_items Table",
                "public.big_orders Table",
                "public.o_r_d Table",
            ]
        );
        assert_eq!(merged[0].estimated_rows, Some(10), "local entry kept");
    }

    #[test]
    fn exact_match_comes_first() {
        let server = vec![
            obj("a", "users_archive", ObjectKind::Table, None),
            obj("a", "Users", ObjectKind::Table, None),
        ];
        let merged = merge("users", [], &server);
        assert_eq!(merged[0].name, "Users");
    }

    #[test]
    fn short_filters_do_not_search() {
        let mut s = ObjectSearch::default();
        assert_eq!(s.changed("o"), None);
        assert!(s.due(1).is_none());
        assert!(ObjectSearch::applies("or"));
    }

    #[test]
    fn stale_ticket_and_stale_results_are_dropped() {
        let mut s = ObjectSearch::default();
        let first = s.changed("ord").unwrap();
        let second = s.changed("orde").unwrap();
        assert!(s.due(first).is_none(), "superseded debounce does not fire");
        let Some(IntrospectScope::Search { pattern, limit, .. }) = s.due(second) else {
            panic!("expected a search");
        };
        assert_eq!((pattern.as_str(), limit), ("orde", LIMIT));
        assert!(matches!(s.state, SearchState::Loading));
        // An answer for the earlier pattern arrives late: ignored.
        s.on_result(
            "ord",
            Ok(vec![obj("p", "ordinal", ObjectKind::Table, None)]),
        );
        assert!(matches!(s.state, SearchState::Loading));
        s.on_result(
            "orde",
            Ok(vec![obj("p", "orders", ObjectKind::Table, None)]),
        );
        assert_eq!(s.hits().len(), 1);
        // The user moves on: old hits disappear at once.
        s.changed("ordx");
        assert!(s.hits().is_empty());
    }

    #[test]
    fn failure_keeps_local_fallback() {
        let mut s = ObjectSearch::default();
        let t = s.changed("cust").unwrap();
        s.due(t);
        s.on_result("cust", Err("permission denied".into()));
        assert!(matches!(s.state, SearchState::Failed(_)));
        assert!(s.hits().is_empty());
    }
}
