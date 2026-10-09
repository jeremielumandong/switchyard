//! Terminal state shared between the byte feeder (I/O side) and the view (UI side).
//!
//! Parsing happens on the I/O side: the feeder locks the [`Term`], runs bytes through the
//! ANSI parser and raises a coalesced "dirty" flag. The UI only takes short locks to copy
//! the visible screen into a [`Snapshot`], so `cat` of a huge file never blocks a frame for
//! longer than one parse chunk.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use alacritty_terminal::event::{Event as AlacEvent, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Direction, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::search::{Match, RegexSearch};
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape as AlacCursorShape, NamedColor, Processor};

use crate::log::SessionLog;

/// Default scrollback, in lines.
pub const DEFAULT_SCROLLBACK: usize = 10_000;

/// Most search matches tracked at once.
const MAX_MATCHES: usize = 1_000;

/// Something the terminal wants the host to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TermEvent {
    /// New content is ready to draw (coalesced until the next snapshot).
    Wakeup,
    /// Bytes to send back to the program (device status reports and similar).
    Reply(Vec<u8>),
    /// The program set the window title.
    Title(String),
    /// The program reset the title.
    ResetTitle,
    /// Bell.
    Bell,
    /// The program asked to copy text to the clipboard (OSC 52).
    Clipboard(String),
    /// Writing the session log failed; logging stopped.
    LogFailed(String),
}

/// Receives terminal events. Called with the terminal locked: it must not lock it again.
pub type EventSink = Arc<dyn Fn(TermEvent) + Send + Sync>;

/// Terminal size in cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TermSize {
    /// Columns.
    pub cols: u16,
    /// Rows.
    pub rows: u16,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.rows as usize
    }

    fn screen_lines(&self) -> usize {
        self.rows as usize
    }

    fn columns(&self) -> usize {
        self.cols as usize
    }
}

#[derive(Clone)]
struct Listener {
    sink: EventSink,
}

impl EventListener for Listener {
    fn send_event(&self, event: AlacEvent) {
        let ev = match event {
            AlacEvent::PtyWrite(s) => TermEvent::Reply(s.into_bytes()),
            AlacEvent::Title(t) => TermEvent::Title(t),
            AlacEvent::ResetTitle => TermEvent::ResetTitle,
            AlacEvent::Bell => TermEvent::Bell,
            AlacEvent::ClipboardStore(_, text) => TermEvent::Clipboard(text),
            _ => return,
        };
        (self.sink)(ev);
    }
}

/// A color as the program asked for it; the view maps it to the theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TermColor {
    /// Default foreground.
    Foreground,
    /// Default background.
    Background,
    /// One of the 256 indexed colors (0–15 are the themed ANSI colors).
    Indexed(u8),
    /// True color.
    Rgb(u8, u8, u8),
}

/// Text attributes of a run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Attrs {
    /// Bold.
    pub bold: bool,
    /// Italic.
    pub italic: bool,
    /// Faint.
    pub dim: bool,
    /// Any underline style.
    pub underline: bool,
    /// Strike-through.
    pub strike: bool,
}

/// Highlight drawn over a run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mark {
    /// None.
    #[default]
    None,
    /// Part of the selection.
    Selected,
    /// A search match.
    Match,
    /// The focused search match.
    FocusedMatch,
}

/// Consecutive cells with the same style.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    /// Byte length of the run's text in [`SnapLine::text`].
    pub len: usize,
    /// Number of cells the run covers.
    pub cells: u16,
    /// Foreground (after inverse/hidden are applied).
    pub fg: TermColor,
    /// Background (after inverse is applied).
    pub bg: TermColor,
    /// Attributes.
    pub attrs: Attrs,
    /// Selection or search highlight.
    pub mark: Mark,
}

/// One visible screen line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SnapLine {
    /// The characters (wide-char spacers omitted).
    pub text: String,
    /// Style runs covering `text`.
    pub runs: Vec<Run>,
}

