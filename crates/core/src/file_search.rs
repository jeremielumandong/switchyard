//! Finding files by name under a folder, for the Files explorer's search.
//!
//! On this computer a search first asks the platform's file index when one runs: on
//! macOS the fsearch daemon (a whole-disk index kept live from FSEvents: answers in well
//! under a millisecond), on Windows Everything (voidtools, through its `es` command).
//! Without one, or while it is still indexing, the walk is `ignore`'s parallel walker
//! (ripgrep's), which is also the fastest portable way on Linux: every core reads
//! folders at once, and folders a `.gitignore` excludes (`node_modules`, `target`, build
//! output) are skipped unless the search asks for everything. On a Host, an FTP server or
//! object storage the walk lists folders over the connection a few at a time, skipping
//! the usual heavy folders. Either way hits stream back in small batches, the search
//! stops at [`MAX_HITS`], and it can be cancelled.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use futures::StreamExt as _;
use futures::stream::FuturesUnordered;
use switchyard_remote::fs::local_entry;
use switchyard_remote::{EntryKind, FileEntry, RemoteFs};
use tokio::sync::mpsc::UnboundedSender;

use crate::files::{CANCEL, child};

/// Most hits one search returns.
pub const MAX_HITS: usize = 5_000;
/// Most folders a remote search lists.
pub const MAX_REMOTE_DIRS: usize = 20_000;
/// Folders listed at once on a remote file system.
const REMOTE_PARALLEL: usize = 6;
/// Folders a remote search does not enter (a local one follows `.gitignore` instead).
const HEAVY_DIRS: [&str; 8] = [
    "node_modules",
    ".git",
    "target",
    "__pycache__",
    ".venv",
    ".cache",
    ".gradle",
    ".terraform",
];

/// One match: the folder it is in and its entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileHit {
    /// The folder holding it.
    pub dir: PathBuf,
    /// Name, kind, size, time.
    pub entry: FileEntry,
}

/// What a search finds: name words (forgiving a typo), a `*` / `?` pattern, and filters.
///
/// The syntax follows fsearch (MIT, github.com/noahdunnagan/fsearch), a macOS-only
/// whole-disk search: words must all appear in the name and forgive one typo (two in
/// words of 8 letters or more), so `mian` finds `main.rs`; `'word` is exact; `ext:rs`,
/// `type:image`, `size:>5mb` and `mtime:<7d` (changed within 7 days) narrow it down.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NameQuery {
    words: Vec<Word>,
    globs: Vec<Vec<char>>,
    exts: Vec<String>,
    kind: Option<TypeFilter>,
    size: Option<(Cmp, u64)>,
    /// Compared with the modification time, in ms since the epoch.
    mtime: Option<(Cmp, i64)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Word {
    text: Vec<char>,
    exact: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cmp {
    Less,
    More,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TypeFilter {
    Folder,
    File,
    Exts(&'static [&'static str]),
}

const IMAGES: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "tif", "tiff", "svg", "heic",
];
const VIDEOS: &[&str] = &[
    "mp4", "mov", "mkv", "avi", "webm", "wmv", "m4v", "mpg", "mpeg",
];
const AUDIO: &[&str] = &["mp3", "wav", "flac", "aac", "ogg", "m4a", "opus", "aiff"];
const ARCHIVES: &[&str] = &[
    "zip", "tar", "gz", "tgz", "bz2", "xz", "zst", "7z", "rar", "jar", "iso", "dmg",
];
const DOCS: &[&str] = &[
    "pdf", "doc", "docx", "odt", "rtf", "pages", "xls", "xlsx", "ods", "csv", "ppt", "pptx", "odp",
    "key", "md", "txt",
];
const CODE: &[&str] = &[
    "rs", "go", "py", "rb", "js", "ts", "tsx", "jsx", "java", "kt", "c", "h", "cc", "cpp", "hpp",
    "cs", "swift", "php", "sql", "sh", "ps1", "html", "css", "scss", "vue", "svelte", "lua", "zig",
];

