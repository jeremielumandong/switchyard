//! Native shaped rich rows over the existing input buffer and history capture.
//! Marker glyphs own no document bytes; their hit tests return the original
//! boundary. Resource/source authorization and rich document edits remain with
//! the application producer, never this presentation object.
use crate::input::{InlineTokenSpan, InputHistorySnapshot, InputHistoryStateId};
use gpui::{
    App, HighlightStyle, Pixels, Point, ShapedLine, SharedString, TextAlign, TextRun, Window,
    point, px,
};
use std::{ops::Range, sync::Arc};

#[derive(Clone, Debug, PartialEq)]
pub enum RichPiece {
    Text {
        range: Range<usize>,
        style: HighlightStyle,
    },
    Atom {
        range: Range<usize>,
    },
    /// Source closers sort before openers. At the other caret stop, Source
    /// adds3 to both sides, placing the markers after the same document cursor.
    Marker {
        at: usize,
        side: i8,
        text: SharedString,
        style: HighlightStyle,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RichProjectionError {
    StaleCapture,
    CompositionActive,
    InvalidRange,
    ChangedSource,
    InvalidMarker,
    MissingAtomMeasurement,
    InvalidMeasurement,
    PaintFailed,
}
impl std::fmt::Display for RichProjectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Native rich presentation refused: {self:?}")
    }
}
impl std::error::Error for RichProjectionError {}

