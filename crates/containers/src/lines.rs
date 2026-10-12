//! Splitting a byte stream (a process pipe or an SSH channel) into text lines.

/// Longest line kept; the rest of a longer line is dropped and `…` appended.
const MAX_LINE: usize = 64 * 1024;

/// Turns chunks of bytes into complete lines: `\n` or `\r\n` endings, a lone `\r`
/// (progress bars) ends a line too, invalid UTF-8 is replaced, very long lines are cut.
#[derive(Debug, Default)]
pub struct LineSplitter {
    partial: Vec<u8>,
    /// The current line was cut at [`MAX_LINE`]; skip until its end.
    overflow: bool,
    /// The previous chunk ended with `\r` (its `\n` may start the next one).
    after_cr: bool,
}

impl LineSplitter {
    /// A new splitter.
    pub fn new() -> Self {
        Self::default()
    }

    fn finish(&mut self, out: &mut Vec<String>) {
        let mut line = String::from_utf8_lossy(&self.partial).into_owned();
        if self.overflow {
            line.push('…');
        }
        out.push(line);
        self.partial.clear();
        self.overflow = false;
    }

    /// Feed bytes; complete lines are appended to `out`.
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<String>) {
        for &b in bytes {
            let after_cr = std::mem::take(&mut self.after_cr);
            match b {
                b'\n' if after_cr => {}
                b'\n' => self.finish(out),
                b'\r' => {
                    self.finish(out);
                    self.after_cr = true;
                }
                _ if self.partial.len() >= MAX_LINE => self.overflow = true,
                _ => self.partial.push(b),
            }
        }
    }

    /// The last line, if the stream ended without a newline.
    pub fn flush(&mut self, out: &mut Vec<String>) {
        if !self.partial.is_empty() || self.overflow {
            self.finish(out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_across_chunks_and_endings() {
        let mut s = LineSplitter::new();
        let mut out = Vec::new();
        s.feed(b"one\ntw", &mut out);
        assert_eq!(out, ["one"]);
        s.feed(b"o\r", &mut out);
        s.feed(b"\nthree\r\nfour", &mut out);
        assert_eq!(out, ["one", "two", "three"]);
        s.flush(&mut out);
        assert_eq!(out, ["one", "two", "three", "four"]);
        s.flush(&mut out);
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn blank_lines_and_bad_utf8() {
        let mut s = LineSplitter::new();
        let mut out = Vec::new();
        s.feed(b"a\n\nb\xff\n", &mut out);
        assert_eq!(out, ["a", "", "b\u{fffd}"]);
    }

    #[test]
    fn long_lines_are_cut() {
        let mut s = LineSplitter::new();
        let mut out = Vec::new();
        s.feed(&vec![b'x'; MAX_LINE + 10], &mut out);
        s.feed(b"\nok\n", &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].chars().count(), MAX_LINE + 1);
        assert!(out[0].ends_with('…'));
        assert_eq!(out[1], "ok");
    }
}
