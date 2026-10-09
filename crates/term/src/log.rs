//! Session logs: a terminal's output copied to a file as it arrives (MX-3).
//!
//! A [`SessionLog`] is fed the same bytes as the terminal, on the I/O side. Raw logs keep
//! every byte (replayable with `cat`); plain logs strip escape sequences and control
//! characters, resolve carriage-return redraws and backspaces line by line, and can prefix
//! each line with a timestamp.

use std::io::Write;

/// What a session log keeps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LogMode {
    /// Text only: escape sequences and control characters stripped.
    #[default]
    Plain,
    /// Every byte the program sent.
    Raw,
}

/// Returns the timestamp put in front of each line of a plain log.
pub type Clock = Box<dyn FnMut() -> String + Send>;

/// Escape-sequence state of the stripper.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ground,
    /// After ESC.
    Esc,
    /// After ESC and an intermediate byte (charset designations like `ESC ( B`).
    EscIntermediate,
    /// Inside `ESC [ ...`.
    Csi,
    /// Inside a string (OSC, DCS, SOS, PM, APC) ended by BEL or ST.
    Str,
    /// ESC inside a string: `\` ends it.
    StrEsc,
}

/// Turns terminal output into plain lines.
#[derive(Debug)]
pub struct AnsiStripper {
    state: State,
    line: Vec<u8>,
    /// A carriage return was seen: the next printable byte starts the line over.
    pending_cr: bool,
}

impl Default for AnsiStripper {
    fn default() -> Self {
        Self::new()
    }
}

impl AnsiStripper {
    /// A stripper at the start of a line.
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            line: Vec::new(),
            pending_cr: false,
        }
    }

    /// Feed bytes; `emit` gets each completed line (without its newline).
    pub fn feed(&mut self, bytes: &[u8], mut emit: impl FnMut(&[u8])) {
        for &b in bytes {
            match self.state {
                State::Ground => self.ground(b, &mut emit),
                State::Esc => {
                    self.state = match b {
                        b'[' => State::Csi,
                        b']' | b'P' | b'X' | b'^' | b'_' => State::Str,
                        0x20..=0x2f => State::EscIntermediate,
                        _ => State::Ground,
                    }
                }
                State::EscIntermediate => {
                    if !(0x20..=0x2f).contains(&b) {
                        self.state = State::Ground;
                    }
                }
                State::Csi => match b {
                    0x40..=0x7e | 0x18 | 0x1a => self.state = State::Ground,
                    0x1b => self.state = State::Esc,
                    _ => {}
                },
                State::Str => match b {
                    0x07 | 0x18 | 0x1a => self.state = State::Ground,
                    0x1b => self.state = State::StrEsc,
                    _ => {}
                },
                State::StrEsc => {
                    self.state = if b == b'\\' {
                        State::Ground
                    } else {
                        State::Str
                    }
                }
            }
        }
    }

    fn ground(&mut self, b: u8, emit: &mut impl FnMut(&[u8])) {
        match b {
            0x1b => self.state = State::Esc,
            b'\n' => {
                emit(&self.line);
                self.line.clear();
                self.pending_cr = false;
            }
            b'\r' => self.pending_cr = true,
            0x08 => {
                // Back over one character (UTF-8 continuation bytes included).
                while let Some(last) = self.line.pop() {
                    if last & 0xc0 != 0x80 {
                        break;
                    }
                }
            }
            b'\t' => self.push(b),
            0x00..=0x1f | 0x7f => {}
            _ => self.push(b),
        }
    }

    fn push(&mut self, b: u8) {
        if self.pending_cr {
            // `\r` followed by text redraws the line (progress bars, prompts).
            self.line.clear();
            self.pending_cr = false;
        }
        self.line.push(b);
    }

    /// The unfinished line, if any; clears it.
    pub fn take_partial(&mut self) -> Option<Vec<u8>> {
        self.pending_cr = false;
        (!self.line.is_empty()).then(|| std::mem::take(&mut self.line))
    }
}

/// Copies a terminal's output into a file.
pub struct SessionLog {
    out: Box<dyn Write + Send>,
    mode: LogMode,
    stripper: AnsiStripper,
    clock: Option<Clock>,
    /// Shown in the UI (the file path).
    label: String,
}

impl std::fmt::Debug for SessionLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionLog")
            .field("mode", &self.mode)
            .field("label", &self.label)
            .finish_non_exhaustive()
    }
}

