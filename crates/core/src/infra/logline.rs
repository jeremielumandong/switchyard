//! Log lines from every source in one shape: time, level, source and message. Parses
//! journald's JSON export (`journalctl -o json`), JSON-per-line application logs and plain
//! text (the level read from words like `ERROR` / `warn`).

use serde_json::Value as Json;

/// Severity, most severe first.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    /// Emergency, alert, critical, fatal, panic.
    Fatal,
    /// Error.
    Error,
    /// Warning.
    Warn,
    /// Notice / info.
    Info,
    /// Debug.
    Debug,
    /// Trace.
    Trace,
    /// Not stated.
    #[default]
    Unknown,
}

impl Level {
    /// Short label.
    pub fn label(self) -> &'static str {
        match self {
            Level::Fatal => "FATAL",
            Level::Error => "ERROR",
            Level::Warn => "WARN",
            Level::Info => "INFO",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
            Level::Unknown => "",
        }
    }

    /// From a syslog priority (0 emergency … 7 debug).
    pub fn from_priority(p: u8) -> Self {
        match p {
            0..=2 => Level::Fatal,
            3 => Level::Error,
            4 => Level::Warn,
            5 | 6 => Level::Info,
            _ => Level::Debug,
        }
    }

    /// From a level name (`warning`, `ERR`, `Information`, `5`).
    pub fn from_name(s: &str) -> Self {
        let s = s.trim().to_ascii_lowercase();
        if let Ok(n) = s.parse::<u8>() {
            return Level::from_priority(n);
        }
        match s.as_str() {
            "fatal" | "panic" | "crit" | "critical" | "emerg" | "emergency" | "alert" => {
                Level::Fatal
            }
            "error" | "err" | "eror" | "severe" => Level::Error,
            "warn" | "warning" | "wrn" => Level::Warn,
            "info" | "information" | "informational" | "notice" | "inf" => Level::Info,
            "debug" | "dbg" | "verbose" | "fine" => Level::Debug,
            "trace" | "trc" | "finest" => Level::Trace,
            _ => Level::Unknown,
        }
    }
}

/// One log line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogLine {
    /// When, ms since the epoch (when the line says).
    pub ts_ms: Option<i64>,
    /// Severity.
    pub level: Level,
    /// Unit, stream, identifier or file.
    pub source: String,
    /// The message (one line; newlines kept as is for multi-line events).
    pub message: String,
}

impl LogLine {
    /// Whether every word of `filter` occurs (case-insensitive) in the message or source,
    /// and the level is at least `min` (`Unknown` lines always pass a level filter).
    pub fn matches(&self, filter: &str, min: Option<Level>) -> bool {
        if let Some(min) = min
            && self.level != Level::Unknown
            && self.level > min
        {
            return false;
        }
        if filter.trim().is_empty() {
            return true;
        }
        let msg = self.message.to_lowercase();
        let src = self.source.to_lowercase();
        filter.split_whitespace().all(|w| {
            let w = w.to_lowercase();
            msg.contains(&w) || src.contains(&w)
        })
    }

    /// `time level source message` for copying and export.
    pub fn to_text(&self) -> String {
        let mut s = String::new();
        if let Some(t) = self.ts_ms {
            s.push_str(&iso(t));
            s.push(' ');
        }
        if self.level != Level::Unknown {
            s.push_str(self.level.label());
            s.push(' ');
        }
        if !self.source.is_empty() {
            s.push_str(&self.source);
            s.push_str(": ");
        }
        s.push_str(&self.message);
        s
    }
}

/// `2026-10-12T10:00:00.123Z`.
pub fn iso(ms: i64) -> String {
    let d = switchyard_cloud::display_ms(ms); // `2026-10-12 10:00 UTC`
    let secs = ms.rem_euclid(60_000) / 1000;
    let frac = ms.rem_euclid(1000);
    format!(
        "{}T{}:{secs:02}.{frac:03}Z",
        d.get(0..10).unwrap_or_default(),
        d.get(11..16).unwrap_or_default()
    )
}

/// The level named in a plain text line, from its first words.
pub fn level_in_text(line: &str) -> Level {
    for word in line
        .split(|c: char| !c.is_ascii_alphabetic())
        .filter(|w| w.len() >= 3)
        .take(12)
    {
        // Upper-case words, or `[warn]`-style tags, count; ordinary words don't.
        let upper = word.chars().all(|c| c.is_ascii_uppercase());
        let tagged = line.contains(&format!("[{word}]")) || line.contains(&format!("level={word}"));
        if upper || tagged {
            let l = Level::from_name(word);
            if l != Level::Unknown {
                return l;
            }
        }
    }
    Level::Unknown
}

