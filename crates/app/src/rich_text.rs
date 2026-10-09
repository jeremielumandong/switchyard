//! Styled text that takes part in the window's text selection: `gpui-base`'s
//! `SelectableText` (Apache-2.0) with highlight runs (bold, italic, inline code, links) and
//! clickable links. Copy gives the text as shown.

use std::ops::Range;

use gpui_kit::base::{
    TextSelection, TextSelectionHandle, TextSelectionRegistration, TextSelectionRun, Theme,
};
use gpui_kit::{
    App, BorderStyle, Bounds, Corners, CursorStyle, Edges, Element, ElementId, GlobalElementId,
    HighlightStyle, Hitbox, HitboxBehavior, Hsla, InspectorElementId, IntoElement, LayoutId,
    MouseButton, MouseUpEvent, PaintQuad, Pixels, Point, SharedString, StyledText, Window,
    transparent_black,
};

/// Selectable text with highlighted ranges and links.
pub struct RichText {
    id: ElementId,
    text: SharedString,
    styled: StyledText,
    order: u64,
    links: Vec<(Range<usize>, String)>,
}

impl RichText {
    /// `highlights` and `fonts` are sorted by start and do not overlap; `links` are byte
    /// ranges of `text` that open their URL on click.
    pub fn new(
        id: impl Into<ElementId>,
        text: impl Into<SharedString>,
        highlights: Vec<(Range<usize>, HighlightStyle)>,
        fonts: Vec<(Range<usize>, SharedString)>,
        links: Vec<(Range<usize>, String)>,
    ) -> Self {
        let text = text.into();
        let mut styled = StyledText::new(text.clone());
        if !highlights.is_empty() {
            styled = styled.with_highlights(highlights);
        }
        if !fonts.is_empty() {
            styled = styled.with_font_family_overrides(fonts);
        }
        Self {
            id: id.into(),
            text,
            styled,
            order: 0,
            links,
        }
    }

    /// Places this run in reading order among the window's other selectable runs.
    pub fn document_order(mut self, order: u64) -> Self {
        self.order = order;
        self
    }

    fn link_at(&self, position: Point<Pixels>) -> Option<&str> {
        if self.links.is_empty() {
            return None;
        }
        let ix = self.styled.layout().index_for_position(position).ok()?;
        self.links
            .iter()
            .find(|(r, _)| r.contains(&ix))
            .map(|(_, url)| url.as_str())
    }
}

fn selection_quads(
    start: Point<Pixels>,
    end: Point<Pixels>,
    bounds: Bounds<Pixels>,
    line_height: Pixels,
) -> Vec<Bounds<Pixels>> {
    if start.y == end.y {
        return vec![Bounds::from_corners(
            start,
            Point::new(end.x, end.y + line_height),
        )];
    }
    let mut quads = vec![Bounds::from_corners(
        start,
        Point::new(bounds.right(), start.y + line_height),
    )];
    if end.y > start.y + line_height {
        quads.push(Bounds::from_corners(
            Point::new(bounds.left(), start.y + line_height),
            Point::new(bounds.right(), end.y),
        ));
    }
    quads.push(Bounds::from_corners(
        Point::new(bounds.left(), end.y),
        Point::new(end.x, end.y + line_height),
    ));
    quads
}

impl IntoElement for RichText {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for RichText {
    type RequestLayoutState = Option<TextSelectionHandle>;
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        // The id is always set (`Element::id`), so GPUI passes a global id.
        let handle = global_id.map(|gid| {
            window.with_element_state(gid, |retained: Option<TextSelectionHandle>, _| {
                let h = retained.unwrap_or_else(|| TextSelectionHandle::new(self.text.clone(), cx));
                (h.clone(), h)
            })
        });
        let (layout_id, ()) = self
            .styled
            .request_layout(global_id, inspector_id, window, cx);
        (layout_id, handle)
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        handle: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.styled
            .prepaint(global_id, inspector_id, bounds, &mut (), window, cx);
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        if let Some(handle) = handle {
            let registration = TextSelectionRegistration::new(hitbox.clone(), bounds)
                .with_document_order(self.order)
                .with_text_bounds(vec![bounds])
                .with_rendered_element(handle, window, cx);
            handle.register(registration, window, cx);
        }
        hitbox
    }

    fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        handle: &mut Self::RequestLayoutState,
        hitbox: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let layout = self.styled.layout().clone();
        if let Some(handle) = handle {
            let before = TextSelection::selected_text(window, cx);
            let projection = handle.update_runs(
                &[
                    TextSelectionRun::new(self.text.clone(), layout.clone(), bounds)
                        .with_document_order(self.order),
                ],
                cx,
            );
            if before != TextSelection::selected_text(window, cx) {
                window.refresh();
            }
            let color: Hsla = Theme::global(cx).tokens.colors.selection;
            for range in projection.ranges().iter().flatten().cloned() {
                let (Some(start), Some(end)) = (
                    layout.position_for_index(range.start),
                    layout.position_for_index(range.end),
                ) else {
                    continue;
                };
                for quad in selection_quads(start, end, layout.bounds(), layout.line_height()) {
                    window.paint_quad(PaintQuad {
                        bounds: quad,
                        background: color.into(),
                        corner_radii: Corners::default(),
                        border_widths: Edges::default(),
                        border_color: transparent_black(),
                        border_style: BorderStyle::default(),
                    });
                }
            }
        }
        self.styled.paint(
            global_id,
            inspector_id,
            bounds,
            &mut (),
            &mut (),
            window,
            cx,
        );

        if self.links.is_empty() {
            return;
        }
        if hitbox.is_hovered(window) && self.link_at(window.mouse_position()).is_some() {
            window.set_cursor_style(CursorStyle::PointingHand, hitbox);
        }
        let hitbox = hitbox.clone();
        let links = self.links.clone();
        window.on_mouse_event(move |e: &MouseUpEvent, phase, window, cx| {
            if !phase.bubble() || e.button != MouseButton::Left || !hitbox.is_hovered(window) {
                return;
            }
            // A drag that selected text is not a click.
            if TextSelection::has_selection(window, cx) {
                return;
            }
            let Ok(ix) = layout.index_for_position(e.position) else {
                return;
            };
            if let Some((_, url)) = links.iter().find(|(r, _)| r.contains(&ix)) {
                cx.open_url(url);
            }
        });
    }
}
