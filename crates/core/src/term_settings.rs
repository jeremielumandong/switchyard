//! Terminal settings (Settings → Terminal) and session logs (MX-3).

use std::fs::{File, OpenOptions};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use switchyard_term::{LogMode, SessionLog};

/// Settings key the terminal settings are saved under.
pub const TERMINAL_SETTINGS_KEY: &str = "terminal";

/// Default file name of a session log.
pub const DEFAULT_LOG_TEMPLATE: &str = "{host}-{datetime}.log";

/// Terminal settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalSettings {
    /// Session logging.
    pub log: LogSettings,
    /// Copy selected text to the clipboard when the mouse button is released.
    pub copy_on_select: bool,
    /// The right mouse button pastes (when the program does not take mouse input).
    pub right_click_paste: bool,
    /// Ask before pasting text with more than one line.
    pub confirm_multiline_paste: bool,
    /// Color keywords in output (not in full-screen programs).
    pub highlight: bool,
    /// Keywords and their colors.
    pub highlight_rules: Vec<HighlightRule>,
}

impl Default for TerminalSettings {
    fn default() -> Self {
        Self {
            log: LogSettings::default(),
            copy_on_select: false,
            right_click_paste: false,
            confirm_multiline_paste: true,
            highlight: false,
            highlight_rules: default_highlight_rules(),
        }
    }
}

/// A keyword highlight color (drawn with the theme's ANSI color).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HighlightColor {
    /// Red.
    Red,
    /// Yellow.
    Yellow,
    /// Green.
    Green,
    /// Blue.
    Blue,
    /// Magenta.
    Magenta,
    /// Cyan.
    Cyan,
}

impl HighlightColor {
    /// Every color, in settings order.
    pub const ALL: [HighlightColor; 6] = [
        HighlightColor::Red,
        HighlightColor::Yellow,
        HighlightColor::Green,
        HighlightColor::Blue,
        HighlightColor::Magenta,
        HighlightColor::Cyan,
    ];

    /// Display name.
    pub fn label(self) -> &'static str {
        match self {
            HighlightColor::Red => "Red",
            HighlightColor::Yellow => "Yellow",
            HighlightColor::Green => "Green",
            HighlightColor::Blue => "Blue",
            HighlightColor::Magenta => "Magenta",
            HighlightColor::Cyan => "Cyan",
        }
    }

    /// The bright ANSI color index.
    pub fn ansi(self) -> u8 {
        match self {
            HighlightColor::Red => 9,
            HighlightColor::Green => 10,
            HighlightColor::Yellow => 11,
            HighlightColor::Blue => 12,
            HighlightColor::Magenta => 13,
            HighlightColor::Cyan => 14,
        }
    }
}

/// Words drawn in a color wherever they appear as whole words (any case).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HighlightRule {
    /// The word or phrase.
    pub word: String,
    /// Its color.
    pub color: HighlightColor,
}

/// error/fail… in red, warning in yellow, ok/success… in green.
pub fn default_highlight_rules() -> Vec<HighlightRule> {
    let rule = |word: &str, color| HighlightRule {
        word: word.to_owned(),
        color,
    };
    use HighlightColor::*;
    vec![
        rule("error", Red),
        rule("errors", Red),
        rule("fail", Red),
        rule("failed", Red),
        rule("failure", Red),
        rule("fatal", Red),
        rule("denied", Red),
        rule("warning", Yellow),
        rule("warn", Yellow),
        rule("ok", Green),
        rule("success", Green),
        rule("succeeded", Green),
        rule("passed", Green),
        rule("done", Green),
    ]
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

/// Where the rules' words appear in `text` as whole words, ignoring ASCII case: byte
/// ranges in order, not overlapping (the longest word wins at a position).
pub fn highlight_spans(
    text: &str,
    rules: &[HighlightRule],
) -> Vec<(std::ops::Range<usize>, HighlightColor)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let starts_word = i == 0 || !is_word_byte(bytes[i - 1]);
        let best = if starts_word {
            rules
                .iter()
                .filter(|r| {
                    let w = r.word.as_bytes();
                    !w.is_empty()
                        && bytes.len() >= i + w.len()
                        && bytes[i..i + w.len()].eq_ignore_ascii_case(w)
                        && bytes.get(i + w.len()).is_none_or(|&b| !is_word_byte(b))
                })
                .max_by_key(|r| r.word.len())
        } else {
            None
        };
        match best {
            Some(r) => {
                out.push((i..i + r.word.len(), r.color));
                i += r.word.len();
            }
            None => i += 1,
        }
    }
    out
}

