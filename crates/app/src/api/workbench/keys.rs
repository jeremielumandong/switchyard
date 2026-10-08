//! Panel-level keyboard commands: focus the URL bar and switch between the
//! six header tabs. Bound in [`crate::api::compat::bind_keys`] under the
//! Workbench's [`KEY_CONTEXT`], so they never fire from the SQL tab.

use super::*;
use gpui_kit::{Div, Stateful};

gpui_kit::actions!(
    workbench,
    [
        FocusUrl,
        ShowCompose,
        ShowImport,
        ShowRunner,
        ShowEnvs,
        ShowHistory,
        ShowDiff
    ]
);

impl Tab {
    /// The action that switches to this tab (its `secondary-N` binding).
    pub(super) fn action(self) -> Box<dyn gpui_kit::Action> {
        match self {
            Self::Compose => Box::new(ShowCompose),
            Self::Import => Box::new(ShowImport),
            Self::Data => Box::new(ShowRunner),
            Self::Environments => Box::new(ShowEnvs),
            Self::History => Box::new(ShowHistory),
            Self::Diff => Box::new(ShowDiff),
        }
    }
}

impl WorkbenchPanel {
    /// Attach the tab-switching and focus-URL handlers to the panel root.
    pub(super) fn on_panel_key_actions(
        &self,
        root: Stateful<Div>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let root = switch_on::<ShowCompose>(root, Tab::Compose, cx);
        let root = switch_on::<ShowImport>(root, Tab::Import, cx);
        let root = switch_on::<ShowRunner>(root, Tab::Data, cx);
        let root = switch_on::<ShowEnvs>(root, Tab::Environments, cx);
        let root = switch_on::<ShowHistory>(root, Tab::History, cx);
        let root = switch_on::<ShowDiff>(root, Tab::Diff, cx);
        root.on_action(cx.listener(|this, _: &FocusUrl, window, cx| {
            this.show_tab(Tab::Compose, cx);
            this.url.focus_handle(cx).focus(window, cx);
        }))
    }

    fn show_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        if self.tab != tab {
            self.tab = tab;
            cx.notify();
        }
    }
}

/// Switch to `tab` when `A` is dispatched inside the panel.
fn switch_on<A: gpui_kit::Action>(
    root: Stateful<Div>,
    tab: Tab,
    cx: &mut Context<WorkbenchPanel>,
) -> Stateful<Div> {
    root.on_action(cx.listener(move |this, _: &A, _, cx| this.show_tab(tab, cx)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tab_has_its_own_switch_action() {
        let names: HashSet<_> = Tab::ALL.iter().map(|tab| tab.action().name()).collect();
        assert_eq!(names.len(), Tab::ALL.len());
    }
}
