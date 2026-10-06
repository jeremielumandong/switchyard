//! File operations behind the Files explorer: copying between file systems (local ↔ SFTP),
//! recursive delete, and reading / saving text files with a conflict check.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use switchyard_remote::fs::file_name;
use switchyard_remote::{FileEntry, FsError, RemoteFs, SftpFs};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::bus::{OnConflict, SaveError, TextFile, TransferError};

/// Largest file the editor opens.
pub const MAX_EDIT_BYTES: u64 = 5 * 1024 * 1024;
const CHUNK: usize = 256 * 1024;

/// Open SFTP file systems (one per Host) and running transfers.
#[derive(Default)]
pub(crate) struct Files {
    pub(crate) sftp: tokio::sync::Mutex<HashMap<String, Arc<SftpFs>>>,
    cancels: Mutex<HashMap<u64, Arc<AtomicBool>>>,
}

impl Files {
    pub(crate) fn start(&self, id: u64) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        self.cancels
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, flag.clone());
        flag
    }

    pub(crate) fn finish(&self, id: u64) {
        self.cancels
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id);
    }

    pub(crate) fn cancel(&self, id: u64) {
        if let Some(f) = self
            .cancels
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&id)
        {
            f.store(true, Ordering::SeqCst);
        }
    }
}

/// `dir/name` with the separator the target expects (SFTP paths are POSIX).
fn child(dir: &Path, name: &str, posix: bool) -> PathBuf {
    if posix {
        let d = dir.to_string_lossy().replace('\\', "/");
        PathBuf::from(if d.ends_with('/') {
            format!("{d}{name}")
        } else {
            format!("{d}/{name}")
        })
    } else {
        dir.join(name)
    }
}

/// `report (1).pdf`, `report (2).pdf`, …
fn numbered(name: &str, n: u32) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem} ({n}).{ext}"),
        _ => format!("{name} ({n})"),
    }
}

async fn exists(fs: &dyn RemoteFs, path: &Path) -> bool {
    fs.stat(path).await.is_ok()
}

/// Total bytes under `path`.
async fn size_of(fs: &dyn RemoteFs, path: &Path, entry: &FileEntry, posix: bool) -> u64 {
    if !entry.is_dir() {
        return entry.size;
    }
    let mut total = 0;
    let mut stack = vec![path.to_owned()];
    while let Some(dir) = stack.pop() {
        let Ok(list) = fs.list(&dir).await else {
            continue;
        };
        for e in list {
            if e.is_dir() {
                stack.push(child(&dir, &e.name, posix));
            } else {
                total += e.size;
            }
        }
    }
    total
}

struct Copy<'a> {
    src: &'a dyn RemoteFs,
    dst: &'a dyn RemoteFs,
    src_posix: bool,
    dst_posix: bool,
    cancel: &'a AtomicBool,
    done: u64,
    report: &'a (dyn Fn(u64) + Send + Sync),
}

impl Copy<'_> {
    async fn file(&mut self, from: &Path, to: &Path) -> Result<(), TransferError> {
        let fail = |e: FsError| TransferError::Failed(e.to_string());
        let mut r = self.src.open_read(from).await.map_err(fail)?;
        let mut w = self.dst.create(to).await.map_err(fail)?;
        let mut buf = vec![0u8; CHUNK];
        loop {
            if self.cancel.load(Ordering::SeqCst) {
                drop(w);
                // Do not leave half a file behind.
                let _ = self.dst.delete(to).await;
                return Err(TransferError::Cancelled);
            }
            let n = r
                .read(&mut buf)
                .await
                .map_err(|e| TransferError::Failed(e.to_string()))?;
            if n == 0 {
                break;
            }
            w.write_all(&buf[..n])
                .await
                .map_err(|e| TransferError::Failed(e.to_string()))?;
            self.done += n as u64;
            (self.report)(self.done);
        }
        w.shutdown()
            .await
            .map_err(|e| TransferError::Failed(e.to_string()))?;
        Ok(())
    }

    async fn tree(&mut self, from: &Path, to: &Path, is_dir: bool) -> Result<(), TransferError> {
        if !is_dir {
            return self.file(from, to).await;
        }
        if !exists(self.dst, to).await {
            self.dst
                .mkdir(to)
                .await
                .map_err(|e| TransferError::Failed(e.to_string()))?;
        }
        let list = self
            .src
            .list(from)
            .await
            .map_err(|e| TransferError::Failed(e.to_string()))?;
        for e in list {
            // Links that are not directories are copied as the file they point to.
            let (f, t) = (
                child(from, &e.name, self.src_posix),
                child(to, &e.name, self.dst_posix),
            );
            Box::pin(self.tree(&f, &t, e.is_dir())).await?;
        }
        Ok(())
    }
}