/// What a session log keeps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogFormat {
    /// Text with escape sequences stripped.
    #[default]
    Plain,
    /// Every byte, escape sequences included.
    Raw,
}

impl LogFormat {
    /// Display name.
    pub fn label(self) -> &'static str {
        match self {
            LogFormat::Plain => "Plain text",
            LogFormat::Raw => "Raw (with escape sequences)",
        }
    }
}

/// How terminal sessions are logged to files.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LogSettings {
    /// Log every terminal session from the start.
    pub auto: bool,
    /// Plain text or raw bytes.
    pub format: LogFormat,
    /// Prefix each line of a plain log with the local time.
    pub timestamps: bool,
    /// Folder for log files; `None` = `<data>/terminal-logs`.
    pub folder: Option<String>,
    /// File name with `{host}`, `{date}`, `{time}` and `{datetime}` placeholders.
    pub template: String,
}

impl Default for LogSettings {
    fn default() -> Self {
        Self {
            auto: false,
            format: LogFormat::Plain,
            timestamps: false,
            folder: None,
            template: DEFAULT_LOG_TEMPLATE.to_owned(),
        }
    }
}

/// The file name for a log of a session on `host` started at `now`: placeholders
/// expanded, characters that are not allowed in file names replaced.
pub fn log_file_name(template: &str, host: &str, now: chrono::NaiveDateTime) -> String {
    let template = if template.trim().is_empty() {
        DEFAULT_LOG_TEMPLATE
    } else {
        template.trim()
    };
    let name = template
        .replace("{host}", host)
        .replace("{datetime}", &now.format("%Y%m%d-%H%M%S").to_string())
        .replace("{date}", &now.format("%Y-%m-%d").to_string())
        .replace("{time}", &now.format("%H%M%S").to_string());
    let name: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let name = name.trim().trim_start_matches('.').to_owned();
    if name.is_empty() {
        "terminal.log".into()
    } else {
        name
    }
}

/// Create a new log file in `dir` named `name`; when it exists, `name-1`, `name-2`, …
fn create_unique(dir: &Path, name: &str) -> std::io::Result<(File, PathBuf)> {
    std::fs::create_dir_all(dir)?;
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_owned(), format!(".{e}")),
        _ => (name.to_owned(), String::new()),
    };
    for n in 0..1000 {
        let path = if n == 0 {
            dir.join(name)
        } else {
            dir.join(format!("{stem}-{n}{ext}"))
        };
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(f) => return Ok((f, path)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("too many log files with this name"))
}

