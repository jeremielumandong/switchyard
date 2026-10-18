//! The scratchpad: local notes kept as markdown files in the app's data folder.
//!
//! Saved notes are `<data>/notes/<name>.md` (a plain folder of markdown files, so another
//! editor such as Obsidian can open it too). Scratch notes, the unsaved ones, are
//! `<data>/notes/.scratch/<id>.md`: written as they are typed, so they come back after a
//! restart, and deleted only when the user closes them or saves them under a name.
//!
//! All file work runs on one task that handles [`NotesCommand`]s in the order they were
//! sent, so a later autosave never loses to an earlier one.

use std::path::{Path, PathBuf};

use tokio::sync::mpsc;

use crate::bus::{Event, RequestId};
use crate::runtime::EventSender;

/// Folder of the scratch notes, inside the notes folder.
pub const SCRATCH_DIR: &str = ".scratch";
/// Longest note name, in characters.
pub const MAX_NAME_LEN: usize = 120;
/// Notes larger than this are left out of the list (they are not scratch text).
const MAX_NOTE_BYTES: u64 = 4 * 1024 * 1024;

/// A saved note.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    /// Its name: the file name without `.md`.
    pub name: String,
    /// The markdown text.
    pub content: String,
    /// Last change, ms since the epoch.
    pub modified_ms: i64,
}

/// An unsaved scratch note.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scratch {
    /// Its id (the file name without `.md`), chosen by the UI.
    pub id: String,
    /// The text.
    pub content: String,
}

/// A request from the UI ([`crate::Command::Notes`]).
#[derive(Clone, Debug)]
pub enum NotesCommand {
    /// Read the notes folder ([`NotesEvent::Loaded`]).
    Load,
    /// Write scratch note `id` (autosave).
    WriteScratch {
        /// Scratch id.
        id: String,
        /// Its text.
        content: String,
    },
    /// Forget scratch note `id` (the user closed it without saving).
    CloseScratch {
        /// Scratch id.
        id: String,
    },
    /// Write saved note `name` (autosave of its edits).
    WriteNote {
        /// Note name.
        name: String,
        /// Its text.
        content: String,
    },
    /// Create note `name` with `content`; with `scratch`, that scratch note is gone once
    /// it is saved. Refused when a note of that name exists ([`NotesEvent::Saved`]).
    SaveAs {
        /// Request id.
        request: RequestId,
        /// The scratch note being saved, if any.
        scratch: Option<String>,
        /// New note name.
        name: String,
        /// Its text.
        content: String,
    },
    /// Rename note `from` to `to` ([`NotesEvent::Saved`]).
    Rename {
        /// Request id.
        request: RequestId,
        /// Current name.
        from: String,
        /// New name.
        to: String,
    },
    /// Delete saved note `name`.
    Delete {
        /// Note name.
        name: String,
    },
}

/// Answers to [`NotesCommand`]s ([`crate::Event::Notes`]).
#[derive(Clone, Debug)]
pub enum NotesEvent {
    /// The notes folder: saved notes (newest first) and scratch notes (oldest first).
    Loaded {
        /// The notes folder.
        dir: PathBuf,
        /// Saved notes.
        notes: Vec<Note>,
        /// Scratch notes.
        scratch: Vec<Scratch>,
    },
    /// Result of [`NotesCommand::SaveAs`] or [`NotesCommand::Rename`]: the saved note.
    Saved {
        /// Request id.
        request: RequestId,
        /// The note, or why not.
        result: Result<Note, String>,
    },
    /// An autosave, close or delete failed.
    Failed(String),
}

/// Start the notes task on the current runtime; it ends when the sender is dropped.
pub fn spawn(dir: PathBuf, events: EventSender) -> mpsc::UnboundedSender<NotesCommand> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(cmd) = rx.recv().await {
            if let Some(ev) = handle(&dir, cmd).await {
                events.emit(Event::Notes(ev));
            }
        }
    });
    tx
}