impl NameQuery {
    /// `None` for blank text.
    pub fn new(text: &str) -> Option<Self> {
        Self::at(text, now_ms())
    }

    fn at(text: &str, now: i64) -> Option<Self> {
        let mut q = NameQuery {
            words: Vec::new(),
            globs: Vec::new(),
            exts: Vec::new(),
            kind: None,
            size: None,
            mtime: None,
        };
        for token in text.split_whitespace() {
            let lower = token.to_lowercase();
            if let Some(v) = lower.strip_prefix("ext:") {
                q.exts.extend(
                    v.split(',')
                        .map(|e| e.trim_start_matches('.').to_owned())
                        .filter(|e| !e.is_empty()),
                );
            } else if let Some(v) = lower
                .strip_prefix("type:")
                .or_else(|| lower.strip_prefix("kind:"))
            {
                q.kind = type_filter(v);
            } else if let Some((cmp, n)) = lower.strip_prefix("size:").and_then(parse_size) {
                q.size = Some((cmp, n));
            } else if let Some((cmp, age)) = lower.strip_prefix("mtime:").and_then(parse_age) {
                // "Newer than 7 days" is a time after now - 7 days.
                q.mtime = Some((
                    if cmp == Cmp::Less {
                        Cmp::More
                    } else {
                        Cmp::Less
                    },
                    now - age,
                ));
            } else if lower.contains(['*', '?']) {
                q.globs.push(lower.chars().collect());
            } else if let Some(w) = lower.strip_prefix('\'') {
                if !w.is_empty() {
                    q.words.push(Word {
                        text: w.chars().collect(),
                        exact: true,
                    });
                }
            } else {
                q.words.push(Word {
                    text: lower.chars().collect(),
                    exact: false,
                });
            }
        }
        let empty = q.words.is_empty()
            && q.globs.is_empty()
            && q.exts.is_empty()
            && q.kind.is_none()
            && q.size.is_none()
            && q.mtime.is_none();
        (!empty).then_some(q)
    }

    /// Whether the name alone can match (checked before reading the file's metadata).
    pub fn matches_name(&self, name: &str) -> bool {
        let lower: Vec<char> = name.to_lowercase().chars().collect();
        self.words.iter().all(|w| {
            contains(&lower, &w.text) || (!w.exact && near(&lower, &w.text, typos(w.text.len())))
        }) && self.globs.iter().all(|g| glob(g, &lower))
    }

    /// Whether `entry` matches the whole query.
    pub fn matches(&self, entry: &FileEntry) -> bool {
        if !self.matches_name(&entry.name) {
            return false;
        }
        let dir = entry.is_dir();
        let ext = entry
            .name
            .rsplit_once('.')
            .filter(|(stem, _)| !stem.is_empty())
            .map(|(_, e)| e.to_lowercase());
        if !self.exts.is_empty() && (dir || !ext.as_ref().is_some_and(|e| self.exts.contains(e))) {
            return false;
        }
        let kind_ok = match self.kind {
            None => true,
            Some(TypeFilter::Folder) => dir,
            Some(TypeFilter::File) => !dir,
            Some(TypeFilter::Exts(list)) => {
                !dir && ext.as_deref().is_some_and(|e| list.contains(&e))
            }
        };
        let size_ok = match self.size {
            None => true,
            Some(_) if dir => false,
            Some((Cmp::Less, n)) => entry.size < n,
            Some((Cmp::More, n)) => entry.size > n,
        };
        let time_ok = match (self.mtime, entry.modified_ms) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some((Cmp::Less, t)), Some(m)) => m < t,
            (Some((Cmp::More, t)), Some(m)) => m > t,
        };
        kind_ok && size_ok && time_ok
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn type_filter(v: &str) -> Option<TypeFilter> {
    Some(match v {
        "folder" | "dir" | "directory" => TypeFilter::Folder,
        "file" => TypeFilter::File,
        "image" | "picture" | "photo" => TypeFilter::Exts(IMAGES),
        "video" | "movie" => TypeFilter::Exts(VIDEOS),
        "audio" | "music" | "sound" => TypeFilter::Exts(AUDIO),
        "archive" | "zip" => TypeFilter::Exts(ARCHIVES),
        "doc" | "document" | "docs" => TypeFilter::Exts(DOCS),
        "code" | "source" => TypeFilter::Exts(CODE),
        _ => return None,
    })
}