/// A leading timestamp (`2026-10-12T10:00:00Z`, `2026-10-12 10:00:00,123`).
fn leading_ts(line: &str) -> Option<i64> {
    let head = line.get(0..19)?;
    let ms = switchyard_cloud::parse_utc_ms(head)?;
    let frac = line
        .get(19..)
        .and_then(|r| r.strip_prefix('.').or_else(|| r.strip_prefix(',')))
        .map(|r| {
            r.chars()
                .take_while(char::is_ascii_digit)
                .take(3)
                .collect::<String>()
        })
        .and_then(|f| format!("{f:0<3}").parse::<i64>().ok())
        .unwrap_or(0);
    Some(ms + frac)
}

fn json_str(v: &Json) -> Option<String> {
    match v {
        Json::String(s) => Some(s.clone()),
        Json::Null => None,
        // journald sends non-UTF-8 messages as byte arrays.
        Json::Array(a) => Some(
            String::from_utf8_lossy(
                &a.iter()
                    .filter_map(|b| b.as_u64().and_then(|b| u8::try_from(b).ok()))
                    .collect::<Vec<u8>>(),
            )
            .into_owned(),
        ),
        other => Some(other.to_string()),
    }
}

/// One entry of `journalctl -o json`.
pub fn parse_journal(line: &str) -> Option<LogLine> {
    let j: Json = serde_json::from_str(line).ok()?;
    let get = |k: &str| j.get(k).and_then(json_str);
    let ts_ms = get("__REALTIME_TIMESTAMP")
        .and_then(|t| t.parse::<i64>().ok())
        .map(|us| us / 1000);
    let level = get("PRIORITY")
        .and_then(|p| p.parse::<u8>().ok())
        .map_or(Level::Unknown, Level::from_priority);
    let source = get("SYSLOG_IDENTIFIER")
        .or_else(|| get("_SYSTEMD_UNIT"))
        .or_else(|| get("_COMM"))
        .unwrap_or_default();
    let source = match get("_PID") {
        Some(pid) if !source.is_empty() => format!("{source}[{pid}]"),
        _ => source,
    };
    Some(LogLine {
        ts_ms,
        level,
        source,
        message: get("MESSAGE").unwrap_or_default(),
    })
}

/// A JSON object line from an application (`{"level":"info","msg":"…","time":"…"}`).
fn parse_json_line(line: &str, source: &str) -> Option<LogLine> {
    let j: Json = serde_json::from_str(line).ok()?;
    let o = j.as_object()?;
    let pick = |keys: &[&str]| keys.iter().find_map(|k| o.get(*k).and_then(json_str));
    let message = pick(&["msg", "message", "Message", "@m", "log", "text"])?;
    let level = pick(&["level", "lvl", "severity", "Severity", "@l", "loglevel"])
        .map_or(Level::Unknown, |l| Level::from_name(&l));
    let ts_ms = pick(&["time", "ts", "timestamp", "@timestamp", "@t", "Timestamp"]).and_then(|t| {
        t.parse::<f64>()
            .ok()
            .map(|n| {
                if n > 1e12 {
                    n as i64
                } else {
                    (n * 1000.0) as i64
                }
            })
            .or_else(|| leading_ts(&t))
    });
    Some(LogLine {
        ts_ms,
        level,
        source: source.to_owned(),
        message,
    })
}

/// A line of a text log file (JSON lines included), from `source`.
pub fn parse_text(line: &str, source: &str) -> LogLine {
    let line = line.trim_end_matches(['\r', '\n']);
    if line.starts_with('{')
        && let Some(l) = parse_json_line(line, source)
    {
        return l;
    }
    LogLine {
        ts_ms: leading_ts(line),
        level: level_in_text(line),
        source: source.to_owned(),
        message: line.to_owned(),
    }
}

/// Splits a byte stream into lines, keeping a partial last line for the next chunk.
#[derive(Debug, Default)]
pub struct LineSplitter {
    partial: Vec<u8>,
}