/// Carry out one command; the event to send, if any.
pub async fn handle(dir: &Path, cmd: NotesCommand) -> Option<NotesEvent> {
    let failed = |what: &str, e: std::io::Error| Some(NotesEvent::Failed(format!("{what}: {e}")));
    match cmd {
        NotesCommand::Load => Some(match load(dir).await {
            Ok((notes, scratch)) => NotesEvent::Loaded {
                dir: dir.to_path_buf(),
                notes,
                scratch,
            },
            Err(e) => NotesEvent::Failed(format!("Could not read the notes folder: {e}")),
        }),
        NotesCommand::WriteScratch { id, content } => {
            let path = scratch_path(dir, &id)?;
            write(&path, &content)
                .await
                .err()
                .and_then(|e| failed("Could not keep the scratch note", e))
        }
        NotesCommand::CloseScratch { id } => {
            let path = scratch_path(dir, &id)?;
            remove(&path)
                .await
                .err()
                .and_then(|e| failed("Could not remove the scratch note", e))
        }
        NotesCommand::WriteNote { name, content } => {
            let path = note_path(dir, &name).ok()?;
            write(&path, &content)
                .await
                .err()
                .and_then(|e| failed(&format!("Could not save “{name}”"), e))
        }
        NotesCommand::SaveAs {
            request,
            scratch,
            name,
            content,
        } => Some(NotesEvent::Saved {
            request,
            result: save_as(dir, scratch.as_deref(), &name, &content).await,
        }),
        NotesCommand::Rename { request, from, to } => Some(NotesEvent::Saved {
            request,
            result: rename(dir, &from, &to).await,
        }),
        NotesCommand::Delete { name } => {
            let path = note_path(dir, &name).ok()?;
            remove(&path)
                .await
                .err()
                .and_then(|e| failed(&format!("Could not delete “{name}”"), e))
        }
    }
}

/// Check a note name typed by the user; the trimmed name.
pub fn validate_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("A note needs a name".into());
    }
    if name.chars().count() > MAX_NAME_LEN {
        return Err("That name is too long".into());
    }
    if name.starts_with('.') || name.ends_with('.') {
        return Err("A name cannot start or end with a dot".into());
    }
    if let Some(c) = name.chars().find(|c| {
        matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control()
    }) {
        return Err(format!("A name cannot contain {c:?}"));
    }
    Ok(name.to_owned())
}

/// A file name for a note, from its first line (what Save suggests).
pub fn suggest_name(content: &str) -> String {
    let line = content
        .lines()
        .map(|l| l.trim().trim_start_matches('#').trim())
        .find(|l| !l.is_empty())
        .unwrap_or_default();
    let mut name: String = line
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control() {
                ' '
            } else {
                c
            }
        })
        .take(60)
        .collect();
    name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    let name = name.trim_matches('.').trim();
    if name.is_empty() {
        "Untitled".into()
    } else {
        name.to_owned()
    }
}

fn note_path(dir: &Path, name: &str) -> Result<PathBuf, String> {
    Ok(dir.join(format!("{}.md", validate_name(name)?)))
}

/// Scratch ids are the UI's own (letters, digits, `-`); anything else is ignored.
fn scratch_path(dir: &Path, id: &str) -> Option<PathBuf> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    ok.then(|| dir.join(SCRATCH_DIR).join(format!("{id}.md")))
}

async fn write(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    // Write beside it and rename, so a crash mid-write keeps the old text.
    let tmp = path.with_extension("md.tmp");
    tokio::fs::write(&tmp, content).await?;
    tokio::fs::rename(&tmp, path).await
}

async fn remove(path: &Path) -> std::io::Result<()> {
    match tokio::fs::remove_file(path).await {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

fn modified_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as i64)
}