/// `>5mb`, `<100k`, `>=1g` (bytes without a unit).
fn parse_size(v: &str) -> Option<(Cmp, u64)> {
    let (cmp, rest) = cmp_of(v)?;
    let digits = rest.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let unit = &rest[digits.len()..];
    let n: f64 = digits.parse().ok()?;
    let mult = match unit {
        "" | "b" => 1.0,
        "k" | "kb" => 1024.0,
        "m" | "mb" => 1024.0 * 1024.0,
        "g" | "gb" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((cmp, (n * mult) as u64))
}

/// `<7d`, `>2w`, `<3h` as an age in ms.
fn parse_age(v: &str) -> Option<(Cmp, i64)> {
    let (cmp, rest) = cmp_of(v)?;
    let digits = rest.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let n: f64 = digits.parse().ok()?;
    let ms = match &rest[digits.len()..] {
        "m" | "min" => 60_000.0,
        "h" => 3_600_000.0,
        "" | "d" => 86_400_000.0,
        "w" => 7.0 * 86_400_000.0,
        "y" => 365.0 * 86_400_000.0,
        _ => return None,
    };
    Some((cmp, (n * ms) as i64))
}

fn cmp_of(v: &str) -> Option<(Cmp, &str)> {
    let v = v.trim_start_matches('=');
    if let Some(r) = v.strip_prefix(">=").or_else(|| v.strip_prefix('>')) {
        Some((Cmp::More, r))
    } else if let Some(r) = v.strip_prefix("<=").or_else(|| v.strip_prefix('<')) {
        Some((Cmp::Less, r))
    } else {
        None
    }
}

/// Typos a word forgives: none below 4 letters, two from 8.
fn typos(len: usize) -> usize {
    match len {
        0..4 => 0,
        4..8 => 1,
        _ => 2,
    }
}

fn contains(hay: &[char], needle: &[char]) -> bool {
    needle.is_empty() || hay.windows(needle.len()).any(|w| w == needle)
}

/// Whether `needle` appears in `hay` with at most `k` edits, a swap of two neighbours
/// counting as one (Sellers' algorithm with transpositions).
fn near(hay: &[char], needle: &[char], k: usize) -> bool {
    if k == 0 || needle.len() > hay.len() + k {
        return false;
    }
    let m = needle.len();
    // Columns for the previous two characters of `hay`; row 0 is 0: a match may start
    // anywhere in the name.
    let mut prev2: Vec<usize> = (0..=m).collect();
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut cur = vec![0; m + 1];
    for j in 0..hay.len() {
        cur[0] = 0;
        for i in 1..=m {
            let mut d = (prev[i - 1] + usize::from(needle[i - 1] != hay[j]))
                .min(prev[i] + 1)
                .min(cur[i - 1] + 1);
            if i > 1 && j > 0 && needle[i - 1] == hay[j - 1] && needle[i - 2] == hay[j] {
                d = d.min(prev2[i - 2] + 1);
            }
            cur[i] = d;
        }
        if cur[m] <= k {
            return true;
        }
        std::mem::swap(&mut prev2, &mut prev);
        std::mem::swap(&mut prev, &mut cur);
    }
    false
}

/// `*` any run, `?` one character; the whole name must match.
fn glob(pattern: &[char], name: &[char]) -> bool {
    let (mut p, mut n) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some((p, n));
                p += 1;
            }
            Some(c) if *c == '?' || *c == name[n] => {
                p += 1;
                n += 1;
            }
            _ => match star {
                Some((sp, sn)) => {
                    p = sp + 1;
                    n = sn + 1;
                    star = Some((sp, sn + 1));
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|c| *c == '*')
}

/// Whether the search was cancelled.
fn stopped(stop: &AtomicU8) -> bool {
    stop.load(Ordering::SeqCst) == CANCEL
}

/// Search `root` on this computer, sending hits to `tx`. Blocks: run it off the async
/// runtime. `true` when it stopped at [`MAX_HITS`].
pub fn search_local(
    root: &Path,
    query: &NameQuery,
    everything: bool,
    stop: &AtomicU8,
    tx: &UnboundedSender<FileHit>,
) -> bool {
    let found = AtomicUsize::new(0);
    let walk = ignore::WalkBuilder::new(root)
        .hidden(!everything)
        .git_ignore(!everything)
        .git_global(!everything)
        .git_exclude(!everything)
        .ignore(!everything)
        .parents(!everything)
        .follow_links(false)
        .threads(0)
        .build_parallel();
    walk.run(|| {
        let found = &found;
        Box::new(move |res| {
            use ignore::WalkState;
            if stopped(stop) || found.load(Ordering::Relaxed) >= MAX_HITS {
                return WalkState::Quit;
            }
            let Ok(e) = res else {
                return WalkState::Continue;
            };
            if e.depth() == 0 {
                return WalkState::Continue;
            }
            let name = e.file_name().to_string_lossy();
            if !query.matches_name(&name) {
                return WalkState::Continue;
            }
            let path = e.path();
            let Ok(meta) = std::fs::symlink_metadata(path) else {
                return WalkState::Continue;
            };
            let kind = if meta.file_type().is_symlink() {
                let target = std::fs::read_link(path)
                    .map(|t| t.to_string_lossy().into_owned())
                    .unwrap_or_default();
                match std::fs::metadata(path) {
                    Ok(m) if m.is_dir() => EntryKind::Dir,
                    _ => EntryKind::Symlink(target),
                }
            } else if meta.is_dir() {
                EntryKind::Dir
            } else {
                EntryKind::File
            };
            let hit = FileHit {
                dir: path.parent().map(Path::to_path_buf).unwrap_or_default(),
                entry: local_entry(path, &meta, kind),
            };
            if !query.matches(&hit.entry) {
                return WalkState::Continue;
            }
            found.fetch_add(1, Ordering::Relaxed);
            if tx.send(hit).is_err() {
                return WalkState::Quit;
            }
            WalkState::Continue
        })
    });
    found.load(Ordering::Relaxed) >= MAX_HITS
}

/// Search `root` on a remote file system, a few folders at a time. `true` when it stopped
/// at [`MAX_HITS`] or [`MAX_REMOTE_DIRS`].
pub async fn search_remote(
    fs: Arc<dyn RemoteFs>,
    root: PathBuf,
    query: &NameQuery,
    everything: bool,
    posix: bool,
    stop: &AtomicU8,
    tx: &UnboundedSender<FileHit>,
) -> bool {
    let mut queue = VecDeque::from([root]);
    let mut running = FuturesUnordered::new();
    let (mut listed, mut found) = (0usize, 0usize);
    loop {
        while running.len() < REMOTE_PARALLEL
            && let Some(dir) = queue.pop_front()
        {
            listed += 1;
            let fs = fs.clone();
            running.push(async move {
                let r = fs.list(&dir).await;
                (dir, r)
            });
        }
        let Some((dir, result)) = running.next().await else {
            return false;
        };
        if stopped(stop) {
            return false;
        }
        let Ok(entries) = result else { continue };
        for e in entries {
            let hidden = e.name.starts_with('.');
            if e.is_dir()
                && !matches!(e.kind, EntryKind::Symlink(_))
                && (everything || (!hidden && !HEAVY_DIRS.contains(&e.name.as_str())))
            {
                queue.push_back(child(&dir, &e.name, posix));
            }
            if (everything || !hidden) && query.matches(&e) {
                found += 1;
                if tx
                    .send(FileHit {
                        dir: dir.clone(),
                        entry: e,
                    })
                    .is_err()
                {
                    return false;
                }
                if found >= MAX_HITS {
                    return true;
                }
            }
        }
        if listed >= MAX_REMOTE_DIRS && !queue.is_empty() {
            return true;
        }
    }
}

// ------------------------------------------------------------------ file indexes

/// What answered a search.
pub const ENGINE_WALK: &str = "folder walk";
/// The fsearch daemon (macOS).
pub const ENGINE_FSEARCH: &str = "fsearch";
/// Everything (Windows).
pub const ENGINE_EVERYTHING: &str = "Everything";

/// How long an index may take before the walk runs instead.
const INDEX_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Ask the platform's file index for `text` under `root`. `None` when there is none,
/// it is still indexing, or it found nothing (the walk then runs: it forgives typos the
/// index may not). `Some(truncated)` once the hits are sent.
pub async fn search_index(
    root: &Path,
    text: &str,
    query: &NameQuery,
    everything: bool,
    tx: &UnboundedSender<FileHit>,
) -> Option<(bool, &'static str)> {
    if cfg!(target_os = "macos") {
        let hits = fsearch::search(root, text).await?;
        return send_index_hits(root, hits, query, everything, tx).map(|t| (t, ENGINE_FSEARCH));
    }
    if cfg!(windows) {
        let paths = everything::search(root, query).await?;
        let hits = tokio::task::spawn_blocking(move || {
            paths
                .into_iter()
                .filter_map(|p| {
                    let meta = std::fs::symlink_metadata(&p).ok()?;
                    let kind = if meta.is_dir() {
                        EntryKind::Dir
                    } else {
                        EntryKind::File
                    };
                    Some(FileHit {
                        dir: p.parent()?.to_path_buf(),
                        entry: local_entry(&p, &meta, kind),
                    })
                })
                .collect::<Vec<_>>()
        })
        .await
        .ok()?;
        return send_index_hits(root, hits, query, everything, tx).map(|t| (t, ENGINE_EVERYTHING));
    }
    None
}

/// Send an index's hits that the query and the hidden-file rule keep; `None` if none.
fn send_index_hits(
    root: &Path,
    hits: Vec<FileHit>,
    query: &NameQuery,
    everything: bool,
    tx: &UnboundedSender<FileHit>,
) -> Option<bool> {
    let kept: Vec<FileHit> = hits
        .into_iter()
        .filter(|h| everything || !hidden_below(root, &h.dir, &h.entry.name))
        .filter(|h| query.matches(&h.entry))
        .take(MAX_HITS)
        .collect();
    if kept.is_empty() {
        return None;
    }
    let truncated = kept.len() >= MAX_HITS;
    for h in kept {
        tx.send(h).ok()?;
    }
    Some(truncated)
}

/// Whether `dir/name` is hidden, or inside a hidden folder, below `root`.
fn hidden_below(root: &Path, dir: &Path, name: &str) -> bool {
    name.starts_with('.')
        || dir
            .strip_prefix(root)
            .map(|rel| {
                rel.components()
                    .any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
            })
            .unwrap_or(false)
}

/// The first of `names` on `PATH` or in `extra` folders.
fn find_program(names: &[&str], extra: &[PathBuf]) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(extra.iter().cloned())
        .flat_map(|d| names.iter().map(move |n| d.join(n)))
        .find(|p| p.is_file())
}

