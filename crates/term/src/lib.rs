//! Terminal state and local PTY (milestone M2).
//!
//! This crate will wrap `alacritty_terminal`'s `Term` (byte feed, resize, scrollback) and
//! spawn local shells through `portable-pty`. SSH channel bytes are fed through the ANSI
//! parser; the local tty module is only used for local shells.

/// Default scrollback length in lines.
pub const DEFAULT_SCROLLBACK: usize = 10_000;
