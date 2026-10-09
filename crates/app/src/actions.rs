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
        Explain,
        ExplainAnalyze,
        NewTerminal,
        NewConnection,
        NewHost,
        NewQueryTab,
        CloseTab,
        OpenSettings,
        ToggleSidebar,
        ToggleInspector,
        ToggleAssistant,
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
        CopyCells,
        ExtendUp,
        ExtendDown,
        ExtendLeft,
        ExtendRight,
        SplitRight,
        SplitDown,
        Unsplit,
        TreeUp,
        TreeDown,
        TreeExpand,
        TreeCollapse,
        TreeOpen,
        TreeCopy,
        TreeRefresh,
        TreePin,
        PeekTable,
        ClosePeek,
        MenuUp,
        MenuDown,
        MenuOpenSub,
        MenuCloseSub,
        MenuConfirm,
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
        KeyBinding::new("secondary-e", Explain, Some("Workspace")),
        KeyBinding::new("secondary-shift-e", ExplainAnalyze, Some("Workspace")),
        KeyBinding::new("secondary-e", Explain, Some("SqlTab > Input")),
        KeyBinding::new("secondary-shift-e", ExplainAnalyze, Some("SqlTab > Input")),
        KeyBinding::new("secondary-.", StopQuery, None),
        KeyBinding::new(new_terminal, NewTerminal, None),
        KeyBinding::new("secondary-n", NewConnection, None),
        KeyBinding::new("secondary-alt-n", NewQueryTab, None),
        KeyBinding::new("secondary-w", CloseTab, Some("Workspace")),
        KeyBinding::new("secondary-,", OpenSettings, None),
        KeyBinding::new("secondary-b", ToggleSidebar, None),
        KeyBinding::new("secondary-j", ToggleAssistant, None),
        KeyBinding::new("secondary-shift-f", FormatSql, Some("Workspace")),
        KeyBinding::new("secondary-shift-h", ShowHistory, None),
        KeyBinding::new("secondary-\\", SplitRight, None),
        KeyBinding::new("secondary-c", CopyCells, Some("DataTable")),
        // Shift+arrows extend the selected range in the results grid.
        KeyBinding::new("shift-up", ExtendUp, Some("DataTable")),
        KeyBinding::new("shift-down", ExtendDown, Some("DataTable")),
        KeyBinding::new("shift-left", ExtendLeft, Some("DataTable")),
        KeyBinding::new("shift-right", ExtendRight, Some("DataTable")),
        KeyBinding::new("secondary-shift-\\", SplitDown, None),
        // Linux reports Shift+\ as `|`.
        KeyBinding::new("secondary-|", SplitDown, None),
        KeyBinding::new("escape", Dismiss, Some("Overlay")),
        // Schema explorer tree (focused after a click on a row).
        KeyBinding::new("up", TreeUp, Some("SchemaTree")),
        KeyBinding::new("down", TreeDown, Some("SchemaTree")),
        KeyBinding::new("right", TreeExpand, Some("SchemaTree")),
        KeyBinding::new("left", TreeCollapse, Some("SchemaTree")),
        KeyBinding::new("enter", TreeOpen, Some("SchemaTree")),
        KeyBinding::new("secondary-c", TreeCopy, Some("SchemaTree")),
        KeyBinding::new("f5", TreeRefresh, Some("SchemaTree")),
        // Pin / unpin the selected object or schema in Favorites (DBX-5e).
        KeyBinding::new("secondary-d", TreePin, Some("SchemaTree")),
        // Context menu (focused while open): arrows move and open submenus.
        KeyBinding::new("up", MenuUp, Some("CtxMenu")),
        KeyBinding::new("down", MenuDown, Some("CtxMenu")),
        KeyBinding::new("right", MenuOpenSub, Some("CtxMenu")),
        KeyBinding::new("left", MenuCloseSub, Some("CtxMenu")),
        KeyBinding::new("enter", MenuConfirm, Some("CtxMenu")),
        KeyBinding::new("escape", Dismiss, Some("CtxMenu")),
        // Peek table: the columns of the table under the cursor; Escape closes it (the
        // binding only exists while the popover is open, so the editor keeps Escape).
        KeyBinding::new("f12", PeekTable, Some("SqlTab > Input")),
        KeyBinding::new("escape", ClosePeek, Some("Peek > Input")),
        KeyBinding::new("escape", ClosePeek, Some("Peek")),
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
    Explain,
    ExplainAnalyze,
    FormatSql,
    CommitTransaction,
    RollbackTransaction,
    OpenFiles,
    Settings,
    SettingsDrivers,
    ToggleTheme,
    ToggleInspector,
    ToggleAssistant,
    OptimizeQuery,
    PlanQuery,
    SettingsAssistant,
    ToggleSidebar,
    ShowWelcome,
    OpenComponents,
    RefreshSchema,
    ImportSshConfig,
    ExportProfiles,
    ShowHistory,
    ShowWorkload,
    /// Open the activity monitor (DBX-5b).
    ShowActivity,
    /// Open the snippet manager (DBX-4b).
    ManageSnippets,
    /// ER diagram of the schema under the tree cursor (DBX-5d).
    ErDiagram,
    SplitRight,
    SplitDown,
    Unsplit,
    /// Show the Default workspace (databases, terminals, files).
    SwitchToDefault,
    /// Show the API workspace.
    SwitchToApi,
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
        c(
            Explain,
            "Explain (Estimated Plan)",
            "Editor",
            k("⌘E", "Ctrl+E"),
        ),
        c(
            ExplainAnalyze,
            "Explain Analyze (Actual Plan)",
            "Editor",
            k("⇧⌘E", "Ctrl+Shift+E"),
        ),
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
        c(
            ShowWorkload,
            "Workload: Query and Index Statistics",
            "Editor",
            "".into(),
        ),
        c(ShowActivity, "Activity Monitor", "Editor", "".into()),
        c(ManageSnippets, "Manage Snippets…", "Editor", "".into()),
        c(RefreshSchema, "Refresh Schema", "Schema", "".into()),
        c(
            ErDiagram,
            "ER Diagram for Current Schema",
            "Schema",
            "".into(),
        ),
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
        c(
            ToggleAssistant,
            "Toggle Assistant",
            "Assistant",
            k("⌘J", "Ctrl+J"),
        ),
        c(
            OptimizeQuery,
            "Optimize Statement at Cursor",
            "Assistant",
            "".into(),
        ),
        c(PlanQuery, "Plan a Query…", "Assistant", "".into()),
        c(
            SettingsAssistant,
            "Assistant Settings…",
            "Assistant",
            "".into(),
        ),
        c(ToggleSidebar, "Toggle Sidebar", "View", k("⌘B", "Ctrl+B")),
        c(
            SwitchToDefault,
            "Switch to Default Workspace",
            "Workspace",
            "".into(),
        ),
        c(
            SwitchToApi,
            "Switch to API Workspace",
            "Workspace",
            "".into(),
        ),
        c(ShowWelcome, "Show Welcome", "View", "".into()),
        c(OpenComponents, "Open Component Sheet", "View", "".into()),
    ]
}