/// The fsearch daemon (github.com/noahdunnagan/fsearch, MIT): JSON lines over its
/// socket, or through `fsearch stdio`, which starts the daemon when it is not running.
pub mod fsearch {
    use std::path::{Path, PathBuf};

    use serde::Deserialize;
    use switchyard_remote::{EntryKind, FileEntry};
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

    use super::{FileHit, INDEX_TIMEOUT, MAX_HITS, find_program};

    /// One answer line.
    #[derive(Debug, Deserialize)]
    pub(crate) struct Reply {
        #[serde(default)]
        ok: bool,
        #[serde(default)]
        hits: Vec<Hit>,
    }

    #[derive(Debug, Deserialize)]
    struct Hit {
        path: String,
        #[serde(default)]
        kind: String,
        #[serde(default)]
        size: u64,
        /// Seconds since the epoch.
        #[serde(default)]
        mtime: u32,
    }

    /// The request line for `text` under `root` (`in:` scopes it; quotes keep spaces).
    pub(crate) fn request(root: &Path, text: &str) -> String {
        let q = format!("{} in:\"{}\"", text.trim(), root.to_string_lossy());
        serde_json::json!({ "q": q, "limit": MAX_HITS }).to_string() + "\n"
    }

    /// Hits of an answer line; `None` when it is not ok (still indexing, an error).
    pub(crate) fn parse(line: &str) -> Option<Vec<FileHit>> {
        let r: Reply = serde_json::from_str(line).ok()?;
        if !r.ok {
            return None;
        }
        Some(
            r.hits
                .into_iter()
                .filter_map(|h| {
                    let path = PathBuf::from(&h.path);
                    let kind = match h.kind.as_str() {
                        "dir" => EntryKind::Dir,
                        "link" => EntryKind::Symlink(String::new()),
                        _ => EntryKind::File,
                    };
                    Some(FileHit {
                        dir: path.parent()?.to_path_buf(),
                        entry: FileEntry {
                            name: path.file_name()?.to_string_lossy().into_owned(),
                            kind,
                            size: h.size,
                            modified_ms: (h.mtime > 0).then(|| i64::from(h.mtime) * 1000),
                            mode: None,
                        },
                    })
                })
                .collect(),
        )
    }