/// Cursor shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorShape {
    /// Block.
    Block,
    /// Underline.
    Underline,
    /// Vertical bar.
    Beam,
    /// Hollow block (unfocused).
    HollowBlock,
}

/// Where to draw the cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    /// Visible row.
    pub row: u16,
    /// Column.
    pub col: u16,
    /// Shape.
    pub shape: CursorShape,
}

/// Terminal modes the view needs for input encoding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modes {
    /// DECCKM: cursor keys send SS3 sequences.
    pub app_cursor: bool,
    /// Bracketed paste.
    pub bracketed_paste: bool,
    /// Any mouse reporting mode is on.
    pub mouse: bool,
    /// Report motion with a button held.
    pub mouse_drag: bool,
    /// Report all motion.
    pub mouse_motion: bool,
    /// SGR (1006) mouse encoding.
    pub sgr_mouse: bool,
    /// The alternate screen is active (full-screen programs).
    pub alt_screen: bool,
    /// Scroll wheel sends arrow keys on the alternate screen.
    pub alternate_scroll: bool,
    /// Focus in/out reporting.
    pub focus_events: bool,
}

/// A copy of what is on screen.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// Visible lines, top to bottom.
    pub lines: Vec<SnapLine>,
    /// Size in cells.
    pub cols: u16,
    /// Rows.
    pub rows: u16,
    /// Cursor, when visible on screen.
    pub cursor: Option<Cursor>,
    /// Lines scrolled back from the bottom.
    pub display_offset: usize,
    /// Lines of scrollback available.
    pub history: usize,
    /// Modes.
    pub modes: Modes,
    /// `(focused index, total)` while searching.
    pub search: Option<(usize, usize)>,
}

struct Search {
    matches: Vec<Match>,
    focused: usize,
}

struct Shared {
    term: FairMutex<Term<Listener>>,
    dirty: AtomicBool,
    sink: EventSink,
    /// Session log fed with the program's output (MX-3); `logging` mirrors `is_some`
    /// so the feeder skips the lock when there is none.
    log: std::sync::Mutex<Option<SessionLog>>,
    logging: AtomicBool,
}

/// The UI-side handle. Cheap to clone.
#[derive(Clone)]
pub struct Terminal {
    shared: Arc<Shared>,
    search: Arc<std::sync::Mutex<Option<Search>>>,
    /// Generation of the latest search; a scan for an older one stops and drops its result.
    search_gen: Arc<AtomicU64>,
}

/// The I/O-side handle that parses program output into the terminal. One per terminal.
pub struct Feeder {
    shared: Arc<Shared>,
    parser: Processor,
}

impl std::fmt::Debug for Feeder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Feeder").finish_non_exhaustive()
    }
}

impl std::fmt::Debug for Terminal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Terminal").finish_non_exhaustive()
    }
}

/// Create a terminal and its feeder.
pub fn new_terminal(size: TermSize, scrollback: usize, sink: EventSink) -> (Terminal, Feeder) {
    let config = Config {
        scrolling_history: scrollback,
        ..Config::default()
    };
    let size = TermSize {
        cols: size.cols.max(2),
        rows: size.rows.max(1),
    };
    let term = Term::new(config, &size, Listener { sink: sink.clone() });
    let shared = Arc::new(Shared {
        term: FairMutex::new(term),
        dirty: AtomicBool::new(false),
        sink,
        log: std::sync::Mutex::new(None),
        logging: AtomicBool::new(false),
    });
    (
        Terminal {
            shared: shared.clone(),
            search: Arc::default(),
            search_gen: Arc::default(),
        },
        Feeder {
            shared,
            parser: Processor::new(),
        },
    )
}

impl Feeder {
    /// Parse program output. Raises one [`TermEvent::Wakeup`] per batch of changes.
    pub fn feed(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        {
            let mut term = self.shared.term.lock();
            self.parser.advance(&mut *term, bytes);
        }
        if self.shared.logging.load(Ordering::Acquire) {
            self.log(bytes);
        }
        if !self.shared.dirty.swap(true, Ordering::AcqRel) {
            (self.shared.sink)(TermEvent::Wakeup);
        }
    }
}