/// Copy `path` (file or folder) from `src` into the folder `dir` on `dst`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn transfer(
    src: &dyn RemoteFs,
    src_posix: bool,
    path: &Path,
    dst: &dyn RemoteFs,
    dst_posix: bool,
    dir: &Path,
    on_conflict: OnConflict,
    cancel: &AtomicBool,
    progress: &(dyn Fn(u64, Option<u64>) + Send + Sync),
) -> Result<PathBuf, TransferError> {
    let entry = src
        .stat(path)
        .await
        .map_err(|e| TransferError::Failed(e.to_string()))?;
    let name = file_name(path);
    let mut target = child(dir, &name, dst_posix);
    if exists(dst, &target).await {
        match on_conflict {
            OnConflict::Ask => return Err(TransferError::Exists(name)),
            OnConflict::Replace => {}
            OnConflict::KeepBoth => {
                let mut n = 1;
                while exists(dst, &target).await {
                    target = child(dir, &numbered(&name, n), dst_posix);
                    n += 1;
                }
            }
        }
    }
    let total = size_of(src, path, &entry, src_posix).await;
    progress(0, Some(total));
    let last = std::sync::atomic::AtomicU64::new(0);
    let report = |done: u64| {
        // Every ~1 MB, so the bus is not flooded.
        if done.saturating_sub(last.load(Ordering::Relaxed)) >= 1 << 20 {
            last.store(done, Ordering::Relaxed);
            progress(done, Some(total));
        }
    };
    let mut copy = Copy {
        src,
        dst,
        src_posix,
        dst_posix,
        cancel,
        done: 0,
        report: &report,
    };
    copy.tree(path, &target, entry.is_dir()).await?;
    progress(total, Some(total));
    Ok(target)
}

/// Delete a file, or a folder and everything in it.
pub(crate) async fn delete_tree(
    fs: &dyn RemoteFs,
    path: &Path,
    posix: bool,
) -> Result<(), FsError> {
    let entry = fs.stat(path).await;
    if let Ok(e) = &entry
        && e.is_dir()
    {
        // `stat` follows links: never recurse into a linked folder, just remove the link.
        let is_link = fs
            .list(path.parent().unwrap_or(Path::new("/")))
            .await
            .ok()
            .and_then(|l| l.into_iter().find(|x| x.name == file_name(path)))
            .is_some_and(|x| matches!(x.kind, switchyard_remote::EntryKind::Symlink(_)));
        if !is_link {
            for c in fs.list(path).await? {
                Box::pin(delete_tree(fs, &child(path, &c.name, posix), posix)).await?;
            }
        }
    }
    fs.delete(path).await
}

/// Read a text file for the editor.
pub(crate) async fn read_text(fs: &dyn RemoteFs, path: &Path) -> Result<TextFile, String> {
    let meta = fs.stat(path).await.map_err(|e| e.to_string())?;
    if meta.is_dir() {
        return Err(format!("{} is a folder", meta.name));
    }
    let bytes = fs
        .read_file(path, MAX_EDIT_BYTES)
        .await
        .map_err(|e| e.to_string())?;
    if bytes.contains(&0) {
        return Err(format!("{} looks like a binary file", meta.name));
    }
    let content =
        String::from_utf8(bytes).map_err(|_| format!("{} is not UTF-8 text", meta.name))?;
    Ok(TextFile {
        content,
        modified_ms: meta.modified_ms,
    })
}