#[derive(Clone)]
pub struct RichProjection {
    original: Arc<InputHistorySnapshot>,
    pieces: Vec<RichPiece>,
}
impl RichProjection {
    /// Capture actual native provenance with the complete current flat buffer
    /// and tokens. Validate every piece before publishing any presentation.
    pub fn checked(
        original: InputHistorySnapshot,
        pieces: Vec<RichPiece>,
    ) -> Result<Self, RichProjectionError> {
        Self::validate(original, pieces, false)
    }
    /// Render an already committed native preedit capture. Pending edits remain
    /// forbidden, and a change in composition state retires this projection.
    pub fn checked_preedit(
        original: InputHistorySnapshot,
        pieces: Vec<RichPiece>,
    ) -> Result<Self, RichProjectionError> {
        Self::validate(original, pieces, true)
    }
    fn validate(
        original: InputHistorySnapshot,
        mut pieces: Vec<RichPiece>,
        allow_preedit: bool,
    ) -> Result<Self, RichProjectionError> {
        if (original.composing && !allow_preedit) || original.topology.pending.is_some() {
            return Err(RichProjectionError::CompositionActive);
        }
        let text = original.content.text();
        let mut ranges = Vec::new();
        for piece in &pieces {
            match piece {
                RichPiece::Text { range, .. } | RichPiece::Atom { range } => {
                    if range.start >= range.end
                        || range.end > text.len()
                        || !text.is_char_boundary(range.start)
                        || !text.is_char_boundary(range.end)
                    {
                        return Err(RichProjectionError::InvalidRange);
                    }
                    let atom = original
                        .content
                        .tokens()
                        .iter()
                        .find(|atom| atom.range() == *range);
                    let overlaps = original.content.tokens().iter().any(|atom| {
                        range.start < atom.range().end && range.end > atom.range().start
                    });
                    if matches!(piece, RichPiece::Atom { .. }) && atom.is_none()
                        || matches!(piece, RichPiece::Text { .. }) && overlaps
                    {
                        return Err(RichProjectionError::ChangedSource);
                    }
                    ranges.push(range.clone());
                }
                RichPiece::Marker {
                    at,
                    side,
                    text: marker,
                    ..
                } => {
                    if *at > text.len()
                        || !text.is_char_boundary(*at)
                        || original
                            .content
                            .tokens()
                            .iter()
                            .any(|atom| *at > atom.range().start && *at < atom.range().end)
                    {
                        return Err(RichProjectionError::InvalidRange);
                    }
                    if !matches!(side, -2 | -1 | 1 | 2)
                        || marker.is_empty()
                        || marker.len() > 32
                        || !marker
                            .bytes()
                            .all(|b| matches!(b, b'*' | b'_' | b'~' | b'`'))
                    {
                        return Err(RichProjectionError::InvalidMarker);
                    }
                }
            }
        }
        ranges.sort_by_key(|range| range.start);
        let mut offset = 0;
        for range in ranges {
            if range.start != offset {
                return Err(RichProjectionError::ChangedSource);
            }
            offset = range.end;
        }
        if offset != text.len() {
            return Err(RichProjectionError::ChangedSource);
        }
        // A marker splits text at its real boundary. Do not let an arbitrary
        // source-order vector paint it after a whole unsplit text span.
        for piece in &pieces {
            if let RichPiece::Marker { at, .. } = piece
                && pieces.iter().any(|piece| matches!(piece,RichPiece::Text {range,..} if *at>range.start && *at<range.end)) {
                    return Err(RichProjectionError::InvalidRange);
            }
        }
        pieces.sort_by_key(|piece| match piece {
            RichPiece::Text { range, .. } | RichPiece::Atom { range } => (range.start, 3i8),
            RichPiece::Marker { at, side, .. } => (*at, *side),
        });
        Ok(Self {
            original: Arc::new(original),
            pieces,
        })
    }
    pub fn state_id(&self) -> InputHistoryStateId {
        self.original.state_id
    }
    pub fn pieces(&self) -> &[RichPiece] {
        &self.pieces
    }
    /// Original native IDs/epochs/content are coherence, never a resource grant.
    /// Selection movement away and back still retires an older marker layout.
    pub fn is_current(&self, current: &InputHistorySnapshot) -> bool {
        capture_is_current(&self.original, current)
    }
    /// Shape a real current row. Atom widths must come from the existing native
    /// token renderer's actual layout cache; no guessed width is substituted.
    pub fn shape_line(
        &self,
        current: &InputHistorySnapshot,
        range: Range<usize>,
        font_size: Pixels,
        window: &mut Window,
        mut measured_atom: impl FnMut(&InlineTokenSpan) -> Option<Pixels>,
    ) -> Result<RichLine, RichProjectionError> {
        if !self.is_current(current) {
            return Err(RichProjectionError::StaleCapture);
        }
        let text = self.original.content.text();
        if range.start > range.end
            || range.end > text.len()
            || !text.is_char_boundary(range.start)
            || !text.is_char_boundary(range.end)
        {
            return Err(RichProjectionError::InvalidRange);
        }
        if text[range.clone()].contains('\n') {
            return Err(RichProjectionError::InvalidRange);
        }
        if !f32::from(font_size).is_finite() || font_size <= px(0.) {
            return Err(RichProjectionError::InvalidMeasurement);
        }
        let mut fragments = Vec::new();
        let mut group = GlyphGroup::default();
        let mut x = px(0.);
        let base = window.text_style();
        for piece in &self.pieces {
            let (source, glyphs, style, side) = match piece {
                RichPiece::Marker {
                    at,
                    side,
                    text: marker_text,
                    style,
                } if marker_in_row(*at, *side, &range, text) => {
                    (*at..*at, Some(marker_text.clone()), Some(*style), *side)
                }
                RichPiece::Text {
                    range: source,
                    style,
                } if source.start < range.end && source.end > range.start => {
                    let source = source.start.max(range.start)..source.end.min(range.end);
                    (
                        source.clone(),
                        Some(text[source].to_owned().into()),
                        Some(*style),
                        0,
                    )
                }
                RichPiece::Atom { range: source }
                    if source.start < range.end && source.end > range.start =>
                {
                    if source.start < range.start || source.end > range.end {
                        return Err(RichProjectionError::InvalidRange);
                    }
                    (source.clone(), None, None, 0)
                }
                _ => continue,
            };
            if let Some(glyphs) = glyphs {
                // One native line layout owns contiguous glyphs, including
                // virtual markers. Independent span shaping loses kerning,
                // ligatures, combining clusters and font letter spacing.
                let start = group.text.len();
                group.text.push_str(&glyphs);
                group
                    .runs
                    .push(base.clone().highlight(style.unwrap()).to_run(glyphs.len()));
                group.spans.push(GlyphSpan {
                    range: source,
                    glyphs: start..group.text.len(),
                    side,
                });
            } else {
                group.append(&mut fragments, &mut x, font_size, window)?;
                let atom = self
                    .original
                    .content
                    .tokens()
                    .iter()
                    .find(|atom| atom.range() == source)
                    .ok_or(RichProjectionError::ChangedSource)?;
                let width =
                    measured_atom(atom).ok_or(RichProjectionError::MissingAtomMeasurement)?;
                checked_width(width)?;
                fragments.push(Fragment {
                    range: source,
                    x,
                    width,
                    shaped: None,
                    spans: Vec::new(),
                });
                x += width;
                checked_width(x)?;
            }
        }
        group.append(&mut fragments, &mut x, font_size, window)?;
        Ok(RichLine {
            original: self.original.clone(),
            range,
            width: x,
            fragments,
        })
    }
}
fn capture_is_current(original: &InputHistorySnapshot, current: &InputHistorySnapshot) -> bool {
    current.composing == original.composing
        && current.topology.pending.is_none()
        && current.state_id == original.state_id
        && current.text_revision == original.text_revision
        && current.selection_revision == original.selection_revision
        && current.caret_phase == original.caret_phase
        && current.stored_marks == original.stored_marks
        && current.content == original.content
        && current.selections == original.selections
}
// At a soft row boundary, negative Source widgets stay with the preceding
// text and positive widgets with the following text. Hard newlines retain
// every marker at their own logical boundary, including empty paragraphs.
fn marker_in_row(at: usize, side: i8, row: &Range<usize>, text: &str) -> bool {
    (at > row.start && at < row.end)
        || (at == row.start && (at == 0 || text.as_bytes().get(at - 1) == Some(&b'\n') || side > 0))
        || (at == row.end
            && (at == text.len() || text.as_bytes().get(at) == Some(&b'\n') || side < 0))
}
struct GlyphSpan {
    range: Range<usize>,
    glyphs: Range<usize>,
    side: i8,
}
#[derive(Default)]
struct GlyphGroup {
    text: String,
    runs: Vec<TextRun>,
    spans: Vec<GlyphSpan>,
}
impl GlyphGroup {
    fn append(
        &mut self,
        fragments: &mut Vec<Fragment>,
        x: &mut Pixels,
        font_size: Pixels,
        window: &mut Window,
    ) -> Result<(), RichProjectionError> {
        if self.text.is_empty() {
            return Ok(());
        }
        let group = std::mem::take(self);
        let line = window
            .text_system()
            .shape_line(group.text.into(), font_size, &group.runs, None);
        checked_width(line.width)?;
        let range = group.spans.first().unwrap().range.start..group.spans.last().unwrap().range.end;
        let width = line.width;
        fragments.push(Fragment {
            range,
            x: *x,
            width,
            shaped: Some(line),
            spans: group.spans,
        });
        *x += width;
        checked_width(*x)
    }
}
fn checked_width(width: Pixels) -> Result<(), RichProjectionError> {
    if !f32::from(width).is_finite() || width < px(0.) {
        return Err(RichProjectionError::InvalidMeasurement);
    }
    Ok(())
}
struct Fragment {
    range: Range<usize>,
    x: Pixels,
    width: Pixels,
    shaped: Option<ShapedLine>,
    spans: Vec<GlyphSpan>,
}
pub struct RichLine {
    original: Arc<InputHistorySnapshot>,
    range: Range<usize>,
    width: Pixels,
    fragments: Vec<Fragment>,
}
impl RichLine {
    pub fn range(&self) -> Range<usize> {
        self.range.clone()
    }
    pub fn width(&self) -> Pixels {
        self.width
    }
    /// Physical atom extent differs from caret stops when boundary markers
    /// precede or follow the cursor. Return only the actual shaped fragment.
    pub fn atom_extent(&self, range: &Range<usize>) -> Option<Range<Pixels>> {
        self.fragments.iter().find_map(|fragment| {
            (fragment.shaped.is_none() && fragment.range == *range)
                .then(|| fragment.x..fragment.x + fragment.width)
        })
    }
    /// Two native caret stops share a buffer boundary. Actual Source widget
    /// sides decide whether marker glyphs precede or follow this caret.
    pub fn x_for_caret(&self, offset: usize) -> Option<Pixels> {
        if offset < self.range.start
            || offset > self.range.end
            || !self.original.content.text().is_char_boundary(offset)
            || self
                .original
                .content
                .tokens()
                .iter()
                .any(|atom| offset > atom.range().start && offset < atom.range().end)
        {
            return None;
        }
        // Markers at a source boundary are in one contiguous glyph group.
        // Use native glyph indices, never a sum of independently shaped widths.
        for f in &self.fragments {
            if let Some(line) = &f.shaped {
                let mut markers = f
                    .spans
                    .iter()
                    .filter(|span| span.range.is_empty() && span.range.start == offset);
                if let Some(first) = markers.next() {
                    let mut glyph = if first.side < 0 {
                        first.glyphs.end
                    } else {
                        first.glyphs.start
                    };
                    for marker in markers {
                        if marker.side < 0 {
                            glyph = marker.glyphs.end;
                        }
                    }
                    return Some(f.x + line.x_for_index(glyph));
                }
            }
        }
        for f in &self.fragments {
            if let Some(line) = &f.shaped {
                for span in &f.spans {
                    if !span.range.is_empty()
                        && offset >= span.range.start
                        && offset <= span.range.end
                    {
                        return Some(
                            f.x + line.x_for_index(span.glyphs.start + offset - span.range.start),
                        );
                    }
                }
            } else if offset >= f.range.start && offset <= f.range.end {
                return Some(
                    f.x + if offset == f.range.start {
                        px(0.)
                    } else {
                        f.width
                    },
                );
            }
        }
        Some(self.width)
    }
    /// A marker owns no input character. Map the actual native shaped glyph
    /// index back to the immutable buffer, including a combining cluster.
    pub fn closest_index_for_x(&self, x: Pixels) -> Option<usize> {
        if !f32::from(x).is_finite() {
            return None;
        }
        for f in &self.fragments {
            if x <= f.x + f.width {
                if let Some(line) = &f.shaped {
                    let glyph = line.closest_index_for_x(x - f.x);
                    for span in &f.spans {
                        if glyph <= span.glyphs.end {
                            return Some(if span.range.is_empty() {
                                span.range.start
                            } else {
                                span.range.start
                                    + glyph
                                        .saturating_sub(span.glyphs.start)
                                        .min(span.range.len())
                            });
                        }
                    }
                    return Some(f.range.end);
                }
                return Some(if x - f.x < f.width / 2. {
                    f.range.start
                } else {
                    f.range.end
                });
            }
        }
        Some(self.range.end)
    }
    pub fn paint(
        &self,
        current: &InputHistorySnapshot,
        origin: Point<Pixels>,
        height: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<(), RichProjectionError> {
        self.check_paint(current, origin, height)?;
        for f in &self.fragments {
            if let Some(line) = &f.shaped {
                line.paint(
                    origin + point(f.x, px(0.)),
                    height,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                )
                .map_err(|_| RichProjectionError::PaintFailed)?;
            }
        }
        Ok(())
    }
    pub fn paint_background(
        &self,
        current: &InputHistorySnapshot,
        origin: Point<Pixels>,
        height: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<(), RichProjectionError> {
        self.check_paint(current, origin, height)?;
        for f in &self.fragments {
            if let Some(line) = &f.shaped {
                line.paint_background(
                    origin + point(f.x, px(0.)),
                    height,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                )
                .map_err(|_| RichProjectionError::PaintFailed)?;
            }
        }
        Ok(())
    }
    fn check_paint(
        &self,
        current: &InputHistorySnapshot,
        origin: Point<Pixels>,
        height: Pixels,
    ) -> Result<(), RichProjectionError> {
        if !capture_is_current(&self.original, current) {
            return Err(RichProjectionError::StaleCapture);
        }
        if !f32::from(origin.x).is_finite()
            || !f32::from(origin.y).is_finite()
            || !f32::from(height).is_finite()
            || height <= px(0.)
        {
            return Err(RichProjectionError::InvalidMeasurement);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::TextareaState;
    use gpui::{AppContext, TestAppContext};
    #[gpui::test]
    fn native_markers_use_original_document_tail_hard_start_and_newline_in_both_phases(
        cx: &mut TestAppContext,
    ) {
        let window = cx.add_empty_window();
        window.update(|window, cx| {
            for text in ["bold", "bold\nnext", "bold\n\nnext"] {
                let input = cx.new(|cx| {
                    let mut input = TextareaState::new(window, cx);
                    input.set_value(text, window, cx);
                    input
                });
                let capture = input.read(cx).history_snapshot();
                let mut rows = vec![0..4];
                if text.len() > 4 {
                    rows.push(text.len() - 4..text.len());
                }
                let mut base = Vec::new();
                let mut offset = 0;
                for row in &rows {
                    if offset < row.start {
                        base.push(RichPiece::Text {
                            range: offset..row.start,
                            style: HighlightStyle::default(),
                        });
                    }
                    base.push(RichPiece::Text {
                        range: row.clone(),
                        style: HighlightStyle::default(),
                    });
                    base.push(RichPiece::Marker {
                        at: row.start,
                        side: -1,
                        text: "**".into(),
                        style: HighlightStyle::default(),
                    });
                    base.push(RichPiece::Marker {
                        at: row.end,
                        side: -2,
                        text: "**".into(),
                        style: HighlightStyle::default(),
                    });
                    offset = row.end;
                }
                let before = RichProjection::checked(capture.clone(), base.clone()).unwrap();
                for piece in &mut base {
                    if let RichPiece::Marker { side, .. } = piece {
                        *side += 3;
                    }
                }
                let after = RichProjection::checked(capture.clone(), base).unwrap();
                for row in &rows {
                    let left = before
                        .shape_line(&capture, row.clone(), px(14.), window, |_| None)
                        .unwrap();
                    let right = after
                        .shape_line(&capture, row.clone(), px(14.), window, |_| None)
                        .unwrap();
                    assert_eq!(
                        left.width(),
                        right.width(),
                        "Original document {text:?}, row {row:?}"
                    );
                    assert!(
                        left.x_for_caret(row.start).unwrap()
                            > right.x_for_caret(row.start).unwrap()
                    );
                    assert!(
                        left.x_for_caret(row.end).unwrap() > right.x_for_caret(row.end).unwrap()
                    );
                    for line in [&left, &right] {
                        assert_eq!(line.closest_index_for_x(line.width()), Some(row.end));
                    }
                }
                if text == "bold\n\nnext" {
                    let empty = before
                        .shape_line(&capture, 5..5, px(14.), window, |_| None)
                        .unwrap();
                    assert_eq!(empty.width(), px(0.));
                    assert_eq!(empty.x_for_caret(5), Some(px(0.)));
                    assert_eq!(empty.closest_index_for_x(px(0.)), Some(5));
                }
                assert!(before.is_current(&input.read(cx).history_snapshot()));
                assert!(capture.topology.undo.is_empty());
            }
        });
    }
}
