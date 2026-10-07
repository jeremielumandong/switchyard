//! Settings → Assistant: which coding CLI the assistant uses, each CLI's path, model and
//! extra arguments, and the custom CLI (command template, output mapping, MCP config).

use std::collections::HashMap;

use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, EventEmitter, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use switchyard_core::Command;
use switchyard_core::RuntimeHandle;
use switchyard_core::agent_run::{ASSISTANT_SETTINGS_KEY, AssistantSettings, CliSettings};
use switchyard_core::agents::custom::{JsonlMapping, McpConfigTemplate, OutputFormat};
use switchyard_core::agents::{AgentKind, CustomCli};
use switchyard_core::drivers::{Component, ComponentStatus};

use crate::theme::{MONO, Palette, palette};
use crate::ui::{self, Kind};

/// The CLIs in display order.
pub const AGENTS: [AgentKind; 4] = [
    AgentKind::ClaudeCode,
    AgentKind::Codex,
    AgentKind::Gemini,
    AgentKind::Custom,
];

/// The Driver Manager component for a CLI.
pub fn component_id(kind: AgentKind) -> Option<&'static str> {
    match kind {
        AgentKind::ClaudeCode => Some("claude-code"),
        AgentKind::Codex => Some("codex-cli"),
        AgentKind::Gemini => Some("gemini-cli"),
        AgentKind::Custom => None,
    }
}

/// "Installed 2.1.292", "Not installed", "Too old (0.12; needs 0.160.0)", …
pub fn status_text(status: &ComponentStatus) -> String {
    match status {
        ComponentStatus::Installed { version, .. } => match version {
            Some(v) => format!("Installed {v}"),
            None => "Installed".into(),
        },
        ComponentStatus::TooOld {
            version, required, ..
        } => format!("Too old ({version}; needs {required})"),
        ComponentStatus::TooNew {
            version,
            supported_below,
            ..
        } => format!("Untested version {version} (tested below {supported_below})"),
        ComponentStatus::Missing => "Not installed".into(),
    }
}

/// Split a command line into words (double and single quotes group, backslash escapes).
pub fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let (mut quote, mut any, mut chars) = (None::<char>, false, s.chars());
    while let Some(c) = chars.next() {
        match (quote, c) {
            (None, ' ' | '\t' | '\n') => {
                if any {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            (None, '"' | '\'') => {
                quote = Some(c);
                any = true;
            }
            (Some(q), c) if c == q => quote = None,
            (q, '\\') if q != Some('\'') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                    any = true;
                }
            }
            (_, c) => {
                cur.push(c);
                any = true;
            }
        }
    }
    if any {
        out.push(cur);
    }
    out
}