/// Subsequence fuzzy match (case-insensitive), like the design's palette.
pub fn fuzzy(query: &str, text: &str) -> bool {
    let mut q = query.chars().flat_map(char::to_lowercase).peekable();
    for ch in text.chars().flat_map(char::to_lowercase) {
        match q.peek() {
            None => return true,
            Some(c) if *c == ch => {
                q.next();
            }
            Some(_) => {}
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

    #[test]
    fn commands_are_listed_once() {
        let cmds = palette_commands();
        for (i, a) in cmds.iter().enumerate() {
            assert!(
                cmds[i + 1..]
                    .iter()
                    .all(|b| b.id != a.id && b.label != a.label),
                "{:?} is listed twice",
                a.id
            );
        }
    }

    #[test]
    fn workspace_switch_commands_are_listed() {
        let cmds = palette_commands();
        for id in [CommandId::SwitchToDefault, CommandId::SwitchToApi] {
            assert!(cmds.iter().any(|c| c.id == id), "{id:?} missing");
        }
    }

    /// Every command bound in `init` shows its shortcut in the palette. Keep this list in
    /// step with the bindings.
    #[test]
    fn bound_commands_show_their_shortcut() {
        use CommandId::*;
        let bound = [
            NewConnection,
            NewTerminal,
            NewQueryTab,
            RunStatement,
            RunScript,
            StopQuery,
            Explain,
            ExplainAnalyze,
            FormatSql,
            ShowHistory,
            Settings,
            ToggleAssistant,
            ToggleSidebar,
            SplitRight,
            SplitDown,
        ];
        let cmds = palette_commands();
        for id in bound {
            let cmd = cmds.iter().find(|c| c.id == id);
            assert!(
                cmd.is_some_and(|c| !c.key.is_empty()),
                "{id:?} has a binding but no shortcut hint"
            );
        }
        for c in cmds.iter().filter(|c| !c.key.is_empty()) {
            assert!(
                bound.contains(&c.id),
                "{:?} shows a shortcut it lacks",
                c.id
            );
        }
    }
}