fn named_index(n: NamedColor) -> (TermColor, bool) {
    use NamedColor as N;
    let ix = n as usize;
    if ix < 16 {
        return (TermColor::Indexed(ix as u8), false);
    }
    match n {
        N::Background => (TermColor::Background, false),
        N::DimForeground => (TermColor::Foreground, true),
        N::DimBlack => (TermColor::Indexed(0), true),
        N::DimRed => (TermColor::Indexed(1), true),
        N::DimGreen => (TermColor::Indexed(2), true),
        N::DimYellow => (TermColor::Indexed(3), true),
        N::DimBlue => (TermColor::Indexed(4), true),
        N::DimMagenta => (TermColor::Indexed(5), true),
        N::DimCyan => (TermColor::Indexed(6), true),
        N::DimWhite => (TermColor::Indexed(7), true),
        _ => (TermColor::Foreground, false),
    }
}

fn resolve(c: Color, colors: &alacritty_terminal::term::color::Colors) -> (TermColor, bool) {
    match c {
        Color::Spec(rgb) => (TermColor::Rgb(rgb.r, rgb.g, rgb.b), false),
        Color::Indexed(i) => match colors[i as usize] {
            Some(rgb) => (TermColor::Rgb(rgb.r, rgb.g, rgb.b), false),
            None => (TermColor::Indexed(i), false),
        },
        Color::Named(n) => match colors[n] {
            Some(rgb) => (TermColor::Rgb(rgb.r, rgb.g, rgb.b), false),
            None => named_index(n),
        },
    }
}

impl Feeder {
    fn log(&self, bytes: &[u8]) {
        let mut slot = lock_log(&self.shared.log);
        let Some(log) = slot.as_mut() else { return };
        if let Err(e) = log.write(bytes) {
            tracing::warn!(error = %e, "session log failed; logging stopped");
            *slot = None;
            self.shared.logging.store(false, Ordering::Release);
            drop(slot);
            (self.shared.sink)(TermEvent::LogFailed(e.to_string()));
        }
    }
}

