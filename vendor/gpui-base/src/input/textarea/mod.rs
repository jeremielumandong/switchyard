use gpui::{App, Context, Entity, IntoElement, RenderOnce, Window};

use super::{InputBaseState, TextareaMode};
mod before_edit;
mod history_program;
mod metadata_fence;
pub use before_edit::{
    TextareaBeforeEditHandler, TextareaClipboardRequest, TextareaEditDecision, TextareaEditKind,
    TextareaEditPlan, TextareaEditRefusal, TextareaEditRefusalReason, TextareaEditRequest,
    TextareaPasteRequest, TextareaReplacement,
};
pub use metadata_fence::PreparedMetadata;
mod rich_projection;
pub(crate) mod rich_render;
pub use history_program::{
    PreparedHistory, TextareaHistoryBoundary, TextareaHistoryError, TextareaHistoryStage,
};
pub use rich_projection::{RichLine, RichPiece, RichProjection, RichProjectionError};

/// State for editing ordinary multi-line text.
///
/// This is the shared editing engine in its multi-line kind. Code-editor
/// facilities such as languages, diagnostics, folding, and LSP do not exist on
/// this type — those methods live on [`super::EditorState`].
pub type TextareaState = InputBaseState<TextareaMode>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputCaretPhaseError {
    StaleCapture,
    CompositionActive,
    NotEditable,
    UnsupportedMode,
    NonCollapsedSelection,
    TransactionActive,
}
impl std::fmt::Display for InputCaretPhaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Native caret phase refused: {self:?}")
    }
}
impl std::error::Error for InputCaretPhaseError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputCaretTransactionError {
    Caret(InputCaretPhaseError),
    History(super::InputHistoryError),
}
impl std::fmt::Display for InputCaretTransactionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Native caret transaction refused: {self:?}")
    }
}
impl std::error::Error for InputCaretTransactionError {}