    fn home() -> Option<PathBuf> {
        std::env::var_os("HOME").map(PathBuf::from)
    }

    /// The daemon's socket.
    pub fn socket() -> Option<PathBuf> {
        Some(home()?.join("Library/Application Support/FSearch/fsearch.sock"))
    }

    /// Ask the daemon at `sock`.
    #[cfg(unix)]
    pub(crate) async fn ask_socket(sock: &Path, line: &str) -> Option<String> {
        let stream = tokio::net::UnixStream::connect(sock).await.ok()?;
        let (r, mut w) = stream.into_split();
        w.write_all(line.as_bytes()).await.ok()?;
        let mut out = String::new();
        BufReader::new(r).read_line(&mut out).await.ok()?;
        Some(out)
    }

    /// Ask through `fsearch stdio` (starts the daemon).
    async fn ask_cli(line: &str) -> Option<String> {
        let extra: Vec<PathBuf> = [".local/bin", ".cargo/bin"]
            .iter()
            .filter_map(|d| Some(home()?.join(d)))
            .chain([
                PathBuf::from("/opt/homebrew/bin"),
                PathBuf::from("/usr/local/bin"),
            ])
            .collect();
        let exe = find_program(&["fsearch"], &extra)?;
        let mut child = tokio::process::Command::new(exe)
            .arg("stdio")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .ok()?;
        let mut stdin = child.stdin.take()?;
        stdin.write_all(line.as_bytes()).await.ok()?;
        drop(stdin);
        let mut out = String::new();
        BufReader::new(child.stdout.take()?)
            .read_line(&mut out)
            .await
            .ok()?;
        Some(out)
    }

