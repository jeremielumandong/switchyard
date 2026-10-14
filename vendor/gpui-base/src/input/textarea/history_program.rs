//! Checked producer transactions in the existing native history owner.
use super::TextareaState;
use crate::input::{
    InputCaretMarks, InputCaretPhase, InputContent, InputHistoryError, InputHistoryEvent,
    InputHistoryKind, InputHistorySelection, InputHistorySnapshot,
};
use gpui::{Context, Window};

/// A boundary supplied by the actual typed document producer. The native
/// kernel does not infer document changes or command success from flat text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextareaHistoryBoundary {
    /// One genuine document/metadata undo step, including equal flat content.
    Undo,
    /// A selection/stored-mark dispatch with no document history step.
    Selection,
}

#[derive(Clone)]
pub struct TextareaHistoryStage {
    /// Complete native projection, including original token occurrences.
    pub content: InputContent,
    /// Actual Source result, in UTF8 bytes, with its direction retained.
    pub selection: InputHistorySelection,
    pub caret_phase: Option<InputCaretPhase>,
    pub stored_marks: Option<InputCaretMarks>,
    pub boundary: TextareaHistoryBoundary,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextareaHistoryError {
    Capture(InputHistoryError),
    MultipleSelections,
    InvalidSelection,
    NormalizationRequired,
    ValidationRejected,
    SelectionChangesContent,
    TooManyStages,
    InvalidPreparedBoundary,
}
impl std::fmt::Display for TextareaHistoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Native history program refused: {self:?}")
    }
}
impl std::error::Error for TextareaHistoryError {}

/// A validated single-Undo program held by this input's existing exclusive
/// fence. Drop leaves content, selection and native history untouched. This is
/// editor preparation, never authority to commit external resources.
pub struct PreparedHistory {
    fence: super::PreparedMetadata,
    stages: Vec<TextareaHistoryStage>,
}
impl PreparedHistory {
    pub fn undo_destinations(&self) -> &[crate::input::InputHistoryStateId] {
        self.fence.undo_destinations()
    }
}

