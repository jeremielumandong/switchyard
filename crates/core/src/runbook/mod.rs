//! Runbooks at run time: `{{param}}` substitution ([`params`]), the run engine
//! ([`engine`]) and small helpers shared by the service and the runbook tab.
//!
//! The model and its storage live in `switchyard_store::runbooks`; steps execute in the
//! core service (`service::runbook`): SQL through the ordinary query path (so Production
//! confirmation, streaming, cancel and history work as in a SQL tab), HTTP through the
//! API workspace's sender, shell commands locally or over the Host's shared SSH session.

pub mod engine;
pub mod params;

pub use engine::{Gate, Next, Outcome, RunEngine, RunMode, StepStatus};
pub use params::{
    MASK, ParamError, ParamValue, ParamValues, Reveal, names_in, render_sql, render_text,
};

/// Most output kept (and streamed) per step.
pub const OUTPUT_CAP: usize = 256 * 1024;

/// Rows shown in a SQL step's grid before the rest is dropped.
pub const SQL_ROW_CAP: usize = 1_000;

/// `text` with every secret value replaced by [`MASK`].
pub fn redact(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_owned();
    for s in secrets {
        if !s.is_empty() {
            out = out.replace(s.as_str(), MASK);
        }
    }
    out
}

/// Headers from `Name: value` lines (blank lines and lines without a name are dropped).
pub fn parse_header_lines(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            let k = k.trim();
            (!k.is_empty()).then(|| (k.to_owned(), v.trim().to_owned()))
        })
        .collect()
}

/// Headers as `Name: value` lines.
pub fn header_lines(headers: &[(String, String)]) -> String {
    headers
        .iter()
        .map(|(k, v)| format!("{k}: {v}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Append `chunk` to `buf`, keeping at most [`OUTPUT_CAP`] bytes. Returns false once the
/// cap is reached (the rest is dropped).
pub fn append_capped(buf: &mut String, chunk: &str) -> bool {
    let room = OUTPUT_CAP.saturating_sub(buf.len());
    append_limited(buf, chunk, room)
}

/// Append at most `room` bytes of `chunk` to `buf` (cut on a character boundary).
/// Returns whether all of it fit.
pub fn append_limited(buf: &mut String, chunk: &str, room: usize) -> bool {
    if chunk.len() <= room {
        buf.push_str(chunk);
        return true;
    }
    let mut cut = room;
    while cut > 0 && !chunk.is_char_boundary(cut) {
        cut -= 1;
    }
    buf.push_str(&chunk[..cut]);
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_and_headers() {
        assert_eq!(
            redact("token=abc abc", &["abc".into(), String::new()]),
            format!("token={MASK} {MASK}")
        );
        let h = parse_header_lines("Accept: application/json\n\n: x\nX-A:  b:c ");
        assert_eq!(
            h,
            [
                ("Accept".to_owned(), "application/json".to_owned()),
                ("X-A".to_owned(), "b:c".to_owned())
            ]
        );
        assert_eq!(parse_header_lines(&header_lines(&h)), h);
    }

    #[test]
    fn output_is_capped_on_a_char_boundary() {
        let mut buf = "x".repeat(OUTPUT_CAP - 1);
        assert!(!append_capped(&mut buf, "éé"));
        assert_eq!(buf.len(), OUTPUT_CAP - 1);
        let mut buf = String::new();
        assert!(append_capped(&mut buf, "ok"));
        assert_eq!(buf, "ok");
    }
}