    /// Search through the daemon, if it is installed and ready.
    pub async fn search(root: &Path, text: &str) -> Option<Vec<FileHit>> {
        let line = request(root, text);
        let ask = async {
            #[cfg(unix)]
            if let Some(sock) = socket().filter(|s| s.exists())
                && let Some(answer) = ask_socket(&sock, &line).await
            {
                return Some(answer);
            }
            ask_cli(&line).await
        };
        let answer = tokio::time::timeout(INDEX_TIMEOUT, ask).await.ok()??;
        parse(&answer)
    }
}

/// Everything (voidtools, Windows): its `es` command lists matching paths.
pub mod everything {
    use std::path::{Path, PathBuf};

    use super::{INDEX_TIMEOUT, MAX_HITS, NameQuery, find_program};

    /// `es` arguments for `query` under `root`: the plain words and patterns (Everything
    /// matches them exactly; filters are applied to the results here).
    pub(crate) fn args(root: &Path, query: &NameQuery) -> Option<Vec<String>> {
        let mut terms: Vec<String> = query
            .words
            .iter()
            .map(|w| w.text.iter().collect::<String>())
            .collect();
        terms.extend(query.globs.iter().map(|g| g.iter().collect::<String>()));
        if !query.exts.is_empty() {
            terms.push(format!("ext:{}", query.exts.join(";")));
        }
        if query.words.is_empty() && query.globs.is_empty() {
            return None;
        }
        let mut a = vec![
            "-n".to_owned(),
            MAX_HITS.to_string(),
            "-path".to_owned(),
            root.to_string_lossy().into_owned(),
        ];
        a.extend(terms);
        Some(a)
    }

