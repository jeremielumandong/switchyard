//! Coding CLIs run in a project folder ("Code here" in the Files tab, the palette's
//! "Run … in folder"): a terminal tab starts the user's shell in the folder and types the
//! CLI's command, so the CLI finds the shell's PATH and the shell stays when it exits.
//! These runs are the user's own sessions in their own project: no Switchyard tools, no
//! session token (see `docs/DECISIONS.md`); "Open in terminal" from the assistant is the
//! run with Switchyard's tools.

use switchyard_core::agent_run::AssistantSettings;
use switchyard_core::drivers::{Component, ComponentStatus};

/// A coding CLI the launcher offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodingCli {
    /// Stable id.
    pub id: &'static str,
    /// Display name.
    pub name: &'static str,
    /// Program looked up on the shell's PATH.
    pub program: &'static str,
    /// Its Driver Manager component, when it has one (install status).
    pub component: Option<&'static str>,
}

/// The CLIs in menu order (the custom one from Settings → Assistant comes last).
pub const CLIS: [CodingCli; 5] = [
    CodingCli {
        id: "claude-code",
        name: "Claude Code",
        program: "claude",
        component: Some("claude-code"),
    },
    CodingCli {
        id: "codex",
        name: "Codex CLI",
        program: "codex",
        component: Some("codex-cli"),
    },
    CodingCli {
        id: "gemini",
        name: "Gemini CLI",
        program: "gemini",
        component: Some("gemini-cli"),
    },
    CodingCli {
        id: "aider",
        name: "Aider",
        program: "aider",
        component: None,
    },
    CodingCli {
        id: "opencode",
        name: "OpenCode",
        program: "opencode",
        component: None,
    },
];

/// One launcher entry: name, the command typed into the shell, and install status text
/// (`None` when unknown).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliChoice {
    /// Display name.
    pub name: String,
    /// Command line typed into the shell (no newline).
    pub command: String,
    /// "Installed 2.1.292", "Not installed", or `None` when Switchyard doesn't track it.
    pub status: Option<String>,
    /// Known to be missing.
    pub missing: bool,
}

/// Quote one word for a POSIX shell when it needs it.
fn sh_word(w: &str) -> String {
    if !w.is_empty()
        && w.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@+,%".contains(c))
    {
        w.to_owned()
    } else {
        format!("'{}'", w.replace('\'', "'\\''"))
    }
}

/// Quote one word for the shell the terminal runs: POSIX quoting on Unix and on Hosts,
/// double quotes on Windows.
pub fn shell_word(w: &str, posix: bool) -> String {
    if posix {
        sh_word(w)
    } else if w.is_empty() || w.contains([' ', '\t', '&', '(', ')', '^', '|', '<', '>']) {
        format!("\"{w}\"")
    } else {
        w.to_owned()
    }
}

/// The launcher's entries: the built-in CLIs (a program set in Settings → Assistant wins
/// over the PATH name) with their install status, then the custom CLI when configured.
pub fn choices(settings: &AssistantSettings, components: &[Component]) -> Vec<CliChoice> {
    let mut out: Vec<CliChoice> = CLIS
        .iter()
        .map(|c| {
            let path = match c.id {
                "claude-code" => settings.claude_code.path.trim(),
                "codex" => settings.codex.path.trim(),
                "gemini" => settings.gemini.path.trim(),
                _ => "",
            };
            let status = c
                .component
                .and_then(|id| components.iter().find(|k| k.id == id))
                .map(|k| &k.status);
            CliChoice {
                name: c.name.into(),
                command: if path.is_empty() {
                    c.program.into()
                } else {
                    path.into()
                },
                missing: matches!(status, Some(ComponentStatus::Missing)) && path.is_empty(),
                status: status.map(crate::assistant_settings::status_text),
            }
        })
        .collect();
    let custom = &settings.custom;
    // The settings page starts from an example; only a CLI the user set up is offered.
    let example = switchyard_core::agents::custom::CustomCli::example().program;
    if !custom.program.trim().is_empty() && custom.program.trim() != example {
        out.push(CliChoice {
            name: if custom.name.trim().is_empty() {
                "Custom CLI".into()
            } else {
                custom.name.trim().into()
            },
            command: custom.program.trim().into(),
            status: None,
            missing: false,
        });
    }
    out
}

/// ` cd '<dir>'` for a POSIX shell (no newline; the leading space keeps it out of history).
pub fn posix_cd(dir: &str) -> String {
    format!(" cd {}", sh_word(dir))
}

/// What is typed into a shell to run `command` (a program, maybe a path with spaces):
/// a leading space keeps it out of bash/zsh history where `ignorespace` is set.
pub fn launch_line(command: &str, cd: Option<&str>, posix: bool) -> String {
    let program = shell_word(command, posix);
    // PowerShell needs `&` to run a quoted path.
    let program = if !posix && program.starts_with('"') {
        format!("& {program}")
    } else {
        program
    };
    match cd {
        Some(dir) if posix => format!("{} && {program}\r", posix_cd(dir)),
        Some(dir) => format!("cd {}\r{program}\r", shell_word(dir, false)),
        None => format!("{program}\r"),
    }
}

/// The last part of a folder's path, for tab titles.
pub fn folder_name(path: &std::path::Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    let s = s.trim_end_matches('/');
    match s.rsplit_once('/') {
        Some((_, n)) if !n.is_empty() => n.to_owned(),
        _ if s.is_empty() => "/".into(),
        _ => s.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_and_launch_lines() {
        assert_eq!(launch_line("claude", None, true), "claude\r");
        assert_eq!(
            launch_line("/opt/My Tools/codex", Some("/home/me/it's"), true),
            " cd '/home/me/it'\\''s' && '/opt/My Tools/codex'\r"
        );
        assert_eq!(
            launch_line(r"C:\Program Files\gemini.cmd", None, false),
            "& \"C:\\Program Files\\gemini.cmd\"\r"
        );
        assert_eq!(shell_word("aider", false), "aider");
    }

    #[test]
    fn choices_use_settings_and_status() {
        let mut s = AssistantSettings::default();
        s.codex.path = "/usr/local/bin/codex".into();
        s.custom.program = "goose".into();
        let c = choices(&s, &[]);
        assert_eq!(c.len(), CLIS.len() + 1);
        assert_eq!(c[0].command, "claude");
        assert_eq!(c[1].command, "/usr/local/bin/codex");
        s.custom.name = String::new();
        assert_eq!(
            choices(&s, &[]).last().map(|c| c.name.clone()),
            Some("Custom CLI".to_owned())
        );
        assert_eq!(c.last().map(|c| c.command.as_str()), Some("goose"));
        assert!(c.iter().all(|c| !c.missing && c.status.is_none()));
        // The untouched example isn't offered.
        assert_eq!(
            choices(&AssistantSettings::default(), &[]).len(),
            CLIS.len()
        );
    }

    #[test]
    fn folder_names() {
        assert_eq!(folder_name(std::path::Path::new("/home/me/shop/")), "shop");
        assert_eq!(folder_name(std::path::Path::new("/")), "/");
        assert_eq!(folder_name(std::path::Path::new(r"C:\work\api")), "api");
    }
}