/// The `.md` files of `dir`: (stem, text, modified).
async fn read_md(dir: &Path) -> std::io::Result<Vec<(String, String, i64)>> {
    let mut out = Vec::new();
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(meta) = entry.metadata().await else {
            continue;
        };
        if !meta.is_file() || meta.len() > MAX_NOTE_BYTES {
            continue;
        }
        // Not UTF-8: not a note this editor can show.
        let Ok(text) = tokio::fs::read_to_string(&path).await else {
            continue;
        };
        out.push((stem.to_owned(), text, modified_ms(&meta)));
    }
    Ok(out)
}

async fn load(dir: &Path) -> std::io::Result<(Vec<Note>, Vec<Scratch>)> {
    // Made up front, so "Open folder" has somewhere to go.
    tokio::fs::create_dir_all(dir).await?;
    let mut notes: Vec<Note> = read_md(dir)
        .await?
        .into_iter()
        .filter(|(name, _, _)| !name.starts_with('.'))
        .map(|(name, content, modified_ms)| Note {
            name,
            content,
            modified_ms,
        })
        .collect();
    notes.sort_by(|a, b| {
        b.modified_ms
            .cmp(&a.modified_ms)
            .then_with(|| a.name.cmp(&b.name))
    });
    let mut scratch: Vec<Scratch> = read_md(&dir.join(SCRATCH_DIR))
        .await?
        .into_iter()
        .map(|(id, content, _)| Scratch { id, content })
        .collect();
    // Ids start with the time they were made, so this is the order they were opened in.
    scratch.sort_by(|a, b| a.id.cmp(&b.id));
    Ok((notes, scratch))
}

async fn exists(path: &Path) -> bool {
    tokio::fs::try_exists(path).await.unwrap_or(false)
}

async fn save_as(
    dir: &Path,
    scratch: Option<&str>,
    name: &str,
    content: &str,
) -> Result<Note, String> {
    let name = validate_name(name)?;
    let path = note_path(dir, &name)?;
    if exists(&path).await {
        return Err(format!("A note named “{name}” already exists"));
    }
    write(&path, content).await.map_err(|e| e.to_string())?;
    if let Some(path) = scratch.and_then(|id| scratch_path(dir, id)) {
        remove(&path).await.map_err(|e| e.to_string())?;
    }
    let modified_ms = tokio::fs::metadata(&path)
        .await
        .map(|m| modified_ms(&m))
        .unwrap_or_default();
    Ok(Note {
        name,
        content: content.to_owned(),
        modified_ms,
    })
}

