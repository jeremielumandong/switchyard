//! Open terminals: where each one's input goes. Output is parsed on the I/O side straight
//! into the [`Terminal`] the UI holds; the UI is only woken (coalesced) to redraw.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use switchyard_term::{
    EventSink, LocalShell, PtyInput, TermEvent, TermSize, Terminal, spawn_local,
};

use crate::bus::{Event, TermId};
use crate::runtime::EventSender;

/// Input for a terminal's program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TermInput {
    /// Bytes (keys, pastes, replies to queries).
    Data(Vec<u8>),
    /// Window size changed.
    Resize(TermSize),
    /// Close the terminal.
    Close,
}

/// Delivers input to a terminal's program (PTY writer thread or SSH channel task).
pub type InputFn = Arc<dyn Fn(TermInput) + Send + Sync>;

/// Registry of open terminals.
#[derive(Default)]
pub struct Terminals {
    inputs: Mutex<HashMap<TermId, InputFn>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The sink that turns terminal events into bus events and answers terminal queries.
pub fn event_sink(term: TermId, events: EventSender, input: InputFn) -> EventSink {
    Arc::new(move |ev| match ev {
        TermEvent::Wakeup => events.emit(Event::TerminalWake { term }),
        TermEvent::Reply(bytes) => input(TermInput::Data(bytes)),
        TermEvent::Title(title) => events.emit(Event::TerminalTitle { term, title }),
        TermEvent::ResetTitle => events.emit(Event::TerminalTitle {
            term,
            title: String::new(),
        }),
        TermEvent::Bell => events.emit(Event::TerminalBell { term }),
        TermEvent::Clipboard(text) => events.emit(Event::TerminalClipboard { term, text }),
    })
}

impl Terminals {
    /// Register a terminal's input.
    pub fn insert(&self, term: TermId, input: InputFn) {
        lock(&self.inputs).insert(term, input);
    }

    /// Send input; ignored for unknown (already closed) terminals.
    pub fn send(&self, term: TermId, msg: TermInput) {
        let input = lock(&self.inputs).get(&term).cloned();
        if let Some(f) = input {
            f(msg);
        }
    }

    /// Close and forget a terminal.
    pub fn close(&self, term: TermId) {
        if let Some(f) = lock(&self.inputs).remove(&term) {
            f(TermInput::Close);
        }
    }

    /// Forget a terminal whose program ended.
    pub fn remove(&self, term: TermId) {
        lock(&self.inputs).remove(&term);
    }

    /// Start a local shell. Returns the terminal for the UI.
    pub fn open_local(
        self: &Arc<Self>,
        term: TermId,
        shell: LocalShell,
        size: TermSize,
        scrollback: usize,
        events: EventSender,
    ) -> switchyard_term::Result<Terminal> {
        let (tx, rx) = std::sync::mpsc::channel::<PtyInput>();
        let input: InputFn = Arc::new(move |msg| {
            let _ = tx.send(match msg {
                TermInput::Data(b) => PtyInput::Data(b),
                TermInput::Resize(s) => PtyInput::Resize(s),
                TermInput::Close => PtyInput::Close,
            });
        });
        let sink = event_sink(term, events.clone(), input.clone());
        let (terminal, feeder) = switchyard_term::new_terminal(size, scrollback, sink);
        let registry = self.clone();
        spawn_local(
            &shell,
            size,
            feeder,
            rx,
            Box::new(move |code| {
                registry.remove(term);
                events.emit(Event::TerminalExited {
                    term,
                    code,
                    message: None,
                });
            }),
        )?;
        self.insert(term, input);
        Ok(terminal)
    }
}
