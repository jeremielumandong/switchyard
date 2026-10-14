//! Opt-in pre-mutation producer interception; never an after-edit text diff.
use super::{TextareaHistoryError, TextareaHistoryStage, TextareaState};
use crate::input::{InputBaseState, InputContent, InputHistorySnapshot, InputModeKind};
use gpui::{Context, Window};
use std::{ops::Range, rc::Rc};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextareaEditKind {
    Text,
    Backspace,
    DeleteForward,
    DeleteToBeginningOfLine,
    DeleteToEndOfLine,
    DeletePreviousWord,
    DeleteNextWord,
    Cut,
    Paste,
    Enter {
        shift: bool,
        submit: bool,
        secondary: bool,
    },
    Indent {
        outdent: bool,
        block: bool,
    },
    Composition,
}

#[derive(Clone)]
pub struct TextareaReplacement {
    /// Actual UTF8 native range after existing atomic-token expansion.
    pub range: Range<usize>,
    /// Original supplied text/token occurrences, before normalization.
    pub content: InputContent,
}

#[derive(Clone)]
pub struct TextareaEditRequest {
    /// Coherent ORIGINAL selection/marks/content/history; delete has not yet
    /// expanded selections or opened a transaction, IME has not opened one.
    pub captured: InputHistorySnapshot,
    pub kind: TextareaEditKind,
    pub replacements: Vec<TextareaReplacement>,
    /// Original platform operand, when one was supplied. UTF16 conversion
    /// remains the existing native handler's responsibility.
    pub range_utf16: Option<Range<usize>>,
    pub composition_selection_utf16: Option<Range<usize>>,
}

/// Prepare both stages and infallible associations before returning Apply.
/// The callback runs synchronously with actual native IDs in the same update.
/// It must not call back into the borrowed native entity or try to roll back.
pub struct TextareaEditPlan {
    pub stages: Vec<TextareaHistoryStage>,
    typing_group: bool,
    composition: bool,
    submit_enter: bool,
    associate: Box<dyn FnOnce(&TextareaEditRequest, &[InputHistorySnapshot])>,
}
impl TextareaEditPlan {
    pub fn new(
        stages: Vec<TextareaHistoryStage>,
        associate: impl FnOnce(&TextareaEditRequest, &[InputHistorySnapshot]) + 'static,
    ) -> Self {
        Self {
            stages,
            typing_group: false,
            composition: false,
            submit_enter: false,
            associate: Box::new(associate),
        }
    }

    /// Opt in only for a producer's single plain ReplaceStep. Native capture
    /// and exact replacement projection are checked again before grouping;
    /// structural commands and clipboard programs remain separate undo steps.
    pub fn with_typing_group(mut self) -> Self {
        self.typing_group = true;
        self
    }

    /// Prepared canonical preedit update or commit. This remains opt-in:
    /// ordinary atomic producers still refuse IME before changing anything.
    pub fn with_composition(mut self) -> Self {
        self.composition = true;
        self.typing_group = true;
        self
    }

    /// An actual composer command consumed Enter after its earlier structural
    /// handlers declined. The native owner emits its normal submission event
    /// only after validating this empty plan against the original capture.
    pub fn with_enter_submission(mut self) -> Self {
        self.submit_enter = true;
        self
    }