    /// Paths Everything finds; `None` when it is not installed or not running.
    pub async fn search(root: &Path, query: &NameQuery) -> Option<Vec<PathBuf>> {
        let args = args(root, query)?;
        let extra = [
            PathBuf::from(r"C:\Program Files\Everything"),
            PathBuf::from(r"C:\Program Files (x86)\Everything"),
        ];
        let exe = find_program(&["es.exe"], &extra)?;
        let run = tokio::process::Command::new(exe)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        let out = tokio::time::timeout(INDEX_TIMEOUT, run).await.ok()?.ok()?;
        if !out.status.success() {
            return None;
        }
        let paths: Vec<PathBuf> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(PathBuf::from)
            .collect();
        (!paths.is_empty()).then_some(paths)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn file(name: &str, size: u64, modified_ms: i64) -> FileEntry {
        FileEntry {
            name: name.into(),
            kind: EntryKind::File,
            size,
            modified_ms: Some(modified_ms),
            mode: None,
        }
    }

    #[test]
    fn words_typos_and_patterns() {
        let q = NameQuery::new("Rep  PDF").unwrap();
        assert!(q.matches_name("Q3 report.pdf"));
        assert!(!q.matches_name("report.txt"));
        // One typo in a word of 4+, two from 8; short words are exact.
        assert!(NameQuery::new("mian").unwrap().matches_name("main.rs"));
        assert!(
            NameQuery::new("dashbaord")
                .unwrap()
                .matches_name("Dashboard.tsx")
        );
        assert!(!NameQuery::new("mian").unwrap().matches_name("readme.md"));
        assert!(!NameQuery::new("'mian").unwrap().matches_name("main.rs"));
        assert!(!NameQuery::new("rz").unwrap().matches_name("rs"));
        let g = NameQuery::new("*.rs").unwrap();
        assert!(g.matches_name("main.rs"));
        assert!(!g.matches_name("main.rs.bak"));
        let g = NameQuery::new("data-??.csv").unwrap();
        assert!(g.matches_name("DATA-01.csv"));
        assert!(!g.matches_name("data-1.csv"));
        assert!(NameQuery::new("  ").is_none());
    }

    #[test]
    fn filters() {
        let day = 86_400_000;
        let now = 100 * day;
        let q = NameQuery::at("ext:rs,toml", now).unwrap();
        assert!(q.matches(&file("main.rs", 1, now)));
        assert!(!q.matches(&file("main.py", 1, now)));
        let q = NameQuery::at("type:image size:>1mb", now).unwrap();
        assert!(q.matches(&file("a.PNG", 2 << 20, now)));
        assert!(!q.matches(&file("a.png", 10, now)));
        assert!(!q.matches(&file("a.rs", 2 << 20, now)));
        let q = NameQuery::at("log mtime:<7d", now).unwrap();
        assert!(q.matches(&file("app.log", 1, now - day)));
        assert!(!q.matches(&file("app.log", 1, now - 30 * day)));
        let q = NameQuery::at("type:folder", now).unwrap();
        assert!(!q.matches(&file("x", 1, now)));
    }

    #[test]
    fn local_search_respects_gitignore_unless_asked() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src/deep")).unwrap();
        std::fs::create_dir_all(root.join("build")).unwrap();
        std::fs::write(root.join("src/deep/report.md"), "x").unwrap();
        std::fs::write(root.join("build/report.md"), "x").unwrap();
        std::fs::write(root.join(".gitignore"), "build/\n").unwrap();
        // `ignore` reads .gitignore inside a repository only.
        std::fs::create_dir(root.join(".git")).unwrap();
        let q = NameQuery::new("report").unwrap();
        let run = |everything| {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let stop = AtomicU8::new(0);
            search_local(root, &q, everything, &stop, &tx);
            drop(tx);
            let mut v = Vec::new();
            while let Ok(h) = rx.try_recv() {
                v.push(
                    h.dir
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
            v.sort();
            v
        };
        assert_eq!(run(false), ["src/deep"]);
        assert_eq!(run(true), ["build", "src/deep"]);
    }

    #[test]
    fn fsearch_requests_and_answers() {
        let line = fsearch::request(Path::new("/Users/me/My Code"), " main ext:rs ");
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["q"], "main ext:rs in:\"/Users/me/My Code\"");
        assert_eq!(v["limit"], MAX_HITS);
        let hits = fsearch::parse(
            r#"{"hits":[{"kind":"file","mtime":10,"path":"/a/b/main.rs","score":9,"size":42},{"kind":"dir","mtime":0,"path":"/a/src","score":1,"size":0}],"id":null,"ok":true,"took_us":80}"#,
        )
        .unwrap();
        assert_eq!(hits[0].dir, PathBuf::from("/a/b"));
        assert_eq!(hits[0].entry.name, "main.rs");
        assert_eq!(hits[0].entry.modified_ms, Some(10_000));
        assert!(hits[1].entry.is_dir());
        assert!(fsearch::parse(r#"{"ok":false,"error":"indexing"}"#).is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fsearch_socket_round_trip() {
        use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("fsearch.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            let (r, mut w) = s.into_split();
            let mut line = String::new();
            tokio::io::BufReader::new(r)
                .read_line(&mut line)
                .await
                .unwrap();
            assert!(line.contains("in:"));
            w.write_all(b"{\"ok\":true,\"hits\":[{\"kind\":\"file\",\"path\":\"/x/y.txt\",\"size\":1,\"mtime\":1}]}\n")
                .await
                .unwrap();
        });
        let answer = fsearch::ask_socket(&sock, &fsearch::request(Path::new("/x"), "y"))
            .await
            .unwrap();
        assert_eq!(fsearch::parse(&answer).unwrap()[0].entry.name, "y.txt");
    }

    #[test]
    fn everything_arguments_and_hidden_paths() {
        let q = NameQuery::new("report *.pdf ext:pdf size:>1mb").unwrap();
        let a = everything::args(Path::new(r"C:\Users\me"), &q).unwrap();
        assert_eq!(a[..4], ["-n", "5000", "-path", r"C:\Users\me"]);
        assert_eq!(a[4..], ["report", "*.pdf", "ext:pdf"]);
        // Filters alone are not something Everything can search by name.
        assert!(everything::args(Path::new("/"), &NameQuery::new("size:>1mb").unwrap()).is_none());
        assert!(hidden_below(Path::new("/r"), Path::new("/r/.git/x"), "a"));
        assert!(hidden_below(Path::new("/r"), Path::new("/r/src"), ".env"));
        assert!(!hidden_below(
            Path::new("/r/.cfg"),
            Path::new("/r/.cfg/src"),
            "a"
        ));
    }

    #[tokio::test]
    async fn remote_search_walks_folders() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("a/node_modules")).unwrap();
        std::fs::write(root.join("a/notes.txt"), "x").unwrap();
        std::fs::write(root.join("a/node_modules/notes.txt"), "x").unwrap();
        let fs: Arc<dyn RemoteFs> = Arc::new(switchyard_remote::LocalFs);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let stop = AtomicU8::new(0);
        let q = NameQuery::new("notes").unwrap();
        let more = search_remote(fs, root.to_owned(), &q, false, cfg!(unix), &stop, &tx).await;
        drop(tx);
        assert!(!more);
        let hit = rx.try_recv().unwrap();
        assert!(hit.dir.ends_with("a"));
        assert!(rx.try_recv().is_err());
    }
}