impl TextareaState {
    /// Prepare the Source controlled rewrite (one Undo followed by optional
    /// selection dispatches) before an external producer's atomic save.
    pub fn prepare_history_transaction(
        &mut self,
        captured: &InputHistorySnapshot,
        stages: Vec<TextareaHistoryStage>,
        cx: &mut Context<Self>,
    ) -> Result<PreparedHistory, TextareaHistoryError> {
        self.check_history_program_capture(captured)?;
        if stages
            .first()
            .is_none_or(|s| s.boundary != TextareaHistoryBoundary::Undo)
            || stages
                .iter()
                .skip(1)
                .any(|s| s.boundary != TextareaHistoryBoundary::Selection)
        {
            return Err(TextareaHistoryError::InvalidPreparedBoundary);
        }
        self.validate_history_stages(captured, &stages, cx)?;
        // Includes revalidation after an application input predicate.
        let fence = self.prepare_metadata_transaction(captured)?;
        Ok(PreparedHistory { fence, stages })
    }
    /// Commit only the already prepared editor program after the caller's
    /// actual save ACK. Read-only retirement cannot revoke an accepted save.
    /// A real Window must remain available until this completion is applied.
    pub fn commit_prepared_history(
        &mut self,
        prepared: PreparedHistory,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Vec<InputHistorySnapshot>, TextareaHistoryError> {
        self.check_prepared_metadata(&prepared.fence)
            .map_err(TextareaHistoryError::Capture)?;
        // Release only in this synchronous update, after checking the exact
        // retained input/capture. No application predicate runs after ACK.
        self.metadata_fence = std::rc::Weak::new();
        Ok(self.apply_validated_history_stages(
            prepared.fence.captured(),
            &prepared.stages,
            None,
            None,
            window,
            cx,
        ))
    }

    /// Observe the collapsed caret's first/last visual row using the same
    /// wrapping map and affinity as native vertical movement. No selection or
    /// history mutation occurs. Missing layout is not evidence of an edge.
    pub fn history_visual_edges(
        &self,
        captured: &InputHistorySnapshot,
    ) -> Result<Option<(bool, bool)>, TextareaHistoryError> {
        self.check_history_program_capture(captured)?;
        if !captured.selections[0].range.is_empty()
            || self.last_layout.is_none()
            || self.display_map.text() != &self.text
        {
            return Ok(None);
        }
        let rows = self.display_map.display_row_count();
        if rows == 0 {
            return Ok(None);
        }
        let point = self.display_map.offset_to_wrap_display_point_with_affinity(
            captured.selections[0].range.start,
            self.cursor_line_end_affinity,
        );
        Ok(self
            .display_map
            .wrap_row_to_display_row(point.row)
            .map(|row| (row == 0, row == rows - 1)))
    }

    /// Validate every producer stage before any native mutation, then commit
    /// in order through this input's sole undo owner. Each Undo stage is an
    /// atomic boundary; this API is for already prepared producer commands,
    /// not a replacement for interactive typing coalescing or an IME hook.
    ///
    /// Associate each returned actual ID with the corresponding canonical
    /// typed document in the SAME entity update, before deferred GPUI events
    /// run. A later association failure is not a rollback. The producer must
    /// prepare its associations before calling. No parsing/diffing, second
    /// undo manager, or history reset occurs here.
    pub fn apply_history_program(
        &mut self,
        captured: &InputHistorySnapshot,
        stages: &[TextareaHistoryStage],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Vec<InputHistorySnapshot>, TextareaHistoryError> {
        self.apply_history_program_internal(captured, stages, window, cx)
    }
}

impl<M: crate::input::InputModeKind> crate::input::InputBaseState<M> {
    pub(super) fn apply_history_program_internal(
        &mut self,
        captured: &InputHistorySnapshot,
        stages: &[TextareaHistoryStage],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Vec<InputHistorySnapshot>, TextareaHistoryError> {
        self.apply_history_program_grouped(captured, stages, None, None, window, cx)
    }

    pub(super) fn apply_history_program_grouped(
        &mut self,
        captured: &InputHistorySnapshot,
        stages: &[TextareaHistoryStage],
        group: Option<(bool, std::ops::Range<usize>, std::ops::Range<usize>)>,
        composition: Option<Option<std::ops::Range<usize>>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Vec<InputHistorySnapshot>, TextareaHistoryError> {
        self.check_history_program_capture_composition(captured, composition.is_some())?;
        self.validate_history_stages(captured, stages, cx)?;
        // Validation may call an application predicate. Recheck the coherent
        // original capture before crossing the first mutation boundary.
        self.check_history_program_capture_composition(captured, composition.is_some())?;
        Ok(self.apply_validated_history_stages(captured, stages, group, composition, window, cx))
    }

    fn validate_history_stages(
        &self,
        captured: &InputHistorySnapshot,
        stages: &[TextareaHistoryStage],
        cx: &mut Context<Self>,
    ) -> Result<(), TextareaHistoryError> {
        // Match the native owner's bounded transaction retention. A producer
        // cannot submit an unbounded synchronous edit program.
        if stages.len() > 1000 {
            return Err(TextareaHistoryError::TooManyStages);
        }
        let mut previous = &captured.content;
        for stage in stages {
            let text = stage.content.text().as_ref();
            let range = &stage.selection.range;
            // Prepared Source text transactions can leave a scalar caret
            // inside a newly joined grapheme (combining mark, emoji modifier,
            // regional pair or ZWJ sequence). Preserve that exact selection;
            // interactive native movement still uses its grapheme rules.
            let boundary = |offset| text.is_char_boundary(offset);
            if range.start > range.end
                || range.end > text.len()
                || !boundary(range.start)
                || !boundary(range.end)
                || stage.content.tokens().iter().any(|span| {
                    let token = span.range();
                    [range.start, range.end]
                        .iter()
                        .any(|&offset| token.start < offset && offset < token.end)
                })
                || (!range.is_empty() && stage.caret_phase.is_some())
                || stage
                    .selection
                    .column_anchor
                    .is_some_and(|(x, _)| !f32::from(x).is_finite())
            {
                return Err(TextareaHistoryError::InvalidSelection);
            }
            if stage.boundary == TextareaHistoryBoundary::Selection && &stage.content != previous {
                return Err(TextareaHistoryError::SelectionChangesContent);
            }
            if self.normalize_input(text) != text {
                return Err(TextareaHistoryError::NormalizationRequired);
            }
            if !self.is_valid_input(text, cx) {
                return Err(TextareaHistoryError::ValidationRejected);
            }
            previous = &stage.content;
        }
        Ok(())
    }

    fn apply_validated_history_stages(
        &mut self,
        captured: &InputHistorySnapshot,
        stages: &[TextareaHistoryStage],
        group: Option<(bool, std::ops::Range<usize>, std::ops::Range<usize>)>,
        composition: Option<Option<std::ops::Range<usize>>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<InputHistorySnapshot> {
        let emit = self.emit_events;
        let time = cx.background_executor().now();
        let group = group.map(|(insert, before, after)| {
            let join = if composition.is_some() && captured.composing {
                self.undo_manager.composer_can_continue_composition()
            } else {
                self.undo_manager.composer_can_join(insert, &before, time)
            };
            (join, insert, after)
        });
        self.emit_events = false;
        let mut results = Vec::with_capacity(stages.len());
        let mut events = Vec::with_capacity(stages.len());
        for stage in stages {
            let original = self.history_snapshot();
            let before: Vec<_> = self.selections.iter().copied().collect();
            if composition.is_some() {
                self.ime_marked_range = None;
            }
            if stage.boundary == TextareaHistoryBoundary::Undo {
                if stage.content == original.content {
                    // Equal native bytes can represent a genuine rich
                    // document change. Preserve its producer history step.
                    self.undo_manager
                        .record_metadata_transaction(
                            before.clone(),
                            self.mode.auto_closed_pairs().clone(),
                        )
                        .expect("prevalidated program has no open native transaction");
                } else {
                    self.apply_prevalidated_content(
                        0..self.text.len(),
                        stage.content.clone(),
                        window,
                        cx,
                    );
                }
            }
            // Use the original native cursor identity; do not flatten a
            // reversed selection or infer a cursor from replacement length.
            let mut selection = before[0];
            selection.start = stage.selection.range.start;
            selection.end = stage.selection.range.end;
            selection.reversed = stage.selection.reversed;
            selection.column_anchor = stage.selection.column_anchor;
            self.selections.replace_all(vec![selection]);
            self.selections
                .set_caret_state(stage.caret_phase, stage.stored_marks);
            if stage.boundary == TextareaHistoryBoundary::Undo {
                self.undo_manager
                    .record_selections(before, self.selections.iter().copied().collect());
            }
            if let Some((join, insert, ref range)) = group
                && stage.boundary == TextareaHistoryBoundary::Undo
            {
                self.undo_manager
                    .finish_composer_group(join, insert, range.clone(), time);
            }
            if let Some(marked) = &composition {
                self.ime_marked_range = marked.clone().map(Into::into);
            }
            let snapshot = self.history_snapshot();
            let kind = if composition.is_some() {
                Some(InputHistoryKind::Composition)
            } else if stage.boundary == TextareaHistoryBoundary::Selection {
                (snapshot.selection_revision != original.selection_revision)
                    .then_some(InputHistoryKind::Selection)
            } else if stage.content == original.content {
                Some(InputHistoryKind::Metadata)
            } else {
                Some(InputHistoryKind::Edit)
            };
            if let Some(kind) = kind {
                events.push(InputHistoryEvent {
                    kind,
                    snapshot: snapshot.clone(),
                });
            }
            results.push(snapshot);
        }
        self.emit_events = emit;
        if emit {
            for event in events {
                let text_edit = event.kind == InputHistoryKind::Edit;
                cx.emit(event);
                if text_edit {
                    cx.emit(crate::input::InputEvent::Change);
                }
            }
        }
        if !stages.is_empty() {
            cx.notify();
        }
        results
    }

    pub(super) fn check_history_program_capture(
        &self,
        captured: &InputHistorySnapshot,
    ) -> Result<(), TextareaHistoryError> {
        self.check_history_program_capture_composition(captured, false)
    }

    pub(super) fn check_history_program_capture_composition(
        &self,
        captured: &InputHistorySnapshot,
        allow_composition: bool,
    ) -> Result<(), TextareaHistoryError> {
        let reject = |error| TextareaHistoryError::Capture(error);
        if self.ime_marked_range.is_some() && !allow_composition {
            return Err(reject(InputHistoryError::CompositionActive));
        }
        if !self.is_editable() {
            return Err(reject(InputHistoryError::NotEditable));
        }
        if !M::MULTI_LINE || M::CODE_EDITOR || self.masked || !self.mask_pattern.is_none() {
            return Err(reject(InputHistoryError::UnsupportedMode));
        }
        if self.undo_manager.has_open_transaction() || self.undo_manager.is_ignoring() {
            return Err(reject(InputHistoryError::TransactionActive));
        }
        if !self.selections.is_single() {
            return Err(TextareaHistoryError::MultipleSelections);
        }
        let actual = self.history_snapshot();
        if actual.state_id != captured.state_id
            || actual.topology != captured.topology
            || actual.text_revision != captured.text_revision
            || actual.selection_revision != captured.selection_revision
            || actual.content != captured.content
            || actual.selections != captured.selections
            || actual.caret_phase != captured.caret_phase
            || actual.stored_marks != captured.stored_marks
            || actual.composing != captured.composing
        {
            return Err(reject(InputHistoryError::StaleCapture));
        }
        Ok(())
    }
}
