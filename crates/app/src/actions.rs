//! Actions, default keybindings and the command registry the palette searches.

use gpui_kit::{App, KeyBinding, SharedString, actions};

actions!(
    switchyard,
    [
        OpenPalette,
        QuickSwitch,
        RunStatement,
        RunScript,
        StopQuery,
        NewTerminal,
        NewConnection,
        NewHost,
        NewQueryTab,
        CloseTab,
        OpenSettings,
        ToggleSidebar,
        ToggleInspector,
        ToggleTheme,
        ShowWelcome,
        OpenComponents,
        OpenFiles,
        FormatSql,
        CommitTransaction,
        RollbackTransaction,
        RefreshSchema,
        ImportSshConfig,
        ExportProfiles,
        ShowHistory,
        Dismiss,
        TermCopy,
        TermPaste,
        TermFind,
        TermSplit,
        SplitRight,
        SplitDown,
        Unsplit,
    ]
);

/// Register default keybindings (SPEC "Key shortcuts").
pub fn init(cx: &mut App) {
    let new_terminal = if cfg!(target_os = "macos") {
        "cmd-t"
    } else {
        "ctrl-shift-t"
    };
    cx.bind_keys([
        KeyBinding::new("secondary-shift-p", OpenPalette, None),
        KeyBinding::new("secondary-p", QuickSwitch, None),
        KeyBinding::new("secondary-enter", RunStatement, Some("Workspace")),
        KeyBinding::new("secondary-shift-enter", RunScript, Some("Workspace")),
        // Registered after gpui-component's own `Input` bindings, so these win inside
        // the SQL editor instead of inserting a newline.
        KeyBinding::new("secondary-enter", RunStatement, Some("SqlTab > Input")),
        KeyBinding::new("secondary-shift-enter", RunScript, Some("SqlTab > Input")),
        KeyBinding::new("secondary-.", StopQuery, None),
        KeyBinding::new(new_terminal, NewTerminal, None),
        KeyBinding::new("secondary-n", NewConnection, None),
        KeyBinding::new("secondary-alt-n", NewQueryTab, None),
        KeyBinding::new("secondary-w", CloseTab, Some("Workspace")),
        KeyBinding::new("secondary-,", OpenSettings, None),
        KeyBinding::new("secondary-b", ToggleSidebar, None),
        KeyBinding::new("secondary-shift-f", FormatSql, Some("Workspace")),
        KeyBinding::new("secondary-shift-h", ShowHistory, None),
        KeyBinding::new("secondary-\\", SplitRight, None),
        KeyBinding::new("secondary-shift-\\", SplitDown, None),
        // Linux reports Shift+\ as `|`.
        KeyBinding::new("secondary-|", SplitDown, None),
        KeyBinding::new("escape", Dismiss, Some("Overlay")),
    ]);
    // Inside a terminal, Ctrl+letter belongs to the shell (readline, vim, …). App
    // shortcuts there use Cmd on macOS and Ctrl+Shift elsewhere.
    let mac = cfg!(target_os = "macos");
    let k = |mac_key: &'static str, other: &'static str| if mac { mac_key } else { other };
    cx.bind_keys([
        KeyBinding::new(k("cmd-c", "ctrl-shift-c"), TermCopy, Some("Terminal")),
        KeyBinding::new(k("cmd-v", "ctrl-shift-v"), TermPaste, Some("Terminal")),
        KeyBinding::new(k("cmd-f", "ctrl-shift-f"), TermFind, Some("Terminal")),
        KeyBinding::new(k("cmd-d", "ctrl-shift-d"), TermSplit, Some("Terminal")),
    ]);
    if !mac {
        cx.bind_keys(
            [
                "ctrl-p",
                "ctrl-n",
                "ctrl-b",
                "ctrl-w",
                "ctrl-.",
                "ctrl-,",
                "ctrl-alt-n",
            ]
            .map(|key| KeyBinding::new(key, gpui_kit::NoAction, Some("Terminal"))),
        );
    }
}

/// Something the palette can run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandId {
    NewConnection,
    NewHost,
    NewTerminal,
    NewQueryTab,
    RunStatement,
    RunScript,
    StopQuery,
    FormatSql,
    CommitTransaction,
    RollbackTransaction,
    OpenFiles,
    Settings,
    SettingsDrivers,
    ToggleTheme,
    ToggleInspector,
    ToggleSidebar,
    ShowWelcome,
    OpenComponents,
    RefreshSchema,
    ImportSshConfig,
    ExportProfiles,
    ShowHistory,
    SplitRight,
    SplitDown,
    Unsplit,
}

