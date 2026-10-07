//! Persistent desktop pane preferences. Resize completion is the persistence boundary.
use super::*;
use gpui_kit::AnyElement;
use gpui_kit::component::{
    Sizable,
    button::{Button, ButtonVariants},
    menu::{DropdownMenu, PopupMenuItem},
    resizable::ResizableState,
};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(super) struct LayoutPreferences {
    pub side_by_side: bool,
    pub rail_rem: f32,
    pub stacked_rem: Option<f32>,
    pub horizontal_rem: Option<f32>,
}

impl Default for LayoutPreferences {
    fn default() -> Self {
        Self {
            side_by_side: false,
            rail_rem: 15.5,
            stacked_rem: None,
            horizontal_rem: None,
        }
    }
}

impl LayoutPreferences {
    pub fn load() -> Self {
        #[cfg(not(test))]
        if let Some(value) = crate::api::compat::settings::preference("workbench-layout") {
            return serde_json::from_str::<Self>(&value)
                .unwrap_or_default()
                .clamped();
        }
        Self::default()
    }

    fn clamped(mut self) -> Self {
        self.rail_rem = if self.rail_rem.is_finite() {
            self.rail_rem.clamp(10., 30.)
        } else {
            15.5
        };
        self.stacked_rem = self
            .stacked_rem
            .filter(|v| v.is_finite())
            .map(|v| v.clamp(4., 100.));
        self.horizontal_rem = self
            .horizontal_rem
            .filter(|v| v.is_finite())
            .map(|v| v.clamp(16., 160.));
        self
    }

    pub fn composer_size(&self, window: &Window) -> Option<gpui_kit::Pixels> {
        let rem = window.rem_size();
        if self.side_by_side {
            self.horizontal_rem.map(|value| {
                (rem * value).min(
                    (window.viewport_size().width - rem * (self.rail_rem + 20.)).max(rem * 16.),
                )
            })
        } else {
            self.stacked_rem.map(|value| {
                (rem * value).min((window.viewport_size().height - rem * 24.).max(rem * 4.))
            })
        }
    }
}

impl WorkbenchPanel {
    pub(super) fn persist_layout(&self, cx: &mut Context<Self>) {
        #[cfg(not(test))]
        if let Ok(value) = serde_json::to_string(&self.ux.layout)
            && let Err(error) =
                crate::api::compat::settings::set_preference("workbench-layout", &value)
        {
            crate::api::compat::notify::warning(
                cx,
                format!("Layout changed, but could not be saved: {error}"),
            );
        }
        let _ = cx;
    }

    pub(super) fn set_layout(&mut self, side_by_side: bool, cx: &mut Context<Self>) {
        if self.ux.layout.side_by_side != side_by_side {
            self.ux.layout.side_by_side = side_by_side;
            self.ux.response_split = cx.new(|_| ResizableState::default());
            self.persist_layout(cx);
            cx.notify();
        }
    }

    pub(super) fn reset_layout(&mut self, cx: &mut Context<Self>) {
        self.ux.layout = LayoutPreferences::default();
        self.ux.rail_split = cx.new(|_| ResizableState::default());
        self.ux.response_split = cx.new(|_| ResizableState::default());
        self.persist_layout(cx);
        cx.notify();
    }

    pub(super) fn render_layout_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let handle = cx.entity().downgrade();
        let side = self.ux.layout.side_by_side;
        Button::new("workbench-layout")
            .ghost()
            .small()
            .label("Layout")
            .debug_selector(|| "workbench-layout".into())
            .dropdown_menu(move |mut menu, _, _| {
                for (label, value) in [("Stacked", false), ("Side by side", true)] {
                    let handle = handle.clone();
                    menu = menu.item(PopupMenuItem::new(label).checked(side == value).on_click(
                        move |_, _, cx| {
                            let _ = handle.update(cx, |panel, cx| panel.set_layout(value, cx));
                        },
                    ));
                }
                let handle = handle.clone();
                menu.separator()
                    .item(
                        PopupMenuItem::new("Reset layout").on_click(move |_, _, cx| {
                            let _ = handle.update(cx, |panel, cx| panel.reset_layout(cx));
                        }),
                    )
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn persisted_layout_roundtrips_and_clamps_invalid_sizes() {
        let layout = LayoutPreferences {
            side_by_side: true,
            rail_rem: 999.,
            stacked_rem: Some(-1.),
            horizontal_rem: Some(42.),
        }
        .clamped();
        let restored: LayoutPreferences =
            serde_json::from_str(&serde_json::to_string(&layout).unwrap()).unwrap();
        assert!(restored.side_by_side);
        assert_eq!(restored.rail_rem, 30.);
        assert_eq!(restored.stacked_rem, Some(4.));
        assert_eq!(restored.horizontal_rem, Some(42.));
    }
}