impl LineSplitter {
    /// Complete lines in `chunk` (with what was left over before).
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.partial.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(i) = self.partial.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=i).collect();
            out.push(String::from_utf8_lossy(&line[..line.len() - 1]).into_owned());
        }
        // A runaway line without a newline is cut at 64 KiB.
        if self.partial.len() > 64 * 1024 {
            out.push(String::from_utf8_lossy(&self.partial).into_owned());
            self.partial.clear();
        }
        out
    }

    /// Whatever is left (at the end of the stream).
    pub fn finish(&mut self) -> Option<String> {
        (!self.partial.is_empty()).then(|| {
            let s = String::from_utf8_lossy(&self.partial).into_owned();
            self.partial.clear();
            s
        })
    }
}

/// `tail` command for a file on a Host.
pub fn tail_command(path: &str, lines: u32) -> String {
    format!(
        "tail -n {lines} -F -- {} 2>&1",
        super::netcheck::shell_quote(path.trim())
    )
}

/// `journalctl` command following the journal (one unit, or everything).
pub fn journal_command(unit: Option<&str>, lines: u32) -> String {
    let mut c = format!("journalctl -f -o json -n {lines} --no-pager");
    if let Some(u) = unit.map(str::trim).filter(|u| !u.is_empty()) {
        c.push_str(" -u ");
        c.push_str(&super::netcheck::shell_quote(u));
    }
    c.push_str(" 2>&1");
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journald_entries() {
        let l = parse_journal(
            r#"{"__REALTIME_TIMESTAMP":"1791799200123456","PRIORITY":"3","SYSLOG_IDENTIFIER":"nginx","_PID":"812","MESSAGE":"upstream timed out","_SYSTEMD_UNIT":"nginx.service"}"#,
        )
        .unwrap();
        assert_eq!(l.ts_ms, Some(1_791_799_200_123));
        assert_eq!(l.level, Level::Error);
        assert_eq!(l.source, "nginx[812]");
        assert_eq!(l.message, "upstream timed out");
        let bytes = parse_journal(r#"{"PRIORITY":"6","MESSAGE":[104,105]}"#).unwrap();
        assert_eq!(bytes.message, "hi");
        assert_eq!(bytes.level, Level::Info);
        assert!(parse_journal("-- No entries --").is_none());
    }

    #[test]
    fn text_and_json_lines() {
        let l = parse_text("2026-10-12 10:00:00,250 WARN pool exhausted", "app.log");
        assert_eq!(l.level, Level::Warn);
        assert_eq!(l.ts_ms, Some(1_791_799_200_250));
        let l = parse_text("[error] 812#0: connect() failed", "error.log");
        assert_eq!(l.level, Level::Error);
        // Ordinary words are not levels.
        assert_eq!(parse_text("no error here", "x").level, Level::Unknown);
        let j = parse_text(
            r#"{"level":"debug","msg":"cache miss","time":"2026-10-12T10:00:00.5Z"}"#,
            "svc",
        );
        assert_eq!(j.level, Level::Debug);
        assert_eq!(j.message, "cache miss");
        assert_eq!(j.ts_ms, Some(1_791_799_200_500));
        assert_eq!(level_in_text("level=warning msg=x"), Level::Warn);
    }

    #[test]
    fn filtering_and_text() {
        let l = LogLine {
            ts_ms: Some(1_791_799_200_123),
            level: Level::Warn,
            source: "api".into(),
            message: "Slow query on orders".into(),
        };
        assert!(l.matches("slow ORDERS", None));
        assert!(l.matches("api", None));
        assert!(!l.matches("fast", None));
        assert!(l.matches("", Some(Level::Warn)));
        assert!(!l.matches("", Some(Level::Error)));
        assert_eq!(
            l.to_text(),
            "2026-10-12T10:00:00.123Z WARN api: Slow query on orders"
        );
    }

    #[test]
    fn splits_lines_across_chunks() {
        let mut s = LineSplitter::default();
        assert_eq!(s.push(b"one\ntw"), ["one"]);
        assert_eq!(s.push(b"o\r\nthree"), ["two\r"]);
        assert_eq!(s.finish().as_deref(), Some("three"));
        assert_eq!(
            journal_command(Some("nginx.service"), 100),
            "journalctl -f -o json -n 100 --no-pager -u 'nginx.service' 2>&1"
        );
        assert_eq!(
            tail_command("/var/log/a b", 10),
            "tail -n 10 -F -- '/var/log/a b' 2>&1"
        );
    }
}
