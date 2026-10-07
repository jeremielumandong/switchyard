//! The assistant panel: ask a coding CLI to optimize the current statement (from the editor
//! or the plan view) or to plan a new query, with its tool calls and answer streaming in.
//! SQL in the answer becomes suggestion cards: open in an editor, or compare its plan.
//! The panel only consumes normalized `AgentEvent`s; nothing here knows which CLI ran.

use std::sync::atomic::{AtomicU64, Ordering};

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, Context, Entity, EventEmitter, FontWeight, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, px,
};
use switchyard_core::agents::{AgentEvent, AgentKind, RunSummary};
use switchyard_core::store::DbConnection;
use switchyard_core::{Command, RuntimeHandle};

use crate::theme::{MONO, palette};
use crate::ui::{self, Kind};

static NEXT_RUN: AtomicU64 = AtomicU64::new(1);

/// What kind of SQL a suggestion is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuggestionKind {
    /// `CREATE INDEX`: compared with hypothetical indexes (HypoPG).
    Index,
    /// Statistics (`ANALYZE`, `CREATE STATISTICS`, `UPDATE STATISTICS`): applied by you,
    /// then the statement is planned again and compared.
    Statistics,
    /// A rewritten statement: its plan is compared with the original's.
    Rewrite,
    /// Some other statement (DDL, settings): opened in an editor only.
    Other,
}

impl SuggestionKind {
    fn label(self) -> &'static str {
        match self {
            SuggestionKind::Index => "Index",
            SuggestionKind::Statistics => "Statistics",
            SuggestionKind::Rewrite => "Rewrite",
            SuggestionKind::Other => "SQL",
        }
    }
}

/// One fenced SQL block from an answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suggestion {
    /// Its kind.
    pub kind: SuggestionKind,
    /// The SQL, trimmed.
    pub sql: String,
}

fn classify(sql: &str) -> SuggestionKind {
    let first = sql
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("--"))
        .unwrap_or_default()
        .to_ascii_uppercase();
    let words: Vec<&str> = first.split_whitespace().collect();
    match words.as_slice() {
        ["CREATE", "INDEX", ..]
        | ["CREATE", "UNIQUE", "INDEX", ..]
        | ["CREATE", "NONCLUSTERED", "INDEX", ..]
        | ["CREATE", "CLUSTERED", "INDEX", ..] => SuggestionKind::Index,
        ["ANALYZE", ..] | ["VACUUM", "ANALYZE", ..] | ["UPDATE", "STATISTICS", ..] => {
            SuggestionKind::Statistics
        }
        ["CREATE", "STATISTICS", ..] | ["ALTER", "TABLE", .., "STATISTICS", _] => {
            SuggestionKind::Statistics
        }
        ["SELECT", ..] | ["WITH", ..] | ["UPDATE", ..] | ["DELETE", ..] | ["INSERT", ..] => {
            SuggestionKind::Rewrite
        }
        _ if first.contains("SET STATISTICS") => SuggestionKind::Statistics,
        _ => SuggestionKind::Other,
    }
}

/// The ```sql blocks of `text` (also unlabelled blocks that look like SQL).
pub fn suggestions(text: &str) -> Vec<Suggestion> {
    let mut out = Vec::new();
    let mut lines = text.lines();
    while let Some(l) = lines.next() {
        let t = l.trim_start();
        let Some(lang) = t.strip_prefix("```") else {
            continue;
        };
        let lang = lang.trim().to_ascii_lowercase();
        let mut body = Vec::new();
        for l in lines.by_ref() {
            if l.trim_start().starts_with("```") {
                break;
            }
            body.push(l);
        }
        let sql = body.join("\n").trim().to_owned();
        if sql.is_empty() {
            continue;
        }
        let kind = classify(&sql);
        let is_sql = matches!(
            lang.as_str(),
            "sql" | "postgresql" | "postgres" | "tsql" | "plpgsql"
        ) || (lang.is_empty() && kind != SuggestionKind::Other);
        if is_sql && !out.iter().any(|s: &Suggestion| s.sql == sql) {
            out.push(Suggestion { kind, sql });
        }
    }
    out
}

