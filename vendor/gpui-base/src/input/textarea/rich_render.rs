//! Captured rich rows in the existing input layout, wrapping and paint path.
use super::{RichLine, RichProjection, RichProjectionError, TextareaState};
use crate::input::{InlineToken, InputHistorySnapshot, WrappingIndent};
use gpui::{Context, Pixels, TextStyle, Window, px};
use smallvec::SmallVec;
use std::{collections::HashMap, ops::Range, rc::Rc, sync::Arc};
use unicode_segmentation::UnicodeSegmentation as _;

pub(crate) struct Row {
    pub(crate) ranges: SmallVec<[Range<usize>; 1]>,
    pub(crate) shaped: SmallVec<[Rc<RichLine>; 1]>,
    pub(crate) indent: usize,
    pub(crate) indent_width: Pixels,
}
pub(crate) struct Layout {
    pub(crate) projection: Arc<RichProjection>,
    pub(crate) rows: Vec<Row>,
    pub(crate) width: Pixels,
    style: TextStyle,
    font_size: Pixels,
    wrap_width: Option<Pixels>,
    wrapping_indent: WrappingIndent,
    atoms: HashMap<InlineToken, Pixels>,
}
impl Layout {
    pub(crate) fn matches(
        &self,
        projection: &Arc<RichProjection>,
        style: &TextStyle,
        font_size: Pixels,
        width: Option<Pixels>,
        indent: WrappingIndent,
        atoms: &HashMap<InlineToken, Pixels>,
    ) -> bool {
        Arc::ptr_eq(&self.projection, projection)
            && self.style == *style
            && self.font_size == font_size
            && self.wrap_width == width
            && self.wrapping_indent == indent
            && &self.atoms == atoms
    }
    pub(crate) fn prepare(
        projection: Arc<RichProjection>,
        capture: &InputHistorySnapshot,
        font_size: Pixels,
        wrap_width: Option<Pixels>,
        wrapping_indent: WrappingIndent,
        atoms: HashMap<InlineToken, Pixels>,
        window: &mut Window,
    ) -> Result<Self, RichProjectionError> {
        if !projection.is_current(capture) {
            return Err(RichProjectionError::StaleCapture);
        }
        if wrap_width.is_some_and(|width| !f32::from(width).is_finite() || width <= px(0.)) {
            return Err(RichProjectionError::InvalidMeasurement);
        }
        let mut rows = Vec::new();
        let mut offset = 0;
        let mut width = px(0.);
        for text in capture.content.text().split('\n') {
            let indent = if wrapping_indent == WrappingIndent::Same {
                text.bytes()
                    .take_while(|b| *b == b' ')
                    .count()
                    .min(gpui::LineWrapper::MAX_INDENT as usize)
            } else {
                0
            };
            let mut shape = |range: Range<usize>| {
                projection.shape_line(
                    capture,
                    offset + range.start..offset + range.end,
                    font_size,
                    window,
                    |span| atoms.get(span.token()).copied(),
                )
            };
            let indent_width = if indent == 0 {
                px(0.)
            } else {
                shape(0..indent)?.width()
            };
            let mut ranges: SmallVec<[Range<usize>; 1]> = SmallVec::new();
            if let Some(available) = wrap_width {
                let ends: Vec<_> = text
                    .grapheme_indices(true)
                    .map(|(at, g)| at + g.len())
                    .filter(|at| {
                        !capture.content.tokens().iter().any(|span| {
                            span.range().start < offset + *at && offset + *at < span.range().end
                        })
                    })
                    .collect();
                let opportunities: Vec<_> = unicode_linebreak::linebreaks(text)
                    .map(|(at, _)| at)
                    .filter(|at| ends.binary_search(at).is_ok())
                    .collect();
                let mut first = 0;
                let mut start = 0;
                while first < ends.len() {
                    let available = available - if start == 0 { px(0.) } else { indent_width };
                    let remaining = ends.len() - first;
                    let (mut low, mut high) = (0, 1);
                    while shape(start..ends[first + high - 1])?.width() <= available {
                        low = high;
                        if high == remaining {
                            break;
                        }
                        high = (high * 2).min(remaining);
                    }
                    while low + 1 < high {
                        let middle = (low + high) / 2;
                        if shape(start..ends[first + middle - 1])?.width() <= available {
                            low = middle;
                        } else {
                            high = middle;
                        }
                    }
                    let fitting = ends[first + low.max(1) - 1];
                    let end = if fitting == text.len() {
                        fitting
                    } else {
                        opportunities
                            .partition_point(|at| *at <= fitting)
                            .checked_sub(1)
                            .map(|i| opportunities[i])
                            .filter(|at| *at > start && (start != 0 || *at > indent))
                            .unwrap_or(fitting)
                    };
                    ranges.push(start..end);
                    start = end;
                    first = ends.partition_point(|at| *at <= end);
                }
            }
            if ranges.is_empty() {
                ranges.push(0..text.len());
            }
            let shaped: SmallVec<[Rc<RichLine>; 1]> = ranges
                .iter()
                .map(|range| shape(range.clone()).map(Rc::new))
                .collect::<Result<SmallVec<_>, _>>()?;
            for (index, line) in shaped.iter().enumerate() {
                width = width.max(line.width() + if index == 0 { px(0.) } else { indent_width });
            }
            rows.push(Row {
                ranges,
                shaped,
                indent,
                indent_width,
            });
            offset += text.len() + 1;
        }
        Ok(Self {
            projection,
            rows,
            width,
            style: window.text_style(),
            font_size,
            wrap_width,
            wrapping_indent,
            atoms,
        })
    }
}
impl TextareaState {
    /// Install only the producer's complete current projection. This never
    /// edits text, tokens, selections or native history. Invalid/stale captures
    /// leave the previous projection untouched; retiring restores ordinary wrap.
    pub fn set_rich_projection(
        &mut self,
        projection: Option<Arc<RichProjection>>,
        cx: &mut Context<Self>,
    ) -> Result<(), RichProjectionError> {
        if let Some(projection) = &projection {
            if !projection.is_current(&self.history_snapshot()) {
                return Err(RichProjectionError::StaleCapture);
            }
            if self.masked || !self.mask_pattern.is_none() {
                return Err(RichProjectionError::ChangedSource);
            }
            if self.textarea_rich.as_ref().is_some_and(|old| {
                old.is_current(&self.history_snapshot()) && old.pieces() == projection.pieces()
            }) {
                return Ok(());
            }
        }
        self.textarea_rich = projection;
        self.textarea_rich_layout = None;
        self.textarea_rich_error = None;
        self.last_layout = None;
        if self.textarea_rich.is_none() {
            self.display_map.restore_ordinary_wrap(cx);
        }
        cx.notify();
        Ok(())
    }
    pub fn rich_projection_error(&self) -> Option<RichProjectionError> {
        self.textarea_rich_error
    }
}