impl TextareaState {
    /// Associate a rich boundary's visual typing stop with the actual native
    /// selection owner. The parent first computes Source stored marks, then
    /// commits their typed association using the returned coherent capture in
    /// this same entity update. This method never creates an undo transaction
    /// or edits text. Ordinary caret/text changes clear this visual phase.
    /// None is the ordinary or explicitly user-toggled stored-mark state.
    pub fn set_caret_phase(
        &mut self,
        captured: &super::InputHistorySnapshot,
        phase: Option<super::InputCaretPhase>,
        cx: &mut Context<Self>,
    ) -> Result<super::InputHistorySnapshot, InputCaretPhaseError> {
        self.set_caret_state(captured, phase, self.selections.stored_marks(), cx)
    }
    /// Record a genuine producer metadata undo step and its complete typing
    /// association in the sole native owner. Emit only the final Metadata
    /// capture, with the new ID and marks together. Source command planning
    /// decides whether a real undo step exists; stored-only commands use
    /// set_caret_state instead. No text or application document is replaced.
    pub fn commit_caret_metadata_transaction(
        &mut self,
        captured: &super::InputHistorySnapshot,
        phase: Option<super::InputCaretPhase>,
        marks: Option<super::InputCaretMarks>,
        cx: &mut Context<Self>,
    ) -> Result<super::InputHistorySnapshot, InputCaretTransactionError> {
        self.check_caret_capture(captured)
            .map_err(InputCaretTransactionError::Caret)?;
        self.record_metadata_transaction(
            captured.state_id,
            captured.text_revision,
            captured.selection_revision,
        )
        .map_err(InputCaretTransactionError::History)?;
        // All fallible checks precede history mutation. The actual canonical
        // typed mark value can be installed without a second fallible call.
        self.selections.set_caret_state(phase, marks);
        self.emit_history(super::InputHistoryKind::Metadata, cx);
        cx.notify();
        Ok(self.history_snapshot())
    }
    /// Commit the actual producer's typed Source stored-mark set and visual
    /// stop together. A manual mark change advances native selection identity
    /// even when both visual phases are None. None marks selects Source default
    /// marks, distinct from Some(empty). Genuine no-ops retain the epoch. The
    /// rich producer still owns document/schema derivation and editing.
    pub fn set_caret_state(
        &mut self,
        captured: &super::InputHistorySnapshot,
        phase: Option<super::InputCaretPhase>,
        marks: Option<super::InputCaretMarks>,
        cx: &mut Context<Self>,
    ) -> Result<super::InputHistorySnapshot, InputCaretPhaseError> {
        self.check_caret_capture(captured)?;
        if self.selections.set_caret_state(phase, marks) {
            self.emit_history(super::InputHistoryKind::Selection, cx);
            cx.notify();
        }
        Ok(self.history_snapshot())
    }
    fn check_caret_capture(
        &self,
        captured: &super::InputHistorySnapshot,
    ) -> Result<(), InputCaretPhaseError> {
        if self.ime_marked_range.is_some() {
            return Err(InputCaretPhaseError::CompositionActive);
        }
        if !self.is_editable() {
            return Err(InputCaretPhaseError::NotEditable);
        }
        if self.masked || !self.mask_pattern.is_none() {
            return Err(InputCaretPhaseError::UnsupportedMode);
        }
        if captured.state_id != self.undo_manager.state_id()
            || captured.text_revision != self.document_revision
            || captured.selection_revision != self.selections.revision()
        {
            return Err(InputCaretPhaseError::StaleCapture);
        }
        if !self.selections.is_single() || !self.selections.active().is_collapsed() {
            return Err(InputCaretPhaseError::NonCollapsedSelection);
        }
        if self.undo_manager.has_open_transaction() {
            return Err(InputCaretPhaseError::TransactionActive);
        }
        Ok(())
    }
    /// Actual native selection changes, including a move away and return to
    /// identical byte offsets before deferred Change notifications arrive.
    /// Pair with text_revision and the parent's actual view/focus/IME owner.
    pub fn selection_revision(&self) -> u64 {
        self.selections.revision()
    }
    /// Native document revision for a presentation-only parse result. Every
    /// edit, reset, undo/redo and IME update advances this revision.
    pub fn text_revision(&self) -> u64 {
        self.document_revision
    }
    /// Apply ordinary text styles without changing text, tokens, selection,
    /// IME or undo history. Ranges are UTF8 bytes of the complete current
    /// document. A stale parse result returns false; invalid ranges refuse
    /// atomically. Future edits clear styles until a fresh snapshot arrives.
    pub fn set_text_decorations(
        &mut self,
        revision: u64,
        decorations: Vec<super::TextDecoration>,
        cx: &mut Context<Self>,
    ) -> Result<bool, super::InlineTokenError> {
        if revision != self.document_revision {
            return Ok(false);
        }
        let text = self.text.to_string();
        for decoration in &decorations {
            let range = &decoration.range;
            if range.start > range.end || range.end > text.len() {
                return Err(super::InlineTokenError::InvalidRange);
            }
            if !text.is_char_boundary(range.start) || !text.is_char_boundary(range.end) {
                return Err(super::InlineTokenError::InvalidBoundary);
            }
        }
        if self.extras.decorations != decorations {
            self.extras.decorations = decorations;
            cx.notify();
        }
        Ok(true)
    }
    pub fn text_decorations(&self) -> &[super::TextDecoration] {
        &self.extras.decorations
    }
}

/// An unstyled ordinary multi-line text input.
#[derive(IntoElement)]
pub struct Textarea {
    presentation: super::InlineTokenPresentation,
    state: Entity<TextareaState>,
}

impl Textarea {
    pub fn new(state: &Entity<TextareaState>) -> Self {
        Self {
            state: state.clone(),
            presentation: Default::default(),
        }
    }
    /// The element each atomic token renders as; the input keeps editing and history.
    pub fn token<R: IntoElement>(
        mut self,
        render: impl Fn(&super::InlineTokenContext, &mut Window, &mut App) -> R + 'static,
    ) -> Self {
        self.presentation = self.presentation.token(render);
        self
    }
    /// Open a reference after a completed, unconsumed token click.
    pub fn on_token_click(
        mut self,
        listener: impl Fn(&super::InlineTokenClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.presentation = self.presentation.on_token_click(listener);
        self
    }
    /// Report pointer presence over a token; hover never selects or edits.
    pub fn on_token_hover(
        mut self,
        listener: impl Fn(&super::InlineTokenHoverEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.presentation = self.presentation.on_token_hover(listener);
        self
    }
}

impl RenderOnce for Textarea {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        self.state.update(cx, |state, _| {
            state.set_token_presentation(self.presentation)
        });
        self.state
    }
}