/// A run of an answer: prose, or the body of a fenced code block (fences dropped).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Segment {
    /// Text outside code fences, trimmed.
    Prose(String),
    /// The lines between a pair of fences.
    Code(String),
}

/// Splits `text` at its code fences; an unclosed fence runs to the end (still streaming).
pub fn segments(text: &str) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut buf: Vec<&str> = Vec::new();
    let mut in_code = false;
    let flush = |buf: &mut Vec<&str>, code: bool, out: &mut Vec<Segment>| {
        let joined = buf.join("\n");
        let body = if code {
            joined.trim_end()
        } else {
            joined.trim()
        };
        if !body.is_empty() {
            out.push(if code {
                Segment::Code(body.to_owned())
            } else {
                Segment::Prose(body.to_owned())
            });
        }
        buf.clear();
    };
    for l in text.lines() {
        if l.trim_start().starts_with("```") {
            flush(&mut buf, in_code, &mut out);
            in_code = !in_code;
        } else {
            buf.push(l);
        }
    }
    flush(&mut buf, in_code, &mut out);
    out
}

/// What the panel is doing for its connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Make the current statement faster.
    Optimize,
    /// Write a new query from a description.
    PlanQuery,
}

/// One entry of a turn's transcript.
#[derive(Clone, Debug)]
enum Item {
    Text(String),
    Thinking(String),
    Tool {
        id: String,
        name: String,
        args: String,
        result: Option<(String, bool)>,
    },
    Error(String),
    Note(String),
}

struct Turn {
    asked: String,
    items: Vec<Item>,
    done: Option<RunSummary>,
    suggestions: Vec<Suggestion>,
    expanded_tools: bool,
}

/// What the panel asks the workspace to do.
pub enum AssistantPanelEvent {
    /// Open `sql` in a new editor tab on the panel's connection.
    OpenInEditor(String),
    /// Plan `sql` and compare it with the plan of `base` (the statement being optimized).
    ComparePlan {
        /// The statement being optimized.
        base: Option<String>,
        /// The rewrite.
        sql: String,
    },
    /// Plan `base` with hypothetical `indexes` and compare.
    WhatIf {
        /// The statement being optimized.
        base: Option<String>,
        /// `CREATE INDEX` statements.
        indexes: Vec<String>,
    },
    /// Plan `base` again and compare with its previous plan (after statistics changed).
    Replan {
        /// The statement being optimized.
        base: Option<String>,
    },
    /// Start the CLI interactively in a terminal tab.
    OpenTerminal(Option<AgentKind>),
    /// Hide the panel.
    Close,
}

/// The assistant panel.
pub struct AssistantPanel {
    core: RuntimeHandle,
    connection: Option<DbConnection>,
    /// The CLI chosen in the panel; `None`: the connection's or the default.
    choice: Option<AgentKind>,
    /// The CLI the last run used.
    used: Option<AgentKind>,
    run: Option<u64>,
    /// Conversation to continue with follow-ups.
    session: Option<String>,
    /// The statement being optimized.
    base_sql: Option<String>,
    mode: Mode,
    turns: Vec<Turn>,
    input: Entity<InputState>,
    scroll: ScrollHandle,
    /// Settings → Assistant (for the CLI's name).
    settings: switchyard_core::agent_run::AssistantSettings,
    _subs: Vec<Subscription>,
}

impl EventEmitter<AssistantPanelEvent> for AssistantPanel {}

const SQL_RULES: &str = "Use Switchyard's tools to look at the tables and plans before you answer. \
Give every suggested index, statistics change or rewritten query as SQL in its own ```sql block \
(one statement per block), each with a short reason.";

