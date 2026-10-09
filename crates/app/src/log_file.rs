//! The app's log file: `<data>/logs/switchyard.log`, rotated by size, written on its own
//! thread. Release builds on Windows have no console, so this is where their logs go.
//!
//! Callers hand formatted lines to a bounded channel and never wait on the disk (the UI
//! thread logs too). Lines are dropped, not blocked on, if the writer falls behind.
//! What goes in is whatever `tracing` events say: spans and events never carry
//! credentials (CLAUDE.md), so nothing is scrubbed here.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

use tracing_subscriber::fmt::MakeWriter;

/// The current log file's name.
pub const FILE_NAME: &str = "switchyard.log";
/// Rotate when the current file would grow past this.
pub const MAX_BYTES: u64 = 5 * 1024 * 1024;
/// Rotated files kept beside the current one (`switchyard.log.1` is the newest).
pub const KEEP: usize = 3;
/// Lines buffered between the app and the writer thread.
const QUEUE: usize = 4096;

/// A size-capped log file with numbered older copies.
struct Rotating {
    dir: PathBuf,
    max_bytes: u64,
    keep: usize,
    file: Option<File>,
    size: u64,
}

impl Rotating {
    fn open(dir: &Path, max_bytes: u64, keep: usize) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let mut this = Self {
            dir: dir.to_path_buf(),
            max_bytes,
            keep,
            file: None,
            size: 0,
        };
        this.reopen()?;
        Ok(this)
    }

    fn path(&self, n: usize) -> PathBuf {
        if n == 0 {
            self.dir.join(FILE_NAME)
        } else {
            self.dir.join(format!("{FILE_NAME}.{n}"))
        }
    }

    fn reopen(&mut self) -> io::Result<()> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path(0))?;
        self.size = file.metadata()?.len();
        self.file = Some(file);
        Ok(())
    }

    /// `switchyard.log` → `.1` → … → `.keep`; the oldest is deleted.
    fn rotate(&mut self) -> io::Result<()> {
        self.file = None;
        let oldest = self.path(self.keep);
        if oldest.exists() {
            std::fs::remove_file(&oldest)?;
        }
        for n in (0..self.keep).rev() {
            let from = self.path(n);
            if from.exists() {
                std::fs::rename(&from, self.path(n + 1))?;
            }
        }
        self.reopen()
    }

    fn write_line(&mut self, line: &[u8]) -> io::Result<()> {
        if self.size > 0 && self.size + line.len() as u64 > self.max_bytes {
            self.rotate()?;
        }
        if self.file.is_none() {
            self.reopen()?;
        }
        if let Some(file) = self.file.as_mut() {
            file.write_all(line)?;
            self.size += line.len() as u64;
        }
        Ok(())
    }
}

fn run_writer(mut log: Rotating, lines: Receiver<Vec<u8>>) {
    let mut failed = false;
    for line in lines {
        if let Err(e) = log.write_line(&line) {
            if !failed {
                // Logging about logging would loop; say it once on stderr.
                eprintln!("switchyard: writing the log file failed: {e}");
                failed = true;
            }
            log.file = None;
        }
    }
}

/// A [`MakeWriter`] for `tracing-subscriber` that sends each event to the log thread.
#[derive(Clone)]
pub struct LogFile {
    tx: SyncSender<Vec<u8>>,
}

impl LogFile {
    /// Open (or create) the log in `dir` and start its writer thread.
    pub fn start(dir: &Path) -> io::Result<Self> {
        Self::start_with(dir, MAX_BYTES, KEEP)
    }

    fn start_with(dir: &Path, max_bytes: u64, keep: usize) -> io::Result<Self> {
        let log = Rotating::open(dir, max_bytes, keep)?;
        let (tx, rx) = sync_channel(QUEUE);
        std::thread::Builder::new()
            .name("switchyard-log".into())
            .spawn(move || run_writer(log, rx))?;
        Ok(Self { tx })
    }
}

/// One event's bytes, sent when `tracing` drops the writer.
pub struct EventWriter {
    tx: SyncSender<Vec<u8>>,
    buf: Vec<u8>,
}

impl Write for EventWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for EventWriter {
    fn drop(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        // Full: the writer is behind, drop the line rather than block the caller.
        // Disconnected: the writer thread is gone; nothing to do.
        let _ = self.tx.try_send(std::mem::take(&mut self.buf));
    }
}

impl<'a> MakeWriter<'a> for LogFile {
    type Writer = EventWriter;

    fn make_writer(&'a self) -> Self::Writer {
        EventWriter {
            tx: self.tx.clone(),
            buf: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn rotates_by_size_and_keeps_a_few() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Rotating::open(dir.path(), 100, 2).unwrap();
        let line = [b'x'; 39];
        // 39 bytes a line: two fit in 100, the third rotates.
        for _ in 0..9 {
            log.write_line(&line).unwrap();
        }
        let size = |n: &str| std::fs::metadata(dir.path().join(n)).unwrap().len();
        assert_eq!(size(FILE_NAME), 39);
        assert_eq!(size("switchyard.log.1"), 78);
        assert_eq!(size("switchyard.log.2"), 78);
        assert!(!dir.path().join("switchyard.log.3").exists());
    }

    #[test]
    fn appends_to_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE_NAME), b"old\n").unwrap();
        let mut log = Rotating::open(dir.path(), 1000, 2).unwrap();
        log.write_line(b"new\n").unwrap();
        let text = std::fs::read_to_string(dir.path().join(FILE_NAME)).unwrap();
        assert_eq!(text, "old\nnew\n");
    }

    #[test]
    fn events_reach_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = LogFile::start_with(dir.path(), MAX_BYTES, KEEP).unwrap();
        {
            let mut w = file.make_writer();
            w.write_all(b"hello ").unwrap();
            w.write_all(b"log\n").unwrap();
        }
        let path = dir.path().join(FILE_NAME);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::fs::read_to_string(&path).unwrap_or_default() != "hello log\n" {
            assert!(std::time::Instant::now() < deadline, "line never written");
            std::thread::yield_now();
        }
    }
}