impl SessionLog {
    /// A log writing to `out`. `clock` timestamps each line of a plain log.
    pub fn new(
        out: Box<dyn Write + Send>,
        mode: LogMode,
        clock: Option<Clock>,
        label: impl Into<String>,
    ) -> Self {
        Self {
            out,
            mode,
            stripper: AnsiStripper::new(),
            clock,
            label: label.into(),
        }
    }

    /// Where the log goes (for display).
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Record program output.
    pub fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        match self.mode {
            LogMode::Raw => self.out.write_all(bytes)?,
            LogMode::Plain => {
                let Self {
                    out,
                    stripper,
                    clock,
                    ..
                } = self;
                let mut result = Ok(());
                stripper.feed(bytes, |line| {
                    if result.is_ok() {
                        result = write_line(out, clock, line);
                    }
                });
                result?;
            }
        }
        self.out.flush()
    }

    /// Write the unfinished line and flush. Called when logging stops.
    pub fn finish(&mut self) -> std::io::Result<()> {
        if self.mode == LogMode::Plain
            && let Some(line) = self.stripper.take_partial()
        {
            write_line(&mut self.out, &mut self.clock, &line)?;
        }
        self.out.flush()
    }
}

impl Drop for SessionLog {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

fn write_line(
    out: &mut Box<dyn Write + Send>,
    clock: &mut Option<Clock>,
    line: &[u8],
) -> std::io::Result<()> {
    if let Some(clock) = clock {
        write!(out, "[{}] ", clock())?;
    }
    out.write_all(line)?;
    out.write_all(b"\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn strip(input: &[u8]) -> Vec<String> {
        let mut s = AnsiStripper::new();
        let mut lines = Vec::new();
        s.feed(input, |l| {
            lines.push(String::from_utf8_lossy(l).into_owned())
        });
        if let Some(p) = s.take_partial() {
            lines.push(String::from_utf8_lossy(&p).into_owned());
        }
        lines
    }

    #[test]
    fn strips_colors_cursor_moves_and_titles() {
        assert_eq!(
            strip(b"\x1b[1;31merror\x1b[0m: \x1b[2Kdone\r\n"),
            ["error: done"]
        );
        assert_eq!(
            strip(b"\x1b]0;user@host: ~\x07$ ls\r\n\x1b]2;t\x1b\\ok\n"),
            ["$ ls", "ok"]
        );
        assert_eq!(strip(b"\x1b(Bplain\x1b=\x1b>\n"), ["plain"]);
        assert_eq!(strip(b"a\x1bPdcs data\x1b\\b\n"), ["ab"]);
    }

    #[test]
    fn sequences_split_across_chunks() {
        let mut s = AnsiStripper::new();
        let mut lines = Vec::new();
        for chunk in [
            &b"he\x1b"[..],
            b"[3",
            b"2mllo\x1b]0;ti",
            b"tle\x07!\r",
            b"\n",
        ] {
            s.feed(chunk, |l| {
                lines.push(String::from_utf8_lossy(l).into_owned())
            });
        }
        assert_eq!(lines, ["hello!"]);
    }

    #[test]
    fn carriage_return_redraws_and_backspace_erases() {
        assert_eq!(strip(b"10%\r50%\r100%\n"), ["100%"]);
        assert_eq!(strip(b"lss\x08 \x08\n"), ["ls"]);
        assert_eq!(strip(b"caf\xc3\xa9\x08e\n"), ["cafe"]);
        assert_eq!(strip(b"a\tb\x07\x00c\n"), ["a\tbc"]);
    }

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);

    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Buf {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    #[test]
    fn plain_log_timestamps_each_line() {
        let buf = Buf::default();
        let mut n = 0;
        let clock: Clock = Box::new(move || {
            n += 1;
            format!("t{n}")
        });
        let mut log = SessionLog::new(Box::new(buf.clone()), LogMode::Plain, Some(clock), "x");
        log.write(b"\x1b[32mone\x1b[0m\r\ntw").unwrap();
        log.write(b"o\r\nthree").unwrap();
        assert_eq!(buf.text(), "[t1] one\n[t2] two\n");
        log.finish().unwrap();
        assert_eq!(buf.text(), "[t1] one\n[t2] two\n[t3] three\n");
        drop(log);
        // Finishing twice does not repeat the last line.
        assert_eq!(buf.text(), "[t1] one\n[t2] two\n[t3] three\n");
    }

    #[test]
    fn raw_log_keeps_every_byte() {
        let buf = Buf::default();
        let mut log = SessionLog::new(Box::new(buf.clone()), LogMode::Raw, None, "x");
        log.write(b"\x1b[31mred\x1b[0m\r\n").unwrap();
        drop(log);
        assert_eq!(buf.text(), "\x1b[31mred\x1b[0m\r\n");
    }
}
