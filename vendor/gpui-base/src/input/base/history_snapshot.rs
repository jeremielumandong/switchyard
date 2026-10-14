//! Provenance from the existing native undo owner, never inferred from text.
use super::{InputBaseState, InputContent, InputModeKind};
use gpui::{Context, Pixels};
use std::{ops::Range, sync::Mutex};

/// Process-local, opaque state in one actual input's native history. It cannot
/// be constructed, persisted or used as an authorization credential.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct InputHistoryStateId {
    owner: u64,
    state: u64,
}
impl InputHistoryStateId {
    pub(super) fn new_owner() -> Self {
        static NEXT_OWNER: Mutex<u64> = Mutex::new(1);
        let mut next = NEXT_OWNER
            .lock()
            .expect("native input identity lock poisoned");
        let owner = *next;
        *next = next
            .checked_add(1)
            .expect("native input owner identity exhausted");
        Self { owner, state: 0 }
    }
    pub(super) fn fresh(&mut self) -> Self {
        self.state = self
            .state
            .checked_add(1)
            .expect("native history identity exhausted");
        *self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputHistoryKind {
    Edit,
    Undo,
    Redo,
    Reset,
    Composition,
    Metadata,
    Selection,
}

/// Visual caret stop at a Source styled boundary. Actual typed stored marks
/// remain with the parent rich-document owner, associated synchronously with
/// this native selection epoch. This is not a persisted undo destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputCaretPhase {
    Before,
    After,
}

/// The four Source composer marks. This typed native association is transient
/// selection state; it is neither a styled document nor a second undo owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputCaretMark {
    Bold,
    Italic,
    Strike,
    Code,
}
impl InputCaretMark {
    fn bit(self) -> u8 {
        match self {
            Self::Bold => 1,
            Self::Italic => 2,
            Self::Strike => 4,
            Self::Code => 8,
        }
    }
}
/// A canonical Source mark set. None in the snapshot means default marks;
/// Some(empty) means an explicitly chosen unstyled typing stop.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputCaretMarks(u8);
impl InputCaretMarks {
    pub fn contains(self, mark: InputCaretMark) -> bool {
        self.0 & mark.bit() != 0
    }
    pub fn iter(self) -> impl Iterator<Item = InputCaretMark> {
        [
            InputCaretMark::Bold,
            InputCaretMark::Italic,
            InputCaretMark::Strike,
            InputCaretMark::Code,
        ]
        .into_iter()
        .filter(move |mark| self.contains(*mark))
    }
}
impl FromIterator<InputCaretMark> for InputCaretMarks {
    fn from_iter<T: IntoIterator<Item = InputCaretMark>>(marks: T) -> Self {
        Self(marks.into_iter().fold(0, |bits, mark| bits | mark.bit()))
    }
}

/// Actual native stack destinations, nearest replay first. Pending composition
/// is separate: it has not yet become a committed undo transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputHistoryTopology {
    pub undo: Vec<InputHistoryStateId>,
    pub current: InputHistoryStateId,
    pub redo: Vec<InputHistoryStateId>,
    pub pending: Option<(InputHistoryStateId, InputHistoryStateId)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputHistoryError {
    StaleCapture,
    CompositionActive,
    NotEditable,
    UnsupportedMode,
    TransactionActive,
}
impl std::fmt::Display for InputHistoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Native metadata transaction refused: {self:?}")
    }
}
impl std::error::Error for InputHistoryError {}

/// Actual native selection at a captured change boundary. Ranges are UTF8 bytes.
#[derive(Clone, Debug, PartialEq)]
pub struct InputHistorySelection {
    pub range: Range<usize>,
    pub reversed: bool,
    pub column_anchor: Option<(Pixels, usize)>,
}