/// Words back into a command line, quoting where needed.
pub fn join_args(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if !a.is_empty() && !a.chars().any(|c| c.is_whitespace() || "\"'\\".contains(c)) {
                a.clone()
            } else {
                format!("\"{}\"", a.replace('\\', "\\\\").replace('"', "\\\""))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Emitted when the settings are saved.
pub enum AssistantSettingsEvent {
    /// The saved settings.
    Saved(AssistantSettings),
}

struct CliInputs {
    path: Entity<InputState>,
    model: Entity<InputState>,
    extra: Entity<InputState>,
}

/// The settings page.
pub struct AssistantSettingsView {
    core: RuntimeHandle,
    default_agent: AgentKind,
    clis: HashMap<AgentKind, CliInputs>,
    custom_name: Entity<InputState>,
    custom_program: Entity<InputState>,
    custom_args: Entity<InputState>,
    custom_resume: Entity<InputState>,
    custom_interactive: Entity<InputState>,
    prompt_on_stdin: bool,
    jsonl: bool,
    mapping: Vec<(&'static str, &'static str, Entity<InputState>)>,
    mcp_file: Entity<InputState>,
    mcp_template: Entity<TextareaState>,
    statuses: HashMap<AgentKind, String>,
    saved: bool,
}

impl EventEmitter<AssistantSettingsEvent> for AssistantSettingsView {}

fn input(
    window: &mut Window,
    cx: &mut Context<AssistantSettingsView>,
    value: &str,
    placeholder: &str,
) -> Entity<InputState> {
    let (v, ph) = (value.to_owned(), placeholder.to_owned());
    cx.new(|cx| InputState::new(window, cx).placeholder(ph).default_value(v))
}

impl AssistantSettingsView {
    /// A view of `s`.
    pub fn new(
        core: RuntimeHandle,
        s: &AssistantSettings,
        components: &[Component],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut clis = HashMap::new();
        for kind in AGENTS {
            let c = s.cli(kind);
            let program_hint = match kind {
                AgentKind::ClaudeCode => "claude (from PATH)",
                AgentKind::Codex => "codex (from PATH)",
                AgentKind::Gemini => "gemini (from PATH)",
                AgentKind::Custom => "",
            };
            clis.insert(
                kind,
                CliInputs {
                    path: input(window, cx, &c.path, program_hint),
                    model: input(window, cx, &c.model, "CLI default"),
                    extra: input(window, cx, &join_args(&c.extra_args), "none"),
                },
            );
        }
        let m = match &s.custom.output {
            OutputFormat::Jsonl(m) => (**m).clone(),
            OutputFormat::Text => JsonlMapping::default(),
        };
        let mapping_fields: [(&'static str, &'static str, &str); 13] = [
            ("type_field", "Type field", &m.type_field),
            ("text_type", "Text: type", &m.text_type),
            ("text_field", "Text: field", &m.text_field),
            ("tool_type", "Tool call: type", &m.tool_type),
            ("tool_name_field", "Tool call: name", &m.tool_name_field),
            (
                "tool_args_field",
                "Tool call: arguments",
                &m.tool_args_field,
            ),
            ("tool_id_field", "Tool call: id", &m.tool_id_field),
            ("result_type", "Tool result: type", &m.result_type),
            ("result_field", "Tool result: text", &m.result_field),
            ("done_type", "Done: type", &m.done_type),
            ("session_field", "Session id field", &m.session_field),
            ("error_type", "Error: type", &m.error_type),
            ("error_field", "Error: message", &m.error_field),
        ];
        let mapping = mapping_fields
            .into_iter()
            .map(|(k, label, v)| {
                let ph = if k.ends_with("field") {
                    "/json/pointer"
                } else {
                    "event type"
                };
                (k, label, input(window, cx, v, ph))
            })
            .collect();
        let tpl = s.custom.mcp_config.clone().unwrap_or_default();
        let template = tpl.template.clone();
        let mcp_template = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 8)
                .placeholder("{\"mcpServers\":{\"{server}\":{\"command\":{command_json},\"args\":{args_json},\"env\":{env_json}}}}")
                .default_value(template)
        });
        let mut this = Self {
            core,
            default_agent: s.default_agent,
            clis,
            custom_name: input(window, cx, &s.custom.name, "My CLI"),
            custom_program: input(window, cx, &s.custom.program, "mycli"),
            custom_args: input(window, cx, &join_args(&s.custom.args), "--print {prompt}"),
            custom_resume: input(
                window,
                cx,
                &join_args(&s.custom.resume_args),
                "--resume {session}",
            ),
            custom_interactive: input(window, cx, &join_args(&s.custom.interactive_args), "none"),
            prompt_on_stdin: s.custom.prompt_on_stdin,
            jsonl: matches!(s.custom.output, OutputFormat::Jsonl(_)),
            mapping,
            mcp_file: input(window, cx, &tpl.file, "mcp.json"),
            mcp_template,
            statuses: HashMap::new(),
            saved: false,
        };
        this.set_components(components);
        this
    }

    /// Refresh the installed / missing lines from the Driver Manager.
    pub fn set_components(&mut self, components: &[Component]) {
        self.statuses = AGENTS
            .into_iter()
            .filter_map(|k| {
                let id = component_id(k)?;
                let c = components.iter().find(|c| c.id == id)?;
                Some((k, status_text(&c.status)))
            })
            .collect();
    }

    fn value(e: &Entity<InputState>, cx: &Context<Self>) -> String {
        e.read(cx).value().trim().to_owned()
    }

    /// The settings as entered.
    pub fn settings(&self, cx: &Context<Self>) -> AssistantSettings {
        let mut s = AssistantSettings {
            default_agent: self.default_agent,
            ..AssistantSettings::default()
        };
        for kind in AGENTS {
            let i = &self.clis[&kind];
            *s.cli_mut(kind) = CliSettings {
                path: Self::value(&i.path, cx),
                model: Self::value(&i.model, cx),
                extra_args: split_args(&Self::value(&i.extra, cx)),
            };
        }
        let get = |k: &str| {
            self.mapping
                .iter()
                .find(|(key, _, _)| *key == k)
                .map(|(_, _, e)| Self::value(e, cx))
                .unwrap_or_default()
        };
        let file = Self::value(&self.mcp_file, cx);
        let template = self.mcp_template.read(cx).value().to_string();
        s.custom = CustomCli {
            name: Self::value(&self.custom_name, cx),
            program: Self::value(&self.custom_program, cx),
            args: split_args(&Self::value(&self.custom_args, cx)),
            resume_args: split_args(&Self::value(&self.custom_resume, cx)),
            interactive_args: split_args(&Self::value(&self.custom_interactive, cx)),
            prompt_on_stdin: self.prompt_on_stdin,
            output: if self.jsonl {
                OutputFormat::Jsonl(Box::new(JsonlMapping {
                    type_field: get("type_field"),
                    text_type: get("text_type"),
                    text_field: get("text_field"),
                    tool_type: get("tool_type"),
                    tool_name_field: get("tool_name_field"),
                    tool_args_field: get("tool_args_field"),
                    tool_id_field: get("tool_id_field"),
                    result_type: get("result_type"),
                    result_field: get("result_field"),
                    done_type: get("done_type"),
                    session_field: get("session_field"),
                    error_type: get("error_type"),
                    error_field: get("error_field"),
                }))
            } else {
                OutputFormat::Text
            },
            mcp_config: (!file.is_empty() || !template.trim().is_empty())
                .then_some(McpConfigTemplate { file, template }),
        };
        s
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let s = self.settings(cx);
        match serde_json::to_value(&s) {
            Ok(value) => {
                self.core.send(Command::SetSetting {
                    key: ASSISTANT_SETTINGS_KEY.into(),
                    value,
                });
                self.saved = true;
                cx.emit(AssistantSettingsEvent::Saved(s));
            }
            Err(e) => tracing::warn!(error = %e, "assistant settings not saved"),
        }
        cx.notify();
    }
}