async fn rename(dir: &Path, from: &str, to: &str) -> Result<Note, String> {
    let to = validate_name(to)?;
    let old = note_path(dir, from)?;
    let new = note_path(dir, &to)?;
    // A change of case only is a rename on case-insensitive file systems, not a clash.
    if from.to_lowercase() != to.to_lowercase() && exists(&new).await {
        return Err(format!("A note named “{to}” already exists"));
    }
    tokio::fs::rename(&old, &new)
        .await
        .map_err(|e| e.to_string())?;
    let content = tokio::fs::read_to_string(&new)
        .await
        .map_err(|e| e.to_string())?;
    let modified_ms = tokio::fs::metadata(&new)
        .await
        .map(|m| modified_ms(&m))
        .unwrap_or_default();
    Ok(Note {
        name: to,
        content,
        modified_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn loaded(dir: &Path) -> (Vec<Note>, Vec<Scratch>) {
        match handle(dir, NotesCommand::Load).await {
            Some(NotesEvent::Loaded { notes, scratch, .. }) => (notes, scratch),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn scratch_notes_survive_until_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("notes");
        assert_eq!(loaded(&dir).await, (vec![], vec![]));
        assert!(dir.is_dir());
        for (id, text) in [("200-b", "second"), ("100-a", "first")] {
            let cmd = NotesCommand::WriteScratch {
                id: id.into(),
                content: text.into(),
            };
            assert!(handle(&dir, cmd).await.is_none());
        }
        // A later write of the same note wins.
        let cmd = NotesCommand::WriteScratch {
            id: "100-a".into(),
            content: "first, edited".into(),
        };
        assert!(handle(&dir, cmd).await.is_none());
        let (notes, scratch) = loaded(&dir).await;
        assert!(notes.is_empty(), "scratch notes are not saved notes");
        assert_eq!(
            scratch,
            [
                Scratch {
                    id: "100-a".into(),
                    content: "first, edited".into()
                },
                Scratch {
                    id: "200-b".into(),
                    content: "second".into()
                }
            ]
        );
        let cmd = NotesCommand::CloseScratch { id: "100-a".into() };
        assert!(handle(&dir, cmd).await.is_none());
        assert_eq!(loaded(&dir).await.1.len(), 1);
        // Ids that could leave the folder are ignored.
        let cmd = NotesCommand::WriteScratch {
            id: "../escape".into(),
            content: "x".into(),
        };
        assert!(handle(&dir, cmd).await.is_none());
        assert!(!tmp.path().join("escape.md").exists());
    }

    #[tokio::test]
    async fn save_as_moves_a_scratch_note_into_the_notes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let cmd = NotesCommand::WriteScratch {
            id: "1-x".into(),
            content: "# Deploy steps\nrun it".into(),
        };
        handle(&dir, cmd).await;
        let save = |name: &str| NotesCommand::SaveAs {
            request: 7,
            scratch: Some("1-x".into()),
            name: name.into(),
            content: "# Deploy steps\nrun it".into(),
        };
        let Some(NotesEvent::Saved { request, result }) = handle(&dir, save(" Deploy ")).await
        else {
            panic!("no answer");
        };
        assert_eq!(request, 7);
        assert_eq!(result.unwrap().name, "Deploy");
        assert_eq!(
            std::fs::read_to_string(dir.join("Deploy.md")).unwrap(),
            "# Deploy steps\nrun it"
        );
        let (notes, scratch) = loaded(&dir).await;
        assert_eq!(notes.len(), 1);
        assert!(scratch.is_empty());
        // The same name again is refused, not overwritten.
        let Some(NotesEvent::Saved { result, .. }) = handle(&dir, save("deploy")).await else {
            panic!("no answer");
        };
        if cfg!(any(target_os = "macos", target_os = "windows")) {
            assert!(result.is_err());
        }
        let Some(NotesEvent::Saved { result, .. }) = handle(&dir, save("Deploy")).await else {
            panic!("no answer");
        };
        assert!(result.unwrap_err().contains("already exists"));
    }

    #[tokio::test]
    async fn rename_and_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        for name in ["a", "b"] {
            let cmd = NotesCommand::WriteNote {
                name: name.into(),
                content: format!("text {name}"),
            };
            assert!(handle(&dir, cmd).await.is_none());
        }
        let rename = |to: &str| NotesCommand::Rename {
            request: 1,
            from: "a".into(),
            to: to.into(),
        };
        let Some(NotesEvent::Saved { result, .. }) = handle(&dir, rename("b")).await else {
            panic!("no answer");
        };
        assert!(result.is_err(), "b exists");
        let Some(NotesEvent::Saved { result, .. }) = handle(&dir, rename("c")).await else {
            panic!("no answer");
        };
        assert_eq!(result.unwrap().content, "text a");
        handle(&dir, NotesCommand::Delete { name: "b".into() }).await;
        let names: Vec<String> = loaded(&dir).await.0.into_iter().map(|n| n.name).collect();
        assert_eq!(names, ["c"]);
    }

    #[test]
    fn names() {
        assert_eq!(validate_name("  Ideas "), Ok("Ideas".into()));
        assert!(validate_name("").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name("..").is_err());
        assert!(validate_name(".hidden").is_err());
        assert_eq!(
            suggest_name("\n\n## SELECT * FROM t; -- a/b\nmore"),
            "SELECT FROM t; -- a b"
        );
        assert_eq!(suggest_name("   \n"), "Untitled");
        assert_eq!(suggest_name("..."), "Untitled");
        assert!(validate_name(&suggest_name("what? <now>")).is_ok());
    }
}