/// A palette entry.
#[derive(Clone, Debug)]
pub struct PaletteCommand {
    /// Command.
    pub id: CommandId,
    /// Label.
    pub label: SharedString,
    /// Group shown on the right.
    pub group: &'static str,
    /// Shortcut hint.
    pub key: SharedString,
}

fn k(mac: &str, other: &str) -> SharedString {
    crate::ui::keys(mac, other)
}

/// Every command the palette offers, in display order.
pub fn palette_commands() -> Vec<PaletteCommand> {
    use CommandId::*;
    let c = |id, label: &str, group, key: SharedString| PaletteCommand {
        id,
        label: label.to_owned().into(),
        group,
        key,
    };
    vec![
        c(
            NewConnection,
            "New Connection…",
            "Connections",
            k("⌘N", "Ctrl+N"),
        ),
        c(NewHost, "New Host…", "Connections", "".into()),
        c(
            ImportSshConfig,
            "Import Hosts from ~/.ssh/config",
            "Connections",
            "".into(),
        ),
        c(
            ExportProfiles,
            "Export Profiles (no secrets)…",
            "Connections",
            "".into(),
        ),
        c(NewQueryTab, "New SQL Tab", "Editor", k("⌥⌘N", "Ctrl+Alt+N")),
        c(
            RunStatement,
            "Run Statement at Cursor",
            "Editor",
            k("⌘↵", "Ctrl+Enter"),
        ),
        c(
            RunScript,
            "Run Script",
            "Editor",
            k("⇧⌘↵", "Ctrl+Shift+Enter"),
        ),
        c(StopQuery, "Stop Query", "Editor", k("⌘.", "Ctrl+.")),
        c(FormatSql, "Format SQL", "Editor", k("⇧⌘F", "Ctrl+Shift+F")),
        c(CommitTransaction, "Commit Transaction", "Editor", "".into()),
        c(
            RollbackTransaction,
            "Rollback Transaction",
            "Editor",
            "".into(),
        ),
        c(
            ShowHistory,
            "Query History",
            "Editor",
            k("⇧⌘H", "Ctrl+Shift+H"),
        ),
        c(RefreshSchema, "Refresh Schema", "Schema", "".into()),
        c(SplitRight, "Split Right", "View", k("⌘\\", "Ctrl+\\")),
        c(SplitDown, "Split Down", "View", k("⇧⌘\\", "Ctrl+Shift+\\")),
        c(Unsplit, "Close Split", "View", "".into()),
        c(
            NewTerminal,
            "New Terminal",
            "Terminal",
            k("⌘T", "Ctrl+Shift+T"),
        ),
        c(OpenFiles, "Open Files (local)", "Files", "".into()),
        c(Settings, "Settings", "Preferences", k("⌘,", "Ctrl+,")),
        c(
            SettingsDrivers,
            "Settings: Drivers",
            "Preferences",
            "".into(),
        ),
        c(
            ToggleTheme,
            "Toggle Light / Dark Theme",
            "Preferences",
            "".into(),
        ),
        c(ToggleInspector, "Toggle Inspector", "View", "".into()),
        c(ToggleSidebar, "Toggle Sidebar", "View", k("⌘B", "Ctrl+B")),
        c(ShowWelcome, "Show Welcome", "View", "".into()),
        c(OpenComponents, "Open Component Sheet", "View", "".into()),
    ]
}

/// Subsequence fuzzy match (case-insensitive), like the design's palette.
pub fn fuzzy(query: &str, text: &str) -> bool {
    let mut q = query.chars().flat_map(char::to_lowercase).peekable();
    for ch in text.chars().flat_map(char::to_lowercase) {
        if q.peek() == Some(&ch) {
            q.next();
        }
    }
    q.peek().is_none()
}

/// Fuzzy score: lower is better; `None` if no match. Prefers contiguous and early matches.
pub fn fuzzy_score(query: &str, text: &str) -> Option<usize> {
    if query.is_empty() {
        return Some(0);
    }
    let t = text.to_lowercase();
    let q = query.to_lowercase();
    if let Some(pos) = t.find(&q) {
        return Some(pos);
    }
    if !fuzzy(query, text) {
        return None;
    }
    Some(100 + t.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_matching() {
        assert!(fuzzy("rst", "Run Statement at Cursor"));
        assert!(fuzzy("", "anything"));
        assert!(!fuzzy("xyz", "Run Script"));
        assert!(
            fuzzy_score("run", "Run Script").unwrap() < fuzzy_score("rsc", "Run Script").unwrap()
        );
    }

    #[test]
    fn every_command_has_a_label() {
        assert!(palette_commands().iter().all(|c| !c.label.is_empty()));
    }
}