    fn group(&self, request: &TextareaEditRequest) -> Option<(bool, Range<usize>, Range<usize>)> {
        use super::TextareaHistoryBoundary;
        if !self.typing_group {
            return None;
        }
        let [stage] = self.stages.as_slice() else {
            return None;
        };
        let [replacement] = request.replacements.as_slice() else {
            return None;
        };
        if (stage.boundary != TextareaHistoryBoundary::Undo && !self.composition)
            || !replacement.content.tokens().is_empty()
        {
            return None;
        }
        let insert = match request.kind {
            TextareaEditKind::Composition if self.composition => true,
            TextareaEditKind::Text if self.composition && request.captured.composing => true,
            TextareaEditKind::Text
                if replacement.range.is_empty() && !replacement.content.text().is_empty() =>
            {
                true
            }
            TextareaEditKind::Backspace
            | TextareaEditKind::DeleteForward
            | TextareaEditKind::DeleteToBeginningOfLine
            | TextareaEditKind::DeleteToEndOfLine
            | TextareaEditKind::DeletePreviousWord
            | TextareaEditKind::DeleteNextWord
                if !replacement.range.is_empty() && replacement.content.text().is_empty() =>
            {
                false
            }
            _ => return None,
        };
        let mut expected = request.captured.content.text().to_string();
        if expected.get(replacement.range.clone()).is_none() {
            return None;
        }
        expected.replace_range(replacement.range.clone(), replacement.content.text());
        let mut expected = InputContent::new(expected);
        for span in request.captured.content.tokens() {
            let range = span.range();
            let range = if range.end <= replacement.range.start {
                range
            } else if range.start >= replacement.range.end {
                let start =
                    range.start - replacement.range.len() + replacement.content.text().len();
                let end = range.end - replacement.range.len() + replacement.content.text().len();
                start..end
            } else {
                // The original native operand already expanded whole atoms.
                if range.start < replacement.range.start || range.end > replacement.range.end {
                    return None;
                }
                continue;
            };
            expected = expected.with_token(range, span.token().clone()).ok()?;
        }
        if stage.content != expected {
            return None;
        }
        Some((
            insert,
            replacement.range.clone(),
            replacement.range.start..replacement.range.start + replacement.content.text().len(),
        ))
    }
}
pub enum TextareaEditDecision {
    Refuse,
    Apply(TextareaEditPlan),
}
pub type TextareaBeforeEditHandler = Rc<dyn Fn(&TextareaEditRequest) -> TextareaEditDecision>;

/// Original native selection captured before clipboard I/O or deletion. An
/// opted-in owner must publish through its platform service and revalidate this
/// capture before applying a cut. Emitting this event performs no edit.
#[derive(Clone)]
pub struct TextareaClipboardRequest {
    pub captured: InputHistorySnapshot,
    pub cut: bool,
}