/// Open a session log for a terminal on `host` (blocking file I/O: call it off the UI
/// thread). `default_dir` is used when the settings name no folder.
pub fn open_session_log(
    settings: &LogSettings,
    host: &str,
    default_dir: &Path,
) -> std::io::Result<(SessionLog, PathBuf)> {
    let dir = settings
        .folder
        .as_deref()
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .map_or_else(|| default_dir.to_path_buf(), PathBuf::from);
    let now = chrono::Local::now().naive_local();
    let (file, path) = create_unique(&dir, &log_file_name(&settings.template, host, now))?;
    let (mode, clock) = match settings.format {
        LogFormat::Raw => (LogMode::Raw, None),
        LogFormat::Plain => (
            LogMode::Plain,
            settings.timestamps.then(|| -> switchyard_term::log::Clock {
                Box::new(|| chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string())
            }),
        ),
    };
    let log = SessionLog::new(
        Box::new(BufWriter::new(file)),
        mode,
        clock,
        path.display().to_string(),
    );
    Ok((log, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at() -> chrono::NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2026, 3, 7)
            .and_then(|d| d.and_hms_opt(9, 5, 30))
            .unwrap()
    }

    #[test]
    fn template_expands_host_and_time() {
        assert_eq!(
            log_file_name(DEFAULT_LOG_TEMPLATE, "web-1", at()),
            "web-1-20260307-090530.log"
        );
        assert_eq!(
            log_file_name("{date}/{time} {host}.txt", "db", at()),
            "2026-03-07_090530 db.txt"
        );
        // Nothing escapes the folder.
        assert_eq!(log_file_name("{host}", "../etc/x:y", at()), "_etc_x_y");
        assert_eq!(log_file_name("  ", "h", at()), "h-20260307-090530.log");
        assert_eq!(log_file_name("..", "h", at()), "terminal.log");
    }

    #[test]
    fn logs_never_overwrite_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let settings = LogSettings {
            folder: Some(dir.path().display().to_string()),
            template: "{host}.log".into(),
            timestamps: true,
            ..LogSettings::default()
        };
        let (mut a, pa) = open_session_log(&settings, "h", Path::new("/nonexistent")).unwrap();
        let (b, pb) = open_session_log(&settings, "h", Path::new("/nonexistent")).unwrap();
        assert_eq!(pa, dir.path().join("h.log"));
        assert_eq!(pb, dir.path().join("h-1.log"));
        a.write(b"\x1b[1mhello\x1b[0m\r\n").unwrap();
        drop((a, b));
        let text = std::fs::read_to_string(&pa).unwrap();
        // `[YYYY-MM-DD HH:MM:SS] hello`
        assert!(
            text.starts_with('[') && text.ends_with("] hello\n"),
            "{text}"
        );
        assert_eq!(text.len(), "[2026-03-07 09:05:30] hello\n".len());
    }

    #[test]
    fn highlights_whole_words_in_any_case() {
        let rules = default_highlight_rules();
        let spans = highlight_spans("ERROR: build failed; errors=2, 0 warnings, ok", &rules);
        let found: Vec<(&str, HighlightColor)> = spans
            .iter()
            .map(|(r, c)| {
                (
                    &"ERROR: build failed; errors=2, 0 warnings, ok"[r.clone()],
                    *c,
                )
            })
            .collect();
        assert_eq!(
            found,
            [
                ("ERROR", HighlightColor::Red),
                ("failed", HighlightColor::Red),
                ("errors", HighlightColor::Red),
                ("ok", HighlightColor::Green),
            ]
        );
        // Not inside other words, and multi-byte text is safe.
        assert!(highlight_spans("token booking erroneous", &rules).is_empty());
        assert!(highlight_spans("héerror okré", &rules).is_empty());
        let user = [HighlightRule {
            word: "out of memory".into(),
            color: HighlightColor::Magenta,
        }];
        assert_eq!(
            highlight_spans("x: Out of memory!", &user),
            [(3..16, HighlightColor::Magenta)]
        );
    }

    #[test]
    fn settings_default_when_fields_are_missing() {
        let s: TerminalSettings = serde_json::from_str(r#"{"log":{"auto":true}}"#).unwrap();
        assert!(s.log.auto);
        assert_eq!(s.log.template, DEFAULT_LOG_TEMPLATE);
        assert_eq!(s.log.format, LogFormat::Plain);
        assert!(s.confirm_multiline_paste && !s.copy_on_select && !s.right_click_paste);
        assert_eq!(s.highlight_rules, default_highlight_rules());
    }
}
