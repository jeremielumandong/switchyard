//! The log pane's buffer: lines as they stream in, capped, with the filter's matches
//! kept up to date so the virtualized list only draws what is visible.

use gpui_kit::SharedString;

/// Lines kept; older ones are dropped as new ones arrive.
pub const MAX_LINES: usize = 20_000;

/// Log lines and the indices that pass the filter.
#[derive(Default)]
pub struct LogBuffer {
    lines: Vec<SharedString>,
    /// Lowercased filter words.
    words: Vec<String>,
    /// Indices of `lines` that match, in order.
    visible: Vec<usize>,
    /// Lines dropped from the front since the start.
    pub dropped: usize,
}

impl LogBuffer {
    fn matches(&self, line: &str) -> bool {
        if self.words.is_empty() {
            return true;
        }
        let l = line.to_lowercase();
        self.words.iter().all(|w| l.contains(w.as_str()))
    }

    /// Start over.
    pub fn clear(&mut self) {
        self.lines.clear();
        self.visible.clear();
        self.dropped = 0;
    }

    /// Add lines, dropping the oldest past [`MAX_LINES`].
    pub fn push(&mut self, new: Vec<String>) {
        for l in new {
            if self.matches(&l) {
                self.visible.push(self.lines.len());
            }
            self.lines.push(l.into());
        }
        let over = self.lines.len().saturating_sub(MAX_LINES);
        if over > 0 {
            self.lines.drain(..over);
            self.dropped += over;
            let cut = self.visible.partition_point(|&i| i < over);
            self.visible.drain(..cut);
            for i in &mut self.visible {
                *i -= over;
            }
        }
    }

    /// Show only lines containing every word of `filter` (case-insensitive).
    pub fn set_filter(&mut self, filter: &str) {
        self.words = filter.split_whitespace().map(str::to_lowercase).collect();
        self.visible = (0..self.lines.len())
            .filter(|&i| self.matches(&self.lines[i]))
            .collect();
    }

    /// Whether a filter is set.
    pub fn filtered(&self) -> bool {
        !self.words.is_empty()
    }

    /// Matching lines.
    pub fn visible_len(&self) -> usize {
        self.visible.len()
    }

    /// All lines.
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// The `i`th matching line.
    pub fn visible(&self, i: usize) -> Option<SharedString> {
        self.visible
            .get(i)
            .and_then(|&ix| self.lines.get(ix))
            .cloned()
    }

    /// The matching lines as one text (Copy).
    pub fn visible_text(&self) -> String {
        let mut out = String::new();
        for &i in &self.visible {
            out.push_str(&self.lines[i]);
            out.push('\n');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(r: std::ops::Range<usize>) -> Vec<String> {
        r.map(|i| format!("line {i}")).collect()
    }

    #[test]
    fn filter_applies_to_old_and_new_lines() {
        let mut b = LogBuffer::default();
        b.push(vec!["GET /a 200".into(), "POST /b 500".into()]);
        b.set_filter("500");
        assert_eq!(b.visible_len(), 1);
        b.push(vec!["GET /c 500".into(), "GET /d 200".into()]);
        assert_eq!(b.visible_len(), 2);
        assert_eq!(b.visible(1).as_deref(), Some("GET /c 500"));
        b.set_filter("get 500");
        assert_eq!(b.visible_text(), "GET /c 500\n");
        b.set_filter("");
        assert_eq!(b.visible_len(), 4);
        assert!(!b.filtered());
    }

    #[test]
    fn oldest_lines_are_dropped() {
        let mut b = LogBuffer::default();
        b.set_filter("7");
        b.push(lines(0..MAX_LINES));
        let before = b.visible_len();
        b.push(lines(MAX_LINES..MAX_LINES + 100));
        assert_eq!(b.len(), MAX_LINES);
        assert_eq!(b.dropped, 100);
        // "line 7", "line 17", ... among the first 100 are gone; new ones with 7 came.
        let gone = (0..100).filter(|i| i.to_string().contains('7')).count();
        let came = (MAX_LINES..MAX_LINES + 100)
            .filter(|i| i.to_string().contains('7'))
            .count();
        assert_eq!(b.visible_len(), before - gone + came);
        assert!(b.visible(0).is_some_and(|l| l.contains('7')));
        b.clear();
        assert_eq!((b.len(), b.visible_len(), b.dropped), (0, 0, 0));
    }
}