/// Explicit paste captured before ANY platform clipboard read. The opted-in
/// owner completes through the same native typed history after its read returns.
#[derive(Clone)]
pub struct TextareaPasteRequest {
    pub captured: InputHistorySnapshot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextareaEditRefusalReason {
    Producer,
    Native(TextareaHistoryError),
    /// The producer did not prepare an exact, opted-in composition update.
    /// Ordinary atomic command plans cannot stand in for preedit changes.
    CompositionUnsupported,
}
#[derive(Clone)]
pub struct TextareaEditRefusal {
    pub request: TextareaEditRequest,
    pub reason: TextareaEditRefusalReason,
}

impl TextareaState {
    /// Route explicit copy/cut to the subscribed rich clipboard owner. Without
    /// this opt-in the existing native plain-text clipboard behavior is kept.
    pub fn set_clipboard_interception(
        &mut self,
        captured: &InputHistorySnapshot,
        enabled: bool,
    ) -> Result<(), TextareaHistoryError> {
        self.check_history_program_capture(captured)?;
        self.textarea_clipboard_interception = enabled;
        Ok(())
    }
    pub fn set_paste_interception(
        &mut self,
        captured: &InputHistorySnapshot,
        enabled: bool,
    ) -> Result<(), TextareaHistoryError> {
        self.check_history_program_capture(captured)?;
        self.textarea_paste_interception = enabled;
        Ok(())
    }
    /// Install/retire the actual producer against an original native capture.
    /// Atomic commands, typing groups and composition use explicit prepared
    /// plans. There is no unassociated native fallthrough when installed: the
    /// producer owns Source command/fallback ordering and canonical values.
    pub fn set_before_edit_handler(
        &mut self,
        captured: &InputHistorySnapshot,
        handler: Option<TextareaBeforeEditHandler>,
    ) -> Result<(), TextareaHistoryError> {
        self.check_history_program_capture(captured)?;
        self.textarea_before_edit = handler;
        Ok(())
    }
}

impl<M: InputModeKind> InputBaseState<M> {
    pub(in crate::input) fn intercept_textarea_edit(
        &mut self,
        kind: TextareaEditKind,
        mut replacements: Vec<TextareaReplacement>,
        range_utf16: Option<Range<usize>>,
        composition_selection_utf16: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(handler) = self.textarea_before_edit.clone() else {
            return false;
        };
        // Checked program application uses the kernel-private validated path;
        // never reenter the producer for its already validated native stages.
        if self.validated_token_edit || self.replaying_history || self.undo_manager.is_ignoring() {
            return false;
        }
        if range_utf16.is_none()
            && matches!(kind, TextareaEditKind::Text | TextareaEditKind::Composition)
            && let Some(marked) = self.ime_marked_range
            && let [replacement] = replacements.as_mut_slice()
        {
            replacement.range = self.normalize_token_range(marked.start..marked.end);
        }
        let request = TextareaEditRequest {
            captured: self.history_snapshot(),
            kind,
            replacements,
            range_utf16,
            composition_selection_utf16,
        };
        let ime = kind == TextareaEditKind::Composition
            || (kind == TextareaEditKind::Text && request.captured.composing);
        let result = match self.check_history_program_capture_composition(&request.captured, ime) {
            Err(error) => Err(TextareaEditRefusalReason::Native(error)),
            Ok(()) => match handler(&request) {
                TextareaEditDecision::Refuse => Err(TextareaEditRefusalReason::Producer),
                TextareaEditDecision::Apply(ref plan) if ime != plan.composition => {
                    Err(TextareaEditRefusalReason::CompositionUnsupported)
                }
                TextareaEditDecision::Apply(plan) => {
                    if plan.submit_enter
                        && (!plan.stages.is_empty()
                            || !matches!(kind, TextareaEditKind::Enter { .. }))
                    {
                        if self.emit_events {
                            cx.emit(TextareaEditRefusal {
                                request,
                                reason: TextareaEditRefusalReason::Producer,
                            });
                        }
                        return true;
                    }
                    let group = plan.group(&request);
                    // IME must match the exact native replacement projection.
                    // A structural command cannot impersonate a preedit update.
                    if ime && group.is_none() {
                        if self.emit_events {
                            cx.emit(TextareaEditRefusal {
                                request,
                                reason: TextareaEditRefusalReason::CompositionUnsupported,
                            });
                        }
                        return true;
                    }
                    let composition = if ime {
                        let range = group.as_ref().expect("checked composition group").2.clone();
                        Some(
                            (kind == TextareaEditKind::Composition && !range.is_empty())
                                .then_some(range),
                        )
                    } else {
                        None
                    };
                    match self.apply_history_program_grouped(
                        &request.captured,
                        &plan.stages,
                        group,
                        composition,
                        window,
                        cx,
                    ) {
                        Err(error) => Err(TextareaEditRefusalReason::Native(error)),
                        Ok(snapshots) => {
                            (plan.associate)(&request, &snapshots);
                            if plan.submit_enter
                                && self.emit_events
                                && let TextareaEditKind::Enter {
                                    shift, secondary, ..
                                } = kind
                            {
                                cx.emit(crate::input::InputEvent::PressEnter { shift, secondary });
                            }
                            Ok(())
                        }
                    }
                }
            },
        };
        if let Err(reason) = result {
            if self.emit_events {
                cx.emit(TextareaEditRefusal { request, reason });
            }
        }
        true
    }

    pub(in crate::input) fn textarea_replacements(&self, text: &str) -> Vec<TextareaReplacement> {
        self.selections
            .iter()
            .map(|s| TextareaReplacement {
                range: self.normalize_token_range(s.start..s.end),
                content: InputContent::new(text.to_owned()),
            })
            .collect()
    }
}