impl AssistantPanel {
    /// An empty panel.
    pub fn new(core: RuntimeHandle, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Ask a follow-up, or describe a query to write")
        });
        let sub = cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, window, cx| {
            if matches!(ev, InputEvent::PressEnter { .. }) {
                this.submit(window, cx);
            }
        });
        Self {
            core,
            connection: None,
            choice: None,
            used: None,
            run: None,
            session: None,
            base_sql: None,
            mode: Mode::Optimize,
            turns: Vec::new(),
            input,
            scroll: ScrollHandle::new(),
            settings: Default::default(),
            _subs: vec![sub],
        }
    }

    /// The saved Settings → Assistant.
    pub fn set_settings(
        &mut self,
        settings: switchyard_core::agent_run::AssistantSettings,
        cx: &mut Context<Self>,
    ) {
        self.settings = settings;
        cx.notify();
    }

    /// The connection questions are about (the active editor's).
    pub fn set_connection(&mut self, connection: Option<DbConnection>, cx: &mut Context<Self>) {
        let changed = self.connection.as_ref().map(|c| &c.id) != connection.as_ref().map(|c| &c.id);
        self.connection = connection;
        if changed {
            // Another database: another conversation.
            self.session = None;
            self.base_sql = None;
        }
        cx.notify();
    }

    /// The CLI the next run will use, for the header.
    fn agent_label(&self, settings: &switchyard_core::agent_run::AssistantSettings) -> String {
        let kind = self
            .choice
            .unwrap_or_else(|| settings.agent_for(self.connection.as_ref()));
        kind.display_name().to_owned()
    }

    fn start(&mut self, asked: String, prompt: String, resume: bool, cx: &mut Context<Self>) {
        if let Some(run) = self.run.take() {
            self.core.send(Command::CancelAgent { run });
        }
        let run = NEXT_RUN.fetch_add(1, Ordering::Relaxed);
        self.run = Some(run);
        self.turns.push(Turn {
            asked,
            items: Vec::new(),
            done: None,
            suggestions: Vec::new(),
            expanded_tools: false,
        });
        self.core.send(Command::RunAgent {
            run,
            agent: self.choice,
            connection: self.connection.as_ref().map(|c| c.id.clone()),
            prompt,
            resume: if resume { self.session.clone() } else { None },
        });
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    /// Optimize `sql` (from the editor, or the plan view with its findings).
    pub fn optimize(&mut self, sql: String, findings: Vec<String>, cx: &mut Context<Self>) {
        let Some(conn) = self.connection.clone() else {
            return;
        };
        self.mode = Mode::Optimize;
        self.session = None;
        self.base_sql = Some(sql.clone());
        let mut prompt = format!(
            "Optimize this {} statement on the connection named \"{}\". Explain its plan \
             (explain tool), look at the tables it uses, and suggest what would make it faster.\n\n\
             ```sql\n{sql}\n```\n",
            conn.engine.display_name(),
            conn.name
        );
        if !findings.is_empty() {
            prompt.push_str("\nSwitchyard's plan analysis found:\n");
            for f in &findings {
                prompt.push_str(&format!("- {f}\n"));
            }
        }
        prompt.push('\n');
        prompt.push_str(SQL_RULES);
        let first_line = sql.lines().next().unwrap_or_default();
        let asked = format!("Optimize: {}", ellipsis(first_line, 70));
        self.start(asked, prompt, false, cx);
    }

    /// Switch to planning a new query; the next message describes it.
    pub fn plan_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.mode = Mode::PlanQuery;
        self.session = None;
        self.base_sql = None;
        self.input.update(cx, |i, cx| i.focus(window, cx));
        cx.notify();
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_owned();
        if text.is_empty() || self.connection.is_none() {
            return;
        }
        self.input.update(cx, |i, cx| i.set_value("", window, cx));
        let conn = self.connection.clone();
        let continuing = self.session.is_some();
        let prompt = match (self.mode, continuing, conn) {
            (_, true, _) => text.clone(),
            (Mode::PlanQuery, false, Some(c)) => format!(
                "Write a {} query on the connection named \"{}\" that does this: {text}\n\n\
                 Look at the schema first (list_tables, describe_table), check the plan with the \
                 explain tool, and give the final query in a ```sql block. {SQL_RULES}",
                c.engine.display_name(),
                c.name
            ),
            (Mode::Optimize, false, Some(c)) => format!(
                "On the connection named \"{}\": {text}\n\n{SQL_RULES}",
                c.name
            ),
            (_, _, None) => return,
        };
        self.start(text, prompt, continuing, cx);
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(run) = self.run {
            self.core.send(Command::CancelAgent { run });
        }
        cx.notify();
    }

    fn reset(&mut self, cx: &mut Context<Self>) {
        self.stop(cx);
        self.run = None;
        self.turns.clear();
        self.session = None;
        self.base_sql = None;
        cx.notify();
    }

    /// An event of one of this panel's runs; others are ignored.
    pub fn on_agent_event(
        &mut self,
        run: u64,
        agent: AgentKind,
        event: AgentEvent,
        cx: &mut Context<Self>,
    ) {
        if self.run != Some(run) {
            return;
        }
        self.used = Some(agent);
        let Some(turn) = self.turns.last_mut() else {
            return;
        };
        match event {
            AgentEvent::Started { session_id, .. } => {
                if session_id.is_some() {
                    self.session = session_id;
                }
            }
            AgentEvent::Text(t) => match turn.items.last_mut() {
                Some(Item::Text(s)) => s.push_str(&t),
                _ => turn.items.push(Item::Text(t)),
            },
            AgentEvent::Thinking(t) => match turn.items.last_mut() {
                Some(Item::Thinking(s)) => s.push_str(&t),
                _ => turn.items.push(Item::Thinking(t)),
            },
            AgentEvent::ToolCall {
                id,
                name,
                arguments,
            } => turn.items.push(Item::Tool {
                id,
                name,
                args: arguments.to_string(),
                result: None,
            }),
            AgentEvent::ToolResult { id, text, is_error } => {
                if let Some(Item::Tool { result, .. }) = turn
                    .items
                    .iter_mut()
                    .rev()
                    .find(|i| matches!(i, Item::Tool { id: t, .. } if *t == id))
                {
                    *result = Some((text, is_error));
                }
            }
            AgentEvent::Done(summary) => {
                if summary.session_id.is_some() {
                    self.session.clone_from(&summary.session_id);
                }
                // The answer is the streamed text; CLIs that only report it at the end
                // (summary text, nothing streamed) show it here.
                let streamed: String = turn
                    .items
                    .iter()
                    .filter_map(|i| match i {
                        Item::Text(t) => Some(t.as_str()),
                        _ => None,
                    })
                    .collect();
                if streamed.trim().is_empty() && !summary.text.is_empty() {
                    turn.items.push(Item::Text(summary.text.clone()));
                }
                let answer = if streamed.trim().is_empty() {
                    summary.text.clone()
                } else {
                    streamed
                };
                turn.suggestions = suggestions(&answer);
                turn.done = Some(summary);
            }
            AgentEvent::Error(e) => turn.items.push(Item::Error(e)),
            AgentEvent::Log(_) => {}
            AgentEvent::Exited(_) => {
                self.run = None;
                if turn.done.is_none() && !turn.items.iter().any(|i| matches!(i, Item::Error(_))) {
                    turn.items
                        .push(Item::Note("The run ended without an answer.".into()));
                }
            }
        }
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    fn card_action(&mut self, s: &Suggestion, compare: bool, cx: &mut Context<Self>) {
        if !compare {
            cx.emit(AssistantPanelEvent::OpenInEditor(s.sql.clone()));
            return;
        }
        let base = self.base_sql.clone();
        cx.emit(match s.kind {
            SuggestionKind::Index => AssistantPanelEvent::WhatIf {
                base,
                indexes: s
                    .sql
                    .split(';')
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(str::to_owned)
                    .collect(),
            },
            SuggestionKind::Statistics => AssistantPanelEvent::Replan { base },
            SuggestionKind::Rewrite | SuggestionKind::Other => AssistantPanelEvent::ComparePlan {
                base,
                sql: s.sql.clone(),
            },
        });
    }
}

