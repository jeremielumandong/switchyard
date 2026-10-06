//! Terminal state on top of `alacritty_terminal`, input encoding and local PTYs.
//!
//! The same [`Terminal`] serves local shells and SSH channels: whoever owns the byte
//! stream feeds output through a [`Feeder`]; the view reads [`Snapshot`]s.

pub mod error;
pub mod input;
pub mod links;
pub mod pty;
pub mod terminal;

pub use error::{Result, TermError};
pub use pty::{LocalShell, PtyInput, spawn_local};
pub use terminal::{
    Attrs, Cursor, CursorShape, DEFAULT_SCROLLBACK, EventSink, Feeder, Mark, Modes, Run, SnapLine,
    Snapshot, TermColor, TermEvent, TermSize, Terminal, new_terminal,
};
