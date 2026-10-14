//! Exclusive, mount-local metadata preparation. No application persistence or
//! resource authority lives here. Dropping the preparation changes no history.
use super::TextareaState;
use crate::input::{
    InputHistoryError, InputHistoryKind, InputHistorySnapshot, InputHistoryStateId,
    TextareaHistoryError,
};
use gpui::Context;
use std::rc::Rc;

pub struct PreparedMetadata {
    owner: Rc<()>,
    captured: InputHistorySnapshot,
    undo: Vec<InputHistoryStateId>,
}
impl PreparedMetadata {
    pub fn captured(&self) -> &InputHistorySnapshot {
        &self.captured
    }
    /// Exact destinations after one native metadata transaction, nearest first.
    /// Native retention is applied here; applications do not guess its bound.
    pub fn undo_destinations(&self) -> &[InputHistoryStateId] {
        &self.undo
    }
}
impl<M: crate::input::InputModeKind> crate::input::InputBaseState<M> {
    pub(in crate::input) fn metadata_frozen(&self) -> bool {
        self.metadata_fence.strong_count() != 0
    }
}
impl TextareaState {
    pub fn prepare_metadata_transaction(
        &mut self,
        captured: &InputHistorySnapshot,
    ) -> Result<PreparedMetadata, TextareaHistoryError> {
        self.check_history_program_capture(captured)?;
        let owner = Rc::new(());
        let prepared = PreparedMetadata {
            owner: owner.clone(),
            captured: captured.clone(),
            undo: self.undo_manager.metadata_undo_destinations(),
        };
        self.metadata_fence = Rc::downgrade(&owner);
        Ok(prepared)
    }
    /// Consume only after the caller has received the exact persistence ACK.
    /// The retained input must stay alive until ACK/rejection. Editability can
    /// change meanwhile; that cannot revoke a transaction already committed.
    pub fn commit_prepared_metadata(
        &mut self,
        prepared: PreparedMetadata,
        cx: &mut Context<Self>,
    ) -> Result<InputHistorySnapshot, InputHistoryError> {
        self.check_prepared_metadata(&prepared)?;
        self.undo_manager
            .record_metadata_transaction(
                self.selections.iter().copied().collect(),
                self.mode.auto_closed_pairs().clone(),
            )
            .ok_or(InputHistoryError::TransactionActive)?;
        self.metadata_fence = std::rc::Weak::new();
        self.emit_history(InputHistoryKind::Metadata, cx);
        cx.notify();
        Ok(self.history_snapshot())
    }
    pub(super) fn check_prepared_metadata(
        &self,
        prepared: &PreparedMetadata,
    ) -> Result<(), InputHistoryError> {
        if !self.metadata_fence.ptr_eq(&Rc::downgrade(&prepared.owner)) {
            return Err(InputHistoryError::StaleCapture);
        }
        let actual = self.history_snapshot();
        let captured = &prepared.captured;
        if actual.state_id != captured.state_id
            || actual.topology != captured.topology
            || actual.content != captured.content
            || actual.text_revision != captured.text_revision
            || actual.selection_revision != captured.selection_revision
            || actual.selections != captured.selections
            || actual.caret_phase != captured.caret_phase
            || actual.stored_marks != captured.stored_marks
            || actual.composing != captured.composing
        {
            return Err(InputHistoryError::StaleCapture);
        }
        Ok(())
    }
}