/// Save a text file unless it changed since `expect` (SFTP times are whole seconds).
pub(crate) async fn write_text(
    fs: &dyn RemoteFs,
    path: &Path,
    content: &str,
    expect: Option<i64>,
    force: bool,
) -> Result<Option<i64>, SaveError> {
    if !force {
        match fs.stat(path).await {
            Ok(now) if now.modified_ms != expect => {
                return Err(SaveError::Conflict(now.modified_ms));
            }
            Ok(_) => {}
            // Deleted meanwhile: saving recreates it.
            Err(_) => {}
        }
    }
    fs.write_file(path, content.as_bytes())
        .await
        .map_err(|e| SaveError::Failed(e.to_string()))?;
    Ok(fs.stat(path).await.ok().and_then(|m| m.modified_ms))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use switchyard_remote::LocalFs;

    #[test]
    fn names() {
        assert_eq!(numbered("report.pdf", 1), "report (1).pdf");
        assert_eq!(numbered("Makefile", 2), "Makefile (2)");
        assert_eq!(numbered(".bashrc", 1), ".bashrc (1)");
        assert_eq!(
            child(Path::new("/srv/app"), "x", true),
            PathBuf::from("/srv/app/x")
        );
    }

    #[tokio::test]
    async fn copies_folders_with_conflict_policies_and_cancel() {
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("src");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("a.txt"), b"aaa").unwrap();
        std::fs::write(src.join("sub/b.txt"), vec![7u8; 3_000_000]).unwrap();
        let out = t.path().join("out");
        std::fs::create_dir(&out).unwrap();
        let fs = LocalFs;
        let no = AtomicBool::new(false);
        let seen = Mutex::new(Vec::new());
        let progress = |d: u64, t: Option<u64>| seen.lock().unwrap().push((d, t));

        let to = transfer(
            &fs,
            false,
            &src,
            &fs,
            false,
            &out,
            OnConflict::Ask,
            &no,
            &progress,
        )
        .await
        .unwrap();
        assert_eq!(to, out.join("src"));
        assert_eq!(
            std::fs::read(out.join("src/sub/b.txt")).unwrap().len(),
            3_000_000
        );
        assert_eq!(
            seen.lock().unwrap().last(),
            Some(&(3_000_003, Some(3_000_003)))
        );

        let again = transfer(
            &fs,
            false,
            &src,
            &fs,
            false,
            &out,
            OnConflict::Ask,
            &no,
            &progress,
        )
        .await;
        assert_eq!(again, Err(TransferError::Exists("src".into())));
        let both = transfer(
            &fs,
            false,
            &src.join("a.txt"),
            &fs,
            false,
            &out,
            OnConflict::KeepBoth,
            &no,
            &progress,
        )
        .await
        .unwrap();
        assert_eq!(both, out.join("a.txt"));
        let both = transfer(
            &fs,
            false,
            &src.join("a.txt"),
            &fs,
            false,
            &out,
            OnConflict::KeepBoth,
            &no,
            &progress,
        )
        .await
        .unwrap();
        assert_eq!(both, out.join("a (1).txt"));

        let stop = AtomicBool::new(true);
        let r = transfer(
            &fs,
            false,
            &src.join("sub/b.txt"),
            &fs,
            false,
            &t.path().join("out/src"),
            OnConflict::Replace,
            &stop,
            &progress,
        )
        .await;
        assert_eq!(r, Err(TransferError::Cancelled));
        assert!(!out.join("src/b.txt").exists(), "partial file removed");

        delete_tree(&fs, &out.join("src"), false).await.unwrap();
        assert!(!out.join("src").exists());
    }

    #[tokio::test]
    async fn save_detects_changes_made_elsewhere() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("app.conf");
        std::fs::write(&f, "port=1\n").unwrap();
        let fs = LocalFs;
        let opened = read_text(&fs, &f).await.unwrap();
        assert_eq!(opened.content, "port=1\n");
        let saved = write_text(&fs, &f, "port=2\n", opened.modified_ms, false)
            .await
            .unwrap();
        // Someone else edits it (a different mtime).
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&f, "port=3\n").unwrap();
        let r = write_text(&fs, &f, "port=4\n", saved, false).await;
        assert!(matches!(r, Err(SaveError::Conflict(_))), "{r:?}");
        write_text(&fs, &f, "port=4\n", saved, true).await.unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "port=4\n");

        std::fs::write(t.path().join("bin"), [0u8, 1, 2]).unwrap();
        assert!(
            read_text(&fs, &t.path().join("bin"))
                .await
                .unwrap_err()
                .contains("binary")
        );
    }
}