fn field(label: &str, e: &Entity<InputState>, width: f32, p: &Palette) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .w(px(width))
        .child(
            div()
                .text_size(px(11.))
                .text_color(p.fg3)
                .child(label.to_owned()),
        )
        .child(
            div()
                .h(px(28.))
                .flex()
                .items_center()
                .px(px(8.))
                .border_1()
                .border_color(p.bd2)
                .rounded(px(6.))
                .bg(p.bg)
                .font_family(MONO)
                .text_size(px(12.))
                .child(Input::new(e).appearance(false).text_size(px(12.))),
        )
}

fn heading(text: &str, p: &Palette) -> impl IntoElement {
    div()
        .text_size(px(12.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(p.fg2)
        .child(text.to_owned())
}

impl Render for AssistantSettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let picker = div()
            .flex()
            .gap(px(6.))
            .children(AGENTS.into_iter().map(|k| {
                let active = k == self.default_agent;
                let status = self.statuses.get(&k).cloned().unwrap_or_else(|| {
                    if k == AgentKind::Custom {
                        "Your command".into()
                    } else {
                        "Checking…".into()
                    }
                });
                div()
                    .id(SharedString::from(format!("asst-default-{}", k.id())))
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .w(px(150.))
                    .px(px(10.))
                    .py(px(7.))
                    .border_1()
                    .border_color(if active { p.acc } else { p.bd2 })
                    .rounded(px(6.))
                    .bg(if active { p.sel } else { p.bg })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.default_agent = k;
                        this.saved = false;
                        cx.notify();
                    }))
                    .child(div().text_size(px(12.5)).child(k.display_name()))
                    .child(div().text_size(px(11.)).text_color(p.fg3).child(status))
            }));
        let cli_rows = [AgentKind::ClaudeCode, AgentKind::Codex, AgentKind::Gemini]
            .into_iter()
            .map(|k| {
                let i = &self.clis[&k];
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(heading(k.display_name(), &p))
                    .child(
                        div()
                            .flex()
                            .gap(px(10.))
                            .child(field("Program", &i.path, 220., &p))
                            .child(field("Model", &i.model, 150., &p))
                            .child(field("Extra arguments", &i.extra, 220., &p)),
                    )
            });
        let custom = &self.clis[&AgentKind::Custom];
        let custom_section = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(heading("Custom CLI", &p))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(p.fg3)
                    .child("Placeholders: {prompt} {model} {session} {workdir} {mcp_config} {system_prompt}; in the MCP template also {command_json} {args_json} {env_json} {env_names_json} {server}."),
            )
            .child(
                div()
                    .flex()
                    .gap(px(10.))
                    .child(field("Name", &self.custom_name, 150., &p))
                    .child(field("Program", &self.custom_program, 200., &p))
                    .child(field("Model", &custom.model, 120., &p))
                    .child(field("Extra arguments", &custom.extra, 140., &p)),
            )
            .child(
                div()
                    .flex()
                    .gap(px(10.))
                    .child(field("Arguments", &self.custom_args, 300., &p))
                    .child(field("When continuing", &self.custom_resume, 150., &p))
                    .child(field("Interactive (terminal)", &self.custom_interactive, 170., &p)),
            )
            .child(
                div()
                    .flex()
                    .gap(px(16.))
                    .child(
                        ui::checkbox("asst-stdin", self.prompt_on_stdin, "Prompt on standard input", &p)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.prompt_on_stdin = !this.prompt_on_stdin;
                                cx.notify();
                            })),
                    )
                    .child(
                        ui::checkbox("asst-jsonl", self.jsonl, "Output is JSON lines (map fields below)", &p)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.jsonl = !this.jsonl;
                                cx.notify();
                            })),
                    ),
            )
            .when(self.jsonl, |d| {
                d.child(
                    div().flex().flex_wrap().gap(px(10.)).children(
                        self.mapping
                            .iter()
                            .map(|(_, label, e)| field(label, e, 150., &p)),
                    ),
                )
            })
            .child(
                div()
                    .flex()
                    .gap(px(10.))
                    .items_start()
                    .child(field("MCP config file", &self.mcp_file, 150., &p))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(4.))
                            .flex_1()
                            .child(div().text_size(px(11.)).text_color(p.fg3).child("MCP config template"))
                            .child(
                                div()
                                    .px(px(8.))
                                    .py(px(5.))
                                    .border_1()
                                    .border_color(p.bd2)
                                    .rounded(px(6.))
                                    .bg(p.bg)
                                    .font_family(MONO)
                                    .text_size(px(12.))
                                    .child(Textarea::new(&self.mcp_template).appearance(false).text_size(px(12.))),
                            ),
                    ),
            );
        div()
            .flex()
            .flex_col()
            .gap(px(16.))
            .p(px(18.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(heading("Default CLI", &p))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(p.fg3)
                            .child("Each CLI uses its own sign-in. A connection can choose another in its settings. Its tools are Switchyard's read-only database tools only."),
                    )
                    .child(picker),
            )
            .children(cli_rows)
            .child(custom_section)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        ui::button("asst-save", "Save", Kind::Primary, &p)
                            .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                    )
                    .when(self.saved, |d| {
                        d.child(div().text_size(px(12.)).text_color(p.dev).child("Saved"))
                    }),
            )
    }
}

/// The page body for the settings overlay.
pub fn element(view: &Entity<AssistantSettingsView>) -> AnyElement {
    view.clone().into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_round_trip() {
        let a = split_args(r#"--print "{prompt}" --flag='a b' x\ y"#);
        assert_eq!(a, ["--print", "{prompt}", "--flag=a b", "x y"]);
        let v = vec![
            "--tools".to_owned(),
            String::new(),
            "a b".into(),
            "q\"".into(),
        ];
        assert_eq!(split_args(&join_args(&v)), v);
        assert!(split_args("   ").is_empty());
    }
}