fn lock_log(
    m: &std::sync::Mutex<Option<SessionLog>>,
) -> std::sync::MutexGuard<'_, Option<SessionLog>> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Terminal {
    /// Start (`Some`) or stop (`None`) copying output into a session log. Returns the
    /// previous log, unfinished: the caller finishes it off the UI thread.
    pub fn set_log(&self, log: Option<SessionLog>) -> Option<SessionLog> {
        let mut slot = lock_log(&self.shared.log);
        self.shared.logging.store(log.is_some(), Ordering::Release);
        std::mem::replace(&mut *slot, log)
    }

    /// Where the session log goes, while logging.
    pub fn log_label(&self) -> Option<String> {
        lock_log(&self.shared.log)
            .as_ref()
            .map(|l| l.label().to_owned())
    }

    /// Copy the visible screen. Clears the dirty flag so the next change wakes the UI.
    pub fn snapshot(&self) -> Snapshot {
        self.shared.dirty.store(false, Ordering::Release);
        let search = self.search.lock().ok();
        let matches: Vec<(Match, bool)> = search
            .as_ref()
            .and_then(|s| s.as_ref())
            .map(|s| {
                s.matches
                    .iter()
                    .enumerate()
                    .map(|(i, m)| (m.clone(), i == s.focused))
                    .collect()
            })
            .unwrap_or_default();
        let search_pos = search
            .as_ref()
            .and_then(|s| s.as_ref())
            .map(|s| (s.focused, s.matches.len()));
        drop(search);

        let term = self.shared.term.lock();
        let grid = term.grid();
        let cols = grid.columns();
        let rows = grid.screen_lines();
        let offset = grid.display_offset();
        let content = term.renderable_content();
        let selection = content.selection;
        let mode = *term.mode();
        let colors = term.colors();
        // Only matches on screen can mark a cell; each cell checks them all.
        let top = Line(-(offset as i32));
        let bottom = Line(rows as i32 - 1 - offset as i32);
        let mut matches = matches;
        matches.retain(|(m, _)| m.end().line >= top && m.start().line <= bottom);

        let mut lines = Vec::with_capacity(rows);
        for r in 0..rows {
            let line = Line(r as i32 - offset as i32);
            let row = &grid[line];
            let mut out = SnapLine::default();
            for c in 0..cols {
                let cell = &row[Column(c)];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    if let Some(last) = out.runs.last_mut() {
                        last.cells += 1;
                    }
                    continue;
                }
                let point = Point::new(line, Column(c));
                let (mut fg, fg_dim) = resolve(cell.fg, colors);
                let (mut bg, _) = resolve(cell.bg, colors);
                if cell.flags.contains(Flags::INVERSE) {
                    std::mem::swap(&mut fg, &mut bg);
                }
                if cell.flags.contains(Flags::HIDDEN) {
                    fg = bg;
                }
                let attrs = Attrs {
                    bold: cell.flags.contains(Flags::BOLD),
                    italic: cell.flags.contains(Flags::ITALIC),
                    dim: fg_dim || cell.flags.contains(Flags::DIM),
                    underline: cell.flags.intersects(Flags::ALL_UNDERLINES),
                    strike: cell.flags.contains(Flags::STRIKEOUT),
                };
                let mark = match matches.iter().find(|(m, _)| m.contains(&point)) {
                    Some((_, true)) => Mark::FocusedMatch,
                    Some((_, false)) => Mark::Match,
                    None if selection.is_some_and(|s| s.contains(point)) => Mark::Selected,
                    None => Mark::None,
                };
                let ch = if cell.c == '\0' { ' ' } else { cell.c };
                let start = out.text.len();
                out.text.push(ch);
                if let Some(zw) = cell.zerowidth() {
                    out.text.extend(zw.iter());
                }
                let len = out.text.len() - start;
                match out.runs.last_mut() {
                    Some(last)
                        if last.fg == fg
                            && last.bg == bg
                            && last.attrs == attrs
                            && last.mark == mark =>
                    {
                        last.len += len;
                        last.cells += 1;
                    }
                    _ => out.runs.push(Run {
                        len,
                        cells: 1,
                        fg,
                        bg,
                        attrs,
                        mark,
                    }),
                }
            }
            lines.push(out);
        }

        let cursor = (mode.contains(TermMode::SHOW_CURSOR))
            .then_some(content.cursor)
            .and_then(|c| {
                let row = c.point.line.0 + offset as i32;
                (row >= 0 && (row as usize) < rows && c.shape != AlacCursorShape::Hidden).then_some(
                    Cursor {
                        row: row as u16,
                        col: c.point.column.0 as u16,
                        shape: match c.shape {
                            AlacCursorShape::Underline => CursorShape::Underline,
                            AlacCursorShape::Beam => CursorShape::Beam,
                            AlacCursorShape::HollowBlock => CursorShape::HollowBlock,
                            _ => CursorShape::Block,
                        },
                    },
                )
            });

        Snapshot {
            lines,
            cols: cols as u16,
            rows: rows as u16,
            cursor,
            display_offset: offset,
            history: grid.history_size(),
            modes: Modes {
                app_cursor: mode.contains(TermMode::APP_CURSOR),
                bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
                mouse: mode.intersects(TermMode::MOUSE_MODE),
                mouse_drag: mode.contains(TermMode::MOUSE_DRAG),
                mouse_motion: mode.contains(TermMode::MOUSE_MOTION),
                sgr_mouse: mode.contains(TermMode::SGR_MOUSE),
                alt_screen: mode.contains(TermMode::ALT_SCREEN),
                alternate_scroll: mode.contains(TermMode::ALTERNATE_SCROLL),
                focus_events: mode.contains(TermMode::FOCUS_IN_OUT),
            },
            search: search_pos,
        }
    }

    /// Whether output arrived since the last snapshot.
    pub fn is_dirty(&self) -> bool {
        self.shared.dirty.load(Ordering::Acquire)
    }

    /// Resize the grid (the caller also resizes the PTY or SSH channel).
    pub fn resize(&self, size: TermSize) {
        let size = TermSize {
            cols: size.cols.max(2),
            rows: size.rows.max(1),
        };
        self.shared.term.lock().resize(size);
    }

    /// Current size.
    pub fn size(&self) -> TermSize {
        let term = self.shared.term.lock();
        TermSize {
            cols: term.columns() as u16,
            rows: term.screen_lines() as u16,
        }
    }

    /// Scroll the view by `lines` (positive = towards older output).
    pub fn scroll(&self, lines: i32) {
        self.shared.term.lock().scroll_display(Scroll::Delta(lines));
    }

    /// Jump back to the live screen.
    pub fn scroll_to_bottom(&self) {
        self.shared.term.lock().scroll_display(Scroll::Bottom);
    }

    fn point(term: &Term<Listener>, row: u16, col: u16) -> Point {
        let offset = term.grid().display_offset() as i32;
        let col = (col as usize).min(term.columns().saturating_sub(1));
        Point::new(Line(row as i32 - offset), Column(col))
    }

    /// Start a selection at a visible cell. `kind` 1 = characters, 2 = words, 3 = lines.
    pub fn start_selection(&self, row: u16, col: u16, right_half: bool, kind: u8) {
        let mut term = self.shared.term.lock();
        let p = Self::point(&term, row, col);
        let ty = match kind {
            2 => SelectionType::Semantic,
            3 => SelectionType::Lines,
            _ => SelectionType::Simple,
        };
        let side = if right_half { Side::Right } else { Side::Left };
        term.selection = Some(Selection::new(ty, p, side));
    }

    /// Extend the selection to a visible cell.
    pub fn update_selection(&self, row: u16, col: u16, right_half: bool) {
        let mut term = self.shared.term.lock();
        let p = Self::point(&term, row, col);
        let side = if right_half { Side::Right } else { Side::Left };
        if let Some(sel) = term.selection.as_mut() {
            sel.update(p, side);
        }
    }

    /// Drop the selection.
    pub fn clear_selection(&self) {
        self.shared.term.lock().selection = None;
    }

    /// The selected text.
    pub fn selection_text(&self) -> Option<String> {
        self.shared
            .term
            .lock()
            .selection_to_string()
            .filter(|s| !s.is_empty())
    }

    /// Text of one visible row (for link detection).
    pub fn row_text(&self, row: u16) -> String {
        let term = self.shared.term.lock();
        let p = Self::point(&term, row, 0);
        let end = Point::new(p.line, Column(term.columns().saturating_sub(1)));
        term.bounds_to_string(p, end)
    }

    /// Search the scrollback for `pattern` (plain text, case-insensitive). Returns the
    /// number of matches, newest first focused. Blocks while it scans: the UI uses
    /// [`Self::begin_search`] and [`Self::search_as`] off its thread instead.
    pub fn search(&self, pattern: &str) -> usize {
        let generation = self.begin_search();
        self.search_as(pattern, generation).unwrap_or(0)
    }

    /// Start a new search and return its generation. A scan still running for an older
    /// generation stops at its next match and its result is dropped.
    pub fn begin_search(&self) -> u64 {
        self.search_gen.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Whether `generation` is still the latest search.
    pub fn search_current(&self, generation: u64) -> bool {
        self.search_gen.load(Ordering::Acquire) == generation
    }

    /// Scan the scrollback for `pattern` as search `generation` (from
    /// [`Self::begin_search`]); safe to call from any thread. Installs the matches and
    /// returns their count, or returns `None` without touching the current search when a
    /// newer search started meanwhile.
    pub fn search_as(&self, pattern: &str, generation: u64) -> Option<usize> {
        if pattern.is_empty() {
            return self.install_search(None, generation).then_some(0);
        }
        let escaped: String = pattern
            .chars()
            .flat_map(|c| {
                let special = "\\.+*?()|[]{}^$".contains(c);
                special
                    .then_some('\\')
                    .into_iter()
                    .chain(std::iter::once(c))
            })
            .collect();
        let Ok(mut regex) = RegexSearch::new(&format!("(?i){escaped}")) else {
            return self.install_search(None, generation).then_some(0);
        };
        let term = self.shared.term.lock();
        let mut matches = Vec::new();
        let mut origin = Point::new(term.topmost_line(), Column(0));
        while matches.len() < MAX_MATCHES {
            if !self.search_current(generation) {
                return None;
            }
            let Some(m) = term.search_next(&mut regex, origin, Direction::Right, Side::Left, None)
            else {
                break;
            };
            if matches
                .last()
                .is_some_and(|last: &Match| m.start() <= last.start())
            {
                break;
            }
            let next = *m.end();
            matches.push(m);
            origin = next.add(&*term, alacritty_terminal::index::Boundary::None, 1);
            if next >= Point::new(term.bottommost_line(), term.last_column()) {
                break;
            }
        }
        drop(term);
        let n = matches.len();
        let focused = n.saturating_sub(1);
        if !self.install_search(Some(Search { matches, focused }), generation) {
            return None;
        }
        self.reveal_focused();
        Some(n)
    }

    /// Replace the current search with `search` unless `generation` is stale.
    fn install_search(&self, search: Option<Search>, generation: u64) -> bool {
        let Ok(mut guard) = self.search.lock() else {
            return false;
        };
        // Checked under the lock: a newer search installs after this one, never before.
        if !self.search_current(generation) {
            return false;
        }
        *guard = search;
        true
    }

    /// Move to the next (`forward`) or previous match.
    pub fn search_step(&self, forward: bool) {
        if let Ok(mut guard) = self.search.lock()
            && let Some(s) = guard.as_mut()
            && !s.matches.is_empty()
        {
            let n = s.matches.len();
            s.focused = if forward {
                (s.focused + 1) % n
            } else {
                (s.focused + n - 1) % n
            };
        }
        self.reveal_focused();
    }

    /// Stop searching.
    pub fn clear_search(&self) {
        self.begin_search();
        if let Ok(mut g) = self.search.lock() {
            *g = None;
        }
    }

    fn reveal_focused(&self) {
        let line = self.search.lock().ok().and_then(|g| {
            g.as_ref()
                .and_then(|s| s.matches.get(s.focused).map(|m| m.start().line))
        });
        let Some(line) = line else { return };
        let mut term = self.shared.term.lock();
        let rows = term.screen_lines() as i32;
        let offset = term.grid().display_offset() as i32;
        let visible_row = line.0 + offset;
        if visible_row < 0 || visible_row >= rows {
            // Put the match in the middle of the screen.
            let target = (rows / 2 - line.0).max(0);
            term.scroll_display(Scroll::Delta(target - offset));
        }
    }

    /// Mark the terminal as needing a redraw (e.g. after a local view change).
    pub fn touch(&self) {
        if !self.shared.dirty.swap(true, Ordering::AcqRel) {
            (self.shared.sink)(TermEvent::Wakeup);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn term(cols: u16, rows: u16) -> (Terminal, Feeder, Arc<Mutex<Vec<TermEvent>>>) {
        let events: Arc<Mutex<Vec<TermEvent>>> = Arc::default();
        let e = events.clone();
        let sink: EventSink = Arc::new(move |ev| e.lock().expect("lock").push(ev));
        let (t, f) = new_terminal(TermSize { cols, rows }, 100, sink);
        (t, f, events)
    }

    fn text(s: &Snapshot, row: usize) -> String {
        s.lines[row].text.trim_end().to_owned()
    }

    #[test]
    fn plain_text_and_newlines() {
        let (t, mut f, ev) = term(20, 4);
        f.feed(b"hello\r\nworld");
        let s = t.snapshot();
        assert_eq!(text(&s, 0), "hello");
        assert_eq!(text(&s, 1), "world");
        assert_eq!(
            s.cursor,
            Some(Cursor {
                row: 1,
                col: 5,
                shape: CursorShape::Block
            })
        );
        assert_eq!(ev.lock().expect("lock").as_slice(), [TermEvent::Wakeup]);
    }

    #[test]
    fn wakeups_coalesce_until_snapshot() {
        let (t, mut f, ev) = term(20, 4);
        f.feed(b"a");
        f.feed(b"b");
        assert_eq!(ev.lock().expect("lock").len(), 1);
        t.snapshot();
        f.feed(b"c");
        assert_eq!(ev.lock().expect("lock").len(), 2);
    }

    #[test]
    fn sgr_colors_and_attributes() {
        let (t, mut f, _) = term(30, 2);
        f.feed(
            b"\x1b[1;31mred\x1b[0m \x1b[38;2;10;20;30mrgb\x1b[0m \x1b[7minv\x1b[0m \x1b[38;5;208mx",
        );
        let s = t.snapshot();
        let runs = &s.lines[0].runs;
        assert_eq!(runs[0].fg, TermColor::Indexed(1));
        assert!(runs[0].attrs.bold);
        assert_eq!(runs[0].cells, 3);
        let rgb = runs
            .iter()
            .find(|r| r.fg == TermColor::Rgb(10, 20, 30))
            .expect("rgb run");
        assert_eq!(rgb.cells, 3);
        let inv = runs
            .iter()
            .find(|r| r.fg == TermColor::Background && r.bg == TermColor::Foreground)
            .expect("inverse run");
        assert_eq!(inv.cells, 3);
        assert!(runs.iter().any(|r| r.fg == TermColor::Indexed(208)));
    }

    #[test]
    fn cursor_movement_and_erase() {
        let (t, mut f, _) = term(10, 3);
        f.feed(b"abcdef\x1b[3D\x1b[KXY\x1b[2;4Hz");
        let s = t.snapshot();
        assert_eq!(text(&s, 0), "abcXY");
        assert_eq!(text(&s, 1), "   z");
    }

    #[test]
    fn alternate_screen_and_modes() {
        let (t, mut f, _) = term(10, 3);
        f.feed(b"shell");
        f.feed(b"\x1b[?1049h\x1b[?1h\x1b[?2004h\x1b[?1002h\x1b[?1006h\x1b[Hfull");
        let s = t.snapshot();
        assert!(s.modes.alt_screen && s.modes.app_cursor && s.modes.bracketed_paste);
        assert!(s.modes.mouse && s.modes.mouse_drag && s.modes.sgr_mouse);
        assert_eq!(text(&s, 0), "full");
        f.feed(b"\x1b[?1049l");
        let s = t.snapshot();
        assert!(!s.modes.alt_screen);
        assert_eq!(text(&s, 0), "shell");
    }

    #[test]
    fn device_status_report_replies() {
        let (_t, mut f, ev) = term(10, 3);
        f.feed(b"ab\x1b[6n");
        assert!(
            ev.lock()
                .expect("lock")
                .contains(&TermEvent::Reply(b"\x1b[1;3R".to_vec()))
        );
    }

    #[test]
    fn title_is_reported() {
        let (_t, mut f, ev) = term(10, 3);
        f.feed(b"\x1b]0;deploy@prod\x07");
        assert!(
            ev.lock()
                .expect("lock")
                .contains(&TermEvent::Title("deploy@prod".into()))
        );
    }

    #[test]
    fn scrollback_and_scrolling() {
        let (t, mut f, _) = term(10, 3);
        for i in 0..10 {
            f.feed(format!("line{i}\r\n").as_bytes());
        }
        let s = t.snapshot();
        assert_eq!(s.history, 8);
        assert_eq!(text(&s, 0), "line8");
        t.scroll(3);
        let s = t.snapshot();
        assert_eq!(s.display_offset, 3);
        assert_eq!(text(&s, 0), "line5");
        assert_eq!(s.cursor, None, "cursor scrolled off screen");
        t.scroll_to_bottom();
        assert_eq!(t.snapshot().display_offset, 0);
    }

    #[test]
    fn scrollback_is_bounded() {
        let (t, mut f, _) = term(10, 3);
        for i in 0..500 {
            f.feed(format!("{i}\r\n").as_bytes());
        }
        assert_eq!(t.snapshot().history, 100);
    }

    #[test]
    fn resize_rewraps() {
        let (t, mut f, _) = term(10, 3);
        f.feed(b"0123456789abc");
        t.resize(TermSize { cols: 20, rows: 3 });
        let s = t.snapshot();
        assert_eq!(s.cols, 20);
        assert_eq!(text(&s, 0), "0123456789abc");
    }

    #[test]
    fn selection_copies_text() {
        let (t, mut f, _) = term(20, 3);
        f.feed(b"hello world\r\nsecond");
        t.start_selection(0, 6, false, 1);
        t.update_selection(1, 2, true);
        assert_eq!(t.selection_text().as_deref(), Some("world\nsec"));
        let s = t.snapshot();
        assert!(s.lines[0].runs.iter().any(|r| r.mark == Mark::Selected));
        t.start_selection(0, 1, false, 2);
        assert_eq!(t.selection_text().as_deref(), Some("hello"));
    }

    #[test]
    fn search_finds_and_focuses_matches() {
        let (t, mut f, _) = term(20, 3);
        for i in 0..20 {
            f.feed(format!("req {i} POST /api\r\n").as_bytes());
        }
        let n = t.search("post /API");
        assert_eq!(n, 20);
        let s = t.snapshot();
        assert_eq!(s.search, Some((19, 20)));
        t.search_step(false);
        t.search_step(false);
        let s = t.snapshot();
        assert_eq!(s.search, Some((17, 20)));
        assert!(
            s.lines
                .iter()
                .flat_map(|l| &l.runs)
                .any(|r| r.mark == Mark::FocusedMatch)
        );
        // An older match scrolls the view.
        t.search_step(true);
        for _ in 0..10 {
            t.search_step(false);
        }
        let s = t.snapshot();
        assert!(s.display_offset > 0);
        // Matches in the scrollback still mark their cells once scrolled into view.
        assert!(
            s.lines
                .iter()
                .flat_map(|l| &l.runs)
                .any(|r| r.mark == Mark::FocusedMatch)
        );
        assert_eq!(t.search("a.b("), 0, "special characters are literal");
    }

    #[test]
    fn a_stale_search_is_dropped() {
        let (t, mut f, _) = term(20, 3);
        for i in 0..20 {
            f.feed(format!("line {i}\r\n").as_bytes());
        }
        let old = t.begin_search();
        let new = t.begin_search();
        assert_eq!(
            t.search_as("line", old),
            None,
            "superseded scan installs nothing"
        );
        assert_eq!(t.snapshot().search, None);
        assert_eq!(t.search_as("line 1", new), Some(11));
        assert_eq!(t.snapshot().search, Some((10, 11)));
        // A late result for the old generation does not overwrite the newer one.
        assert_eq!(t.search_as("line", old), None);
        assert_eq!(t.snapshot().search, Some((10, 11)));
        // Clearing makes an in-flight scan stale too.
        t.clear_search();
        assert_eq!(t.search_as("line", new), None);
        assert_eq!(t.snapshot().search, None);
    }

    #[test]
    fn wide_chars_take_two_cells() {
        let (t, mut f, _) = term(10, 2);
        f.feed("日本x".as_bytes());
        let s = t.snapshot();
        assert_eq!(s.lines[0].text.trim_end(), "日本x");
        let cells: u16 = s.lines[0].runs.iter().map(|r| r.cells).sum();
        assert_eq!(cells, 10);
    }
}