fn ellipsis(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_owned()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

impl Render for AssistantPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let agent = self.agent_label(&self.settings);
        let running = self.run.is_some();
        let conn_name = self
            .connection
            .as_ref()
            .map(|c| c.name.clone())
            .unwrap_or_else(|| "No connection".into());
        let header = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(12.))
            .h(px(40.))
            .border_b_1()
            .border_color(p.bd)
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(px(13.))
                    .child("Assistant"),
            )
            .child(
                div()
                    .text_size(px(11.5))
                    .text_color(p.fg3)
                    .truncate()
                    .child(conn_name),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id("asst-agent")
                    .px(px(7.))
                    .py(px(2.))
                    .rounded(px(5.))
                    .border_1()
                    .border_color(p.bd2)
                    .text_size(px(11.5))
                    .hover(|s| s.bg(p.hover))
                    .tooltip(|w, cx| {
                        gpui_kit::component::tooltip::Tooltip::new(
                            "Coding CLI for this panel (click to change)",
                        )
                        .build(w, cx)
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        // Default → Claude Code → Codex → Gemini → Custom → Default.
                        let order = crate::assistant_settings::AGENTS;
                        this.choice = match this.choice {
                            None => Some(order[0]),
                            Some(k) => order
                                .iter()
                                .position(|o| *o == k)
                                .and_then(|i| order.get(i + 1))
                                .copied(),
                        };
                        this.session = None;
                        cx.notify();
                    }))
                    .child(match self.choice {
                        Some(_) => agent.clone(),
                        None => format!("{agent} (default)"),
                    }),
            )
            .child(
                div()
                    .id("asst-terminal")
                    .px(px(6.))
                    .text_size(px(11.5))
                    .text_color(p.fg2)
                    .hover(|s| s.text_color(p.fg))
                    .tooltip(|w, cx| {
                        gpui_kit::component::tooltip::Tooltip::new(
                            "Open this CLI in a terminal with Switchyard's tools",
                        )
                        .build(w, cx)
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        cx.emit(AssistantPanelEvent::OpenTerminal(this.choice));
                    }))
                    .child("Terminal"),
            )
            .child(
                div()
                    .id("asst-close")
                    .px(px(4.))
                    .text_color(p.fg3)
                    .hover(|s| s.text_color(p.fg))
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(AssistantPanelEvent::Close)))
                    .child("×"),
            );
        let modes = div()
            .flex_none()
            .flex()
            .gap(px(6.))
            .px(px(12.))
            .py(px(8.))
            .child(
                ui::button(
                    "asst-plan",
                    "Plan a query",
                    if self.mode == Mode::PlanQuery {
                        Kind::Primary
                    } else {
                        Kind::Secondary
                    },
                    &p,
                )
                .on_click(cx.listener(|this, _, w, cx| this.plan_query(w, cx))),
            )
            .child(
                ui::button("asst-new", "New conversation", Kind::Ghost, &p)
                    .on_click(cx.listener(|this, _, _, cx| this.reset(cx))),
            )
            .when(running, |d| {
                d.child(div().flex_1()).child(
                    ui::button("asst-stop", "Stop", Kind::Secondary, &p)
                        .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
                )
            });

        let empty = self.turns.is_empty();
        let mut transcript = div()
            .id("asst-transcript")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .px(px(12.))
            .pb(px(12.))
            .flex()
            .flex_col()
            .gap(px(12.));
        if empty {
            transcript = transcript.child(
                div()
                    .text_size(px(12.))
                    .text_color(p.fg2)
                    .child(match self.mode {
                        Mode::Optimize => "Click Optimize in the editor or the plan view, or ask about this connection below. The assistant uses your coding CLI with Switchyard's read-only tools; it cannot change the database.",
                        Mode::PlanQuery => "Describe the query you need below; the assistant looks at the schema and plans it.",
                    }),
            );
        }
        let turns = self.turns.len();
        for (ti, turn) in self.turns.iter().enumerate() {
            let last = ti + 1 == turns;
            let mut t = div().flex().flex_col().gap(px(8.)).child(
                div()
                    .px(px(9.))
                    .py(px(6.))
                    .rounded(px(6.))
                    .bg(p.sel)
                    .text_size(px(12.))
                    .child(turn.asked.clone()),
            );
            let tools: Vec<&Item> = turn
                .items
                .iter()
                .filter(|i| matches!(i, Item::Tool { .. }))
                .collect();
            if !tools.is_empty() {
                let expanded = turn.expanded_tools;
                let mut list = div().flex().flex_col().gap(px(3.)).child(
                    div()
                        .id(SharedString::from(format!("asst-tools-{ti}")))
                        .text_size(px(11.))
                        .text_color(p.fg3)
                        .hover(|s| s.text_color(p.fg2))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(t) = this.turns.get_mut(ti) {
                                t.expanded_tools = !t.expanded_tools;
                            }
                            cx.notify();
                        }))
                        .child(format!(
                            "{} {} tool call{}",
                            if expanded { "▾" } else { "▸" },
                            tools.len(),
                            if tools.len() == 1 { "" } else { "s" }
                        )),
                );
                for item in tools {
                    let Item::Tool {
                        name, args, result, ..
                    } = item
                    else {
                        continue;
                    };
                    let (mark, color) = match result {
                        None => ("…", p.fg3),
                        Some((_, false)) => ("✓", p.dev),
                        Some((_, true)) => ("✕", p.prod),
                    };
                    let mut row = div()
                        .flex()
                        .gap(px(6.))
                        .text_size(px(11.5))
                        .font_family(MONO)
                        .child(div().text_color(color).child(mark))
                        .child(div().text_color(p.fg2).child(name.clone()))
                        .child(
                            div()
                                .text_color(p.fg3)
                                .truncate()
                                .min_w_0()
                                .child(ellipsis(args, 80)),
                        );
                    if expanded && let Some((text, _)) = result {
                        row = row.flex_wrap().child(
                            div()
                                .w_full()
                                .pl(px(14.))
                                .text_color(p.fg3)
                                .whitespace_normal()
                                .child(ellipsis(text, 1200)),
                        );
                    }
                    list = list.child(row);
                }
                t = t.child(list);
            }
            for item in &turn.items {
                match item {
                    Item::Text(s) => {
                        for seg in segments(s) {
                            t = t.child(match seg {
                                Segment::Prose(s) => {
                                    div().text_size(px(12.5)).whitespace_normal().child(s)
                                }
                                Segment::Code(s) => div()
                                    .px(px(8.))
                                    .py(px(6.))
                                    .rounded(px(6.))
                                    .bg(p.surface)
                                    .font_family(MONO)
                                    .text_size(px(11.5))
                                    .whitespace_normal()
                                    .child(s),
                            });
                        }
                    }
                    Item::Thinking(s) if last && turn.done.is_none() => {
                        t = t.child(
                            div()
                                .text_size(px(11.5))
                                .text_color(p.fg3)
                                .italic()
                                .child(ellipsis(s, 300)),
                        );
                    }
                    Item::Error(e) => {
                        t = t.child(
                            div()
                                .px(px(9.))
                                .py(px(6.))
                                .rounded(px(6.))
                                .bg(p.prod_bg)
                                .text_size(px(12.))
                                .child(e.clone()),
                        );
                    }
                    Item::Note(n) => {
                        t = t.child(div().text_size(px(11.5)).text_color(p.fg3).child(n.clone()));
                    }
                    _ => {}
                }
            }
            for (si, s) in turn.suggestions.iter().enumerate() {
                let s2 = s.clone();
                let s3 = s.clone();
                let compare_label = match s.kind {
                    SuggestionKind::Index => "Compare plan (hypothetical)",
                    SuggestionKind::Statistics => "Re-plan & compare",
                    SuggestionKind::Rewrite if self.base_sql.is_some() => "Compare plan",
                    _ => "Show plan",
                };
                t = t.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(6.))
                        .p(px(8.))
                        .rounded(px(6.))
                        .border_1()
                        .border_color(p.bd2)
                        .child(
                            div()
                                .text_size(px(10.5))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(p.fg3)
                                .child(s.kind.label().to_uppercase()),
                        )
                        .child(
                            div()
                                .font_family(MONO)
                                .text_size(px(11.5))
                                .whitespace_normal()
                                .child(s.sql.clone()),
                        )
                        .child(
                            div()
                                .flex()
                                .gap(px(6.))
                                .child(
                                    ui::button(
                                        SharedString::from(format!("asst-cmp-{ti}-{si}")),
                                        compare_label,
                                        Kind::Secondary,
                                        &p,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| this.card_action(&s2, true, cx),
                                    )),
                                )
                                .child(
                                    ui::button(
                                        SharedString::from(format!("asst-open-{ti}-{si}")),
                                        "Open in editor",
                                        Kind::Ghost,
                                        &p,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| this.card_action(&s3, false, cx),
                                    )),
                                ),
                        ),
                );
            }
            if last && running {
                t = t.child(
                    div()
                        .text_size(px(11.5))
                        .text_color(p.acc)
                        .child("Working…"),
                );
            }
            transcript = transcript.child(t);
        }
        let can_ask = self.connection.is_some();
        let footer = div()
            .flex_none()
            .flex()
            .gap(px(6.))
            .items_center()
            .p(px(10.))
            .border_t_1()
            .border_color(p.bd)
            .child(
                div()
                    .flex_1()
                    .h(px(30.))
                    .flex()
                    .items_center()
                    .px(px(8.))
                    .border_1()
                    .border_color(p.bd2)
                    .rounded(px(6.))
                    .bg(p.bg)
                    .text_size(px(12.5))
                    .child(
                        Input::new(&self.input)
                            .appearance(false)
                            .text_size(px(12.5)),
                    ),
            )
            .child(
                ui::button("asst-send", "Send", Kind::Primary, &p)
                    .when(!can_ask, |b| b.opacity(0.5))
                    .on_click(cx.listener(|this, _, w, cx| this.submit(w, cx))),
            );
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(p.panel)
            .child(header)
            .child(modes)
            .child(transcript)
            .child(footer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_prose_and_code() {
        let s = segments(
            "Add an index:\n\n```sql\nCREATE INDEX i ON t (a);\n```\nThen\n```\nANALYZE t;",
        );
        assert_eq!(
            s,
            [
                Segment::Prose("Add an index:".into()),
                Segment::Code("CREATE INDEX i ON t (a);".into()),
                Segment::Prose("Then".into()),
                Segment::Code("ANALYZE t;".into()),
            ]
        );
    }

    #[test]
    fn finds_and_classifies_sql_blocks() {
        let answer = "The scan is the problem.\n\n```sql\nCREATE INDEX CONCURRENTLY orders_customer_idx ON orders (customer_id);\n```\n\
            Refresh the estimates:\n```sql\nANALYZE orders;\n```\nOr rewrite it:\n```SQL\nSELECT id FROM orders\nWHERE customer_id = 42;\n```\n\
            ```bash\nrm -rf /\n```\n```\nCREATE STATISTICS s ON a, b FROM t;\n```\n```sql\nANALYZE orders;\n```\n```sql\nSET work_mem = '64MB';\n```";
        let s = suggestions(answer);
        assert_eq!(
            s.iter().map(|x| x.kind).collect::<Vec<_>>(),
            [
                SuggestionKind::Index,
                SuggestionKind::Statistics,
                SuggestionKind::Rewrite,
                SuggestionKind::Statistics,
                SuggestionKind::Other
            ]
        );
        assert_eq!(s[2].sql, "SELECT id FROM orders\nWHERE customer_id = 42;");
        assert!(
            !s.iter().any(|x| x.sql.contains("rm -rf")),
            "only SQL blocks"
        );
    }

    #[test]
    fn sql_server_forms() {
        for (sql, kind) in [
            (
                "CREATE NONCLUSTERED INDEX ix ON dbo.t (a) INCLUDE (b);",
                SuggestionKind::Index,
            ),
            (
                "UPDATE STATISTICS dbo.orders WITH FULLSCAN;",
                SuggestionKind::Statistics,
            ),
            (
                "WITH x AS (SELECT 1) SELECT * FROM x;",
                SuggestionKind::Rewrite,
            ),
            (
                "-- faster\nSELECT TOP 10 * FROM t;",
                SuggestionKind::Rewrite,
            ),
            (
                "ALTER TABLE t ALTER COLUMN a SET STATISTICS 1000;",
                SuggestionKind::Statistics,
            ),
        ] {
            assert_eq!(classify(sql), kind, "{sql}");
        }
    }
}
