//! SQL completion for the editor: adapts `switchyard_db::complete` to gpui-component's
//! `CompletionProvider`, and offers SQL snippets (DBX-4b) whose prefix matches the word.
//!
//! gpui-component 0.7 has no snippet insertion (it ignores `insert_text_format` and
//! inserts `new_text` as is), so a snippet item inserts its body with placeholders
//! replaced by their default text, and the SQL tab then selects the first placeholder
//! (`place_snippet_cursor`, from the editor's change event).

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use anyhow::Result;
use gpui_kit::component::RopeExt as _;
use gpui_kit::component::input::{CompletionProvider, EditorState, Rope};
use gpui_kit::{App, Entity, Task, Window};
use lsp_types::{
    CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse, CompletionTextEdit,
    TextEdit,
};
use switchyard_core::db::complete::{CandidateKind, CatalogIndex, complete};
use switchyard_core::db::{Engine, dialect_for};
use switchyard_core::store::snippets::{expand, matching, snippets_for};

/// Completion state shared between a SQL tab and its editor.
#[derive(Default)]
pub struct CompletionState {
    /// Columns and tables of the bound connection.
    pub index: CatalogIndex,
    /// Engine of the bound connection.
    pub engine: Option<Engine>,
    /// Snippet items offered by the last completion request, so the one inserted can
    /// get its first placeholder selected.
    pub snippets: Vec<PendingSnippet>,
}

/// A snippet completion as it will be inserted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingSnippet {
    /// Byte offset where the body is inserted.
    pub start: usize,
    /// The expanded body.
    pub text: String,
    /// Range to select, relative to `start`.
    pub select: Range<usize>,
}

impl PendingSnippet {
    /// The absolute range to select if `text` now holds this snippet just inserted with
    /// the cursor at its end.
    fn selection_in(&self, text: &Rope, cursor: usize) -> Option<Range<usize>> {
        let end = self.start + self.text.len();
        if cursor != end || end > text.len() {
            return None;
        }
        if !text.is_char_boundary(self.start) || !text.is_char_boundary(end) {
            return None;
        }
        (text.slice(self.start..end) == self.text.as_str())
            .then(|| self.start + self.select.start..self.start + self.select.end)
    }
}

/// After an edit: if a snippet item was just inserted, select its first placeholder.
pub fn place_snippet_cursor(
    state: &RefCell<CompletionState>,
    editor: &Entity<EditorState>,
    cx: &mut App,
) {
    if state.borrow().snippets.is_empty() {
        return;
    }
    let range = {
        let e = editor.read(cx);
        let (text, cursor) = (e.text(), e.cursor());
        state
            .borrow()
            .snippets
            .iter()
            .find_map(|p| p.selection_in(text, cursor))
    };
    if let Some(range) = range {
        state.borrow_mut().snippets.clear();
        editor.update(cx, |e, cx| e.set_selected_range(range, cx));
    }
}

/// Snippet completion items for `typed` (replacing `range`, which starts at byte `start`).
fn snippet_items(
    engine: Engine,
    typed: &str,
    start: usize,
    range: lsp_types::Range,
    cx: &App,
) -> (Vec<CompletionItem>, Vec<PendingSnippet>) {
    let all = snippets_for(engine, crate::snippets::user_snippets(cx));
    let mut items = Vec::new();
    let mut pending = Vec::new();
    for s in matching(&all, typed) {
        let e = expand(&s.body);
        let select = e.first.clone().unwrap_or(e.text.len()..e.text.len());
        pending.push(PendingSnippet {
            start,
            text: e.text.clone(),
            select,
        });
        items.push(CompletionItem {
            label: s.prefix.clone(),
            kind: Some(CompletionItemKind::SNIPPET),
            detail: Some(format!("{} (snippet)", s.name)),
            documentation: Some(lsp_types::Documentation::String(e.text.clone())),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit::new(range, e.text))),
            ..Default::default()
        });
    }
    (items, pending)
}

/// The provider installed on each SQL editor.
pub struct SqlCompletion {
    /// Shared state, updated when the catalog loads.
    pub state: Rc<RefCell<CompletionState>>,
}

impl CompletionProvider for SqlCompletion {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        trigger: CompletionContext,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<CompletionResponse>> {
        let typed = trigger.trigger_character.unwrap_or_default();
        tracing::debug!(offset, typed = %typed, "completion requested");
        let engine = self.state.borrow().engine.unwrap_or(Engine::Postgres);
        let dialect = dialect_for(engine);
        let script = text.to_string();
        // Replace exactly what was typed since completion started.
        let start = offset.saturating_sub(typed.len());
        let range = lsp_types::Range::new(
            text.offset_to_position(start),
            text.offset_to_position(offset),
        );
        let (mut items, pending) = snippet_items(engine, &typed, start, range, cx);
        self.state.borrow_mut().snippets = pending;
        let state = self.state.borrow();
        let words = complete(dialect, &state.index, &script, offset, &typed)
            .into_iter()
            .map(|c| CompletionItem {
                label: c.label,
                kind: Some(match c.kind {
                    CandidateKind::Column => CompletionItemKind::FIELD,
                    CandidateKind::Table => CompletionItemKind::CLASS,
                    CandidateKind::Schema => CompletionItemKind::MODULE,
                    CandidateKind::Function => CompletionItemKind::FUNCTION,
                    CandidateKind::Keyword => CompletionItemKind::KEYWORD,
                }),
                detail: Some(c.detail),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit::new(range, c.insert))),
                ..Default::default()
            });
        items.extend(words);
        Task::ready(Ok(CompletionResponse::Array(items)))
    }

    fn is_completion_trigger(&self, _offset: usize, new_text: &str, _cx: &mut App) -> bool {
        let mut chars = new_text.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => c == '.' || c.is_alphanumeric() || c == '_',
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippet_selection_after_insert() {
        let p = PendingSnippet {
            start: 5,
            text: "SELECT *\nFROM t;".into(),
            select: 14..15,
        };
        // Inserted with the cursor at its end: select the placeholder.
        let text = Rope::from("-- x\nSELECT *\nFROM t;\n");
        assert_eq!(p.selection_in(&text, 5 + p.text.len()), Some(19..20));
        assert_eq!(&text.to_string()[19..20], "t");
        // Cursor elsewhere, or different text: not this snippet.
        assert_eq!(p.selection_in(&text, 3), None);
        let other = Rope::from("-- x\nSELECT 1\nFROM t;\n");
        assert_eq!(p.selection_in(&other, 5 + p.text.len()), None);
        // Text shorter than the snippet.
        assert_eq!(p.selection_in(&Rope::from("-- x"), 5 + p.text.len()), None);
    }
}