#[derive(Clone)]
/// Owned, coherent native capture; GPUI delivers its notification later.
pub struct InputHistorySnapshot {
    pub state_id: InputHistoryStateId,
    /// Captured with this content, never reconstructed from later events or
    /// matching text. It is a projection of the sole native history owner.
    pub topology: InputHistoryTopology,
    pub content: InputContent,
    /// The first selection is active; direction and preferred column remain
    /// the native values. Consumers do not restore the native owner from this.
    pub selections: Vec<InputHistorySelection>,
    pub text_revision: u64,
    pub selection_revision: u64,
    pub caret_phase: Option<InputCaretPhase>,
    pub stored_marks: Option<InputCaretMarks>,
    pub composing: bool,
}

#[derive(Clone)]
pub struct InputHistoryEvent {
    pub kind: InputHistoryKind,
    pub snapshot: InputHistorySnapshot,
}

impl<M: InputModeKind> InputBaseState<M> {
    /// Capture actual native history provenance together with current text,
    /// token identity, selection and IME state. Keep application metadata keyed
    /// by this opaque state ID; equal text never proves an undo transition.
    pub fn history_snapshot(&self) -> InputHistorySnapshot {
        InputHistorySnapshot {
            state_id: self.undo_manager.state_id(),
            topology: self.undo_manager.topology(),
            content: self.snapshot_content(),
            selections: self
                .selections
                .iter()
                .map(|s| InputHistorySelection {
                    range: s.start..s.end,
                    reversed: s.reversed,
                    column_anchor: s.column_anchor,
                })
                .collect(),
            text_revision: self.document_revision,
            selection_revision: self.selections.revision(),
            caret_phase: self.selections.caret_phase(),
            stored_marks: self.selections.stored_marks(),
            composing: self.ime_marked_range.is_some(),
        }
    }
    /// Actual states still reachable through this owner's bounded undo/redo
    /// transactions, pending composition or current value. Applications may
    /// prune typed metadata after reconciling the current capture; this is not
    /// a list of every transient keystroke or a second history store.
    pub fn retained_history_state_ids(&self) -> Vec<InputHistoryStateId> {
        self.undo_manager.retained_state_ids()
    }
    /// Add one native undo boundary for an actual application metadata edit.
    /// The parent supplies its captured native state/text/selection epochs and
    /// atomically associates the resulting ID with its typed metadata in the
    /// same entity update. This does not edit text or retain application data;
    /// actor/context/attachment authorization and no-op detection stay with
    /// the parent. Native Undo/Redo emits the existing coherent history event.
    pub fn commit_metadata_transaction(
        &mut self,
        state_id: InputHistoryStateId,
        text_revision: u64,
        selection_revision: u64,
        cx: &mut Context<Self>,
    ) -> Result<InputHistoryStateId, InputHistoryError> {
        let state =
            self.record_metadata_transaction(state_id, text_revision, selection_revision)?;
        self.emit_history(InputHistoryKind::Metadata, cx);
        cx.notify();
        Ok(state)
    }
    /// Record through the original undo owner before the caller publishes its
    /// complete native association. No intermediate history event is emitted.
    pub(super) fn record_metadata_transaction(
        &mut self,
        state_id: InputHistoryStateId,
        text_revision: u64,
        selection_revision: u64,
    ) -> Result<InputHistoryStateId, InputHistoryError> {
        if self.ime_marked_range.is_some() {
            return Err(InputHistoryError::CompositionActive);
        }
        if !self.is_editable() {
            return Err(InputHistoryError::NotEditable);
        }
        if M::CODE_EDITOR || self.masked || !self.mask_pattern.is_none() {
            return Err(InputHistoryError::UnsupportedMode);
        }
        if state_id != self.undo_manager.state_id()
            || text_revision != self.document_revision
            || selection_revision != self.selections.revision()
        {
            return Err(InputHistoryError::StaleCapture);
        }
        let state = self
            .undo_manager
            .record_metadata_transaction(
                self.selections.iter().cloned().collect(),
                self.mode.auto_closed_pairs().clone(),
            )
            .ok_or(InputHistoryError::TransactionActive)?;
        Ok(state)
    }
    pub(super) fn emit_history(&self, kind: InputHistoryKind, cx: &mut Context<Self>) {
        if self.emit_events {
            cx.emit(InputHistoryEvent {
                kind,
                snapshot: self.history_snapshot(),
            });
        }
    }
}
