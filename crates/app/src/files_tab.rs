//! Files tab: a file explorer laid out like the operating system's. One pane by default
//! (two side by side for copying between computers), a places column (quick access,
//! pinned folders, this computer's disks, Hosts and storage), back / forward / up, an
//! address bar, a search box, a command bar, sortable Name / Date modified / Type / Size
//! columns and the transfer drawer while something copies.
//!
//! Right click opens the usual menus: open, open with the default app, cut / copy /
//! paste, duplicate, rename (in place), delete, copy path, pin to quick access, reveal,
//! properties; on the folder itself new folder / new file, paste, sort and hidden files.
//! A double click opens common files here (text, code, configuration, images) and the
//! rest (PDFs, office documents, media, archives) in the computer's default app; a
//! remote file is downloaded to a temp folder first.
//!
//! Folders load a page at a time: object storage (Azure Blob, S3, R2) answers [`PAGE`]
//! entries and a "Load more" footer fetches the next page; other file systems answer in
//! one page. Search filters the loaded rows as you type; Enter also searches the server by
//! name prefix when the folder has more pages than are loaded. The visible rows are sorted
//! once per change (cached), never per frame, and only the rows on screen are drawn.
//!
//! The keyboard drives it like a file manager (arrows, Enter, Backspace, Alt+arrows, F2,
//! Delete, F5, Tab between panes, Ctrl/Cmd+A / C / X / V, Ctrl/Cmd+Shift+N, Ctrl/Cmd+Shift+C
//! copies paths). A folder in a git repository on this computer shows its branch and what
//! changed (badges per row, a "Changes" filter) and refreshes while the tab is on screen,
//! so a coding CLI's edits show up as they happen. "Terminal" and "Code" start a shell or
//! a coding CLI in the folder (on this computer or the Host), optionally in a new git
//! worktree.

mod kinds;
mod menu;
mod places;
mod view;

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement as _, Pixels, Point, Render,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div,
};
use serde::{Deserialize, Serialize};
use switchyard_core::file_search::FileHit;
use switchyard_core::git::{ChangeKind, GitStatus};
use switchyard_core::remote::FileEntry;
use switchyard_core::store::ProfileId;
use switchyard_core::{Command, Event, FsOp, FsRef, OnConflict, RequestId, RuntimeHandle};

use crate::app_state::next_id;
use crate::appearance::{rpx, ts};
use crate::coding_clis::CliChoice;
use crate::theme::{Palette, palette};
use crate::transfers::Transfers;

pub use kinds::FileKind;
use menu::FileMenu;
use places::Place;

/// Entries per page on file systems that page (object storage).
const PAGE: u32 = 100;
/// How often a folder on this computer is re-read while the tab is on screen.
const LIVE_EVERY: std::time::Duration = std::time::Duration::from_secs(3);
/// Setting key of the explorer's layout, pinned folders and hidden-file choice.
pub const FILES_SETTINGS_KEY: &str = "files.explorer";

/// What the tab asks the workspace to do.
pub enum FilesTabEvent {
    /// Open a file in an editor tab (text, code) or the image viewer.
    Open {
        /// Where.
        fs: FsRef,
        /// File.
        path: PathBuf,
    },
    /// Start a shell, or a coding CLI, in a folder.
    RunHere {
        /// The Host, or `None` for this computer.
        host: Option<ProfileId>,
        /// The folder.
        dir: PathBuf,
        /// The coding CLI; `None` for a plain shell.
        cli: Option<CliChoice>,
        /// The git branch of a worktree made for the run.
        worktree: Option<String>,
    },
    /// Something to say in a toast.
    Toast(String),
}

/// How the tab is laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// Two panes side by side, for copying between computers.
    Dual,
    /// One pane, for browsing (the default).
    Single,
    /// One narrow pane beside a coding terminal: names and git badges only.
    Sidebar,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SortBy {
    Name,
    Size,
    Modified,
    Type,
}

/// The explorer's saved choices.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct FilesSettings {
    /// Two panes instead of one.
    #[serde(default)]
    dual: bool,
    /// Folders on this computer pinned to quick access.
    #[serde(default)]
    pinned: Vec<String>,
    /// Show hidden files.
    #[serde(default)]
    show_hidden: bool,
    /// Hide the places column.
    #[serde(default)]
    hide_places: bool,
}

/// Files being dragged from one pane.
#[derive(Clone, Debug)]
pub struct DraggedFiles {
    pane: usize,
    fs: FsRef,
    paths: Vec<PathBuf>,
}

/// What follows the pointer while dragging.
pub struct DragPreview(pub String);

impl Render for DragPreview {
    fn render(&mut self, _w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        div()
            .px(rpx(8.))
            .py(rpx(4.))
            .rounded(gpui_kit::px(5.))
            .bg(p.elev)
            .border_1()
            .border_color(p.acc)
            .text_size(ts::BODY)
            .text_color(p.fg)
            .child(self.0.clone())
    }
}

/// What the cached row list was built from.
#[derive(Clone, Debug, PartialEq)]
struct ViewKey {
    generation: u64,
    filter: String,
    sort: (SortBy, bool),
    hidden: bool,
    changes_only: bool,
}

struct Pane {
    fs: FsRef,
    label: String,
    path: Option<PathBuf>,
    entries: Vec<FileEntry>,
    /// Bumped whenever `entries` or `git` change (invalidates the row cache).
    generation: u64,
    /// The sorted, filtered rows, rebuilt only when their inputs change.
    rows: RefCell<Option<(ViewKey, Rc<Vec<FileEntry>>)>>,
    request: Option<RequestId>,
    /// The pending request fetches the next page (append, don't replace).
    appending: bool,
    /// Cursor for the next page while the folder has more than is loaded.
    next: Option<String>,
    /// Server-side name prefix the listing was fetched with.
    prefix: String,
    /// Search text: shows loaded rows whose names contain it (any case).
    filter: String,
    error: Option<String>,
    sort: (SortBy, bool),
    show_hidden: bool,
    selected: Vec<String>,
    /// Where a Shift+click range starts.
    anchor: Option<String>,
    /// Folders to go back and forward to.
    back: Vec<PathBuf>,
    forward: Vec<PathBuf>,
    /// The folder's git status (this computer only).
    git: Option<GitStatus>,
    git_request: Option<RequestId>,
    /// Show only changed entries.
    changes_only: bool,
    /// The pending listing is a background refresh: no loading bar.
    quiet: bool,
    /// A search through subfolders while the search box has text.
    found: Option<Found>,
}

/// Results of a search through subfolders.
struct Found {
    request: RequestId,
    /// The folder searched.
    root: PathBuf,
    hits: Vec<FileHit>,
    /// Full paths already listed (hits arrive once each, but the folder's own matches are
    /// shown before the search starts).
    seen: HashSet<PathBuf>,
    done: bool,
    /// Stopped at its limit.
    truncated: bool,
    /// What answered: a file index (fsearch, Everything) or the folder walk.
    engine: &'static str,
    error: Option<String>,
    /// Selected results, by full path; the last one is the cursor.
    selected: Vec<PathBuf>,
}

impl Found {
    fn add(&mut self, pane: &Pane, hits: impl IntoIterator<Item = FileHit>) {
        for h in hits {
            if self.seen.insert(pane.join(&h.dir, &h.entry.name)) {
                self.hits.push(h);
            }
        }
    }
}

impl Pane {
    fn new(fs: FsRef, label: String) -> Self {
        Self {
            fs,
            label,
            path: None,
            entries: Vec::new(),
            generation: 0,
            rows: RefCell::new(None),
            request: None,
            appending: false,
            next: None,
            prefix: String::new(),
            filter: String::new(),
            error: None,
            sort: (SortBy::Name, true),
            show_hidden: false,
            selected: Vec::new(),
            anchor: None,
            back: Vec::new(),
            forward: Vec::new(),
            git: None,
            git_request: None,
            changes_only: false,
            quiet: false,
            found: None,
        }
    }

    /// The git change at entry `name` of the current folder.
    fn change(&self, name: &str) -> Option<ChangeKind> {
        let (git, dir) = (self.git.as_ref()?, self.path.as_ref()?);
        git.change_at(&dir.join(name))
    }

    /// Whether the folder can run a shell (this computer or an SSH Host).
    fn has_shell(&self) -> bool {
        matches!(self.fs, FsRef::Local | FsRef::Host(_))
    }

    fn posix(&self) -> bool {
        self.fs.is_remote() || cfg!(unix)
    }

    fn join(&self, dir: &Path, name: &str) -> PathBuf {
        join_path(self.posix(), dir, name)
    }

    /// The folder above `path`, if any.
    fn parent_of(&self, path: &Path) -> Option<PathBuf> {
        if self.posix() {
            let s = path.to_string_lossy().replace('\\', "/");
            let s = s.trim_end_matches('/');
            match s.rfind('/') {
                Some(0) if s.len() > 1 => Some(PathBuf::from("/")),
                Some(i) if i > 0 => Some(PathBuf::from(&s[..i])),
                _ => None,
            }
        } else {
            path.parent().map(Path::to_path_buf)
        }
    }

    /// The entries changed: the row cache is stale.
    fn touch(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    fn view_key(&self) -> ViewKey {
        ViewKey {
            generation: self.generation,
            filter: self.filter.clone(),
            sort: self.sort,
            hidden: self.show_hidden,
            changes_only: self.changes_only,
        }
    }

    /// The rows on screen in order: filtered and sorted once per change.
    fn visible(&self) -> Rc<Vec<FileEntry>> {
        let key = self.view_key();
        if let Some((k, rows)) = self.rows.borrow().as_ref()
            && *k == key
        {
            return rows.clone();
        }
        let rows = Rc::new(self.build_rows());
        *self.rows.borrow_mut() = Some((key, rows.clone()));
        rows
    }

    fn build_rows(&self) -> Vec<FileEntry> {
        let (by, asc) = self.sort;
        let needle = self.filter.trim().to_lowercase();
        // Lowercase each name once, not once per comparison.
        let mut v: Vec<(String, &'static str, &FileEntry)> = self
            .entries
            .iter()
            .filter(|e| self.show_hidden || !e.is_hidden())
            .filter(|e| needle.is_empty() || e.name.to_lowercase().contains(&needle))
            .filter(|e| !self.changes_only || self.change(&e.name).is_some())
            .map(|e| {
                let kind = if by == SortBy::Type && !e.is_dir() {
                    FileKind::of(&e.name).label()
                } else {
                    ""
                };
                (e.name.to_lowercase(), kind, e)
            })
            .collect();
        v.sort_by(|(ka, ta, a), (kb, tb, b)| {
            // Folders stay on top whatever the column.
            let dirs = b.is_dir().cmp(&a.is_dir());
            let ord = match by {
                SortBy::Name => ka.cmp(kb),
                SortBy::Size => a.size.cmp(&b.size).then_with(|| ka.cmp(kb)),
                SortBy::Modified => a.modified_ms.cmp(&b.modified_ms).then_with(|| ka.cmp(kb)),
                SortBy::Type => ta.cmp(tb).then_with(|| ka.cmp(kb)),
            };
            dirs.then(if asc { ord } else { ord.reverse() })
        });
        v.into_iter().map(|(_, _, e)| e.clone()).collect()
    }

    /// Whether the folder pages on the server: more to load, or a server search applied.
    fn paged(&self) -> bool {
        self.next.is_some() || !self.prefix.is_empty()
    }

    /// `/`, `home`, `swy`, `app` with the path up to each.
    fn crumbs(&self) -> Vec<(String, PathBuf)> {
        let Some(path) = &self.path else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut acc = PathBuf::new();
        for c in path.components() {
            acc.push(c);
            let label = match c {
                std::path::Component::RootDir => "/".to_owned(),
                std::path::Component::Prefix(p) => p.as_os_str().to_string_lossy().into_owned(),
                other => other.as_os_str().to_string_lossy().into_owned(),
            };
            if label == "\\" {
                continue;
            }
            out.push((label, acc.clone()));
        }
        out
    }

    /// The selected entries, in listing order.
    fn selected_entries(&self) -> Vec<&FileEntry> {
        let set: HashSet<&str> = self.selected.iter().map(String::as_str).collect();
        self.entries
            .iter()
            .filter(|e| set.contains(e.name.as_str()))
            .collect()
    }

    /// A name like `base` that no entry of the folder has yet: `New folder (2)`.
    fn unused_name(&self, base: &str, ext: &str) -> String {
        let taken: HashSet<String> = self.entries.iter().map(|e| e.name.to_lowercase()).collect();
        (1..)
            .map(|n| {
                if n == 1 {
                    format!("{base}{ext}")
                } else {
                    format!("{base} ({n}){ext}")
                }
            })
            .find(|name| !taken.contains(&name.to_lowercase()))
            .unwrap_or_else(|| format!("{base}{ext}"))
    }
}

/// Cut or copied files waiting for Paste.
#[derive(Clone, Debug)]
struct Clip {
    fs: FsRef,
    paths: Vec<PathBuf>,
    cut: bool,
}

/// An in-place rename: the pane, the old name and the name box.
struct Rename {
    pane: usize,
    old: String,
    input: Entity<InputState>,
    _sub: Subscription,
}

/// The files tab.
pub struct FilesTab {
    core: RuntimeHandle,
    transfers: Entity<Transfers>,
    panes: [Pane; 2],
    active: usize,
    /// Hosts (SFTP), FTP and cloud storage connections, with names, for the places column.
    sources: Vec<Source>,
    /// Home, Desktop, Documents, … and this computer's disks (found off the UI thread).
    local_places: (Vec<Place>, Vec<Place>),
    pinned: Vec<String>,
    show_places: bool,
    rename: Option<Rename>,
    /// A just created folder or file to rename once its listing shows it.
    pending_rename: Option<(usize, String)>,
    /// A pane's "go to folder" box while it is open.
    goto: Option<(usize, Entity<InputState>, Subscription)>,
    /// Each pane's search box.
    search: [Option<(Entity<InputState>, Subscription)>; 2],
    confirm_delete: Option<usize>,
    op: Option<RequestId>,
    /// A new empty file being written: (request, pane, name).
    new_file: Option<(RequestId, usize, String)>,
    error: Option<String>,
    focus: FocusHandle,
    /// Coding CLIs the "Code" menu offers.
    clis: Vec<CliChoice>,
    /// The open "Code" menu: its pane and where.
    code_menu: Option<(usize, Point<Pixels>)>,
    /// The open context, New or Sort menu.
    menu: Option<FileMenu>,
    /// The pane whose selection's properties show.
    properties: Option<usize>,
    clip: Option<Clip>,
    /// Downloads to open in the default app when they finish.
    opening: Vec<u64>,
    /// Copies of cut files across file systems: the source is deleted when each finishes.
    moving: Vec<(u64, FsRef, PathBuf)>,
    /// Make a new git worktree for the next coding CLI run.
    use_worktree: bool,
    /// A worktree being made for a run: (request, CLI).
    pending_worktree: Option<(RequestId, CliChoice)>,
    /// Rendered since the last live refresh: the tab is on screen.
    seen: bool,
    scroll: [gpui_kit::UniformListScrollHandle; 2],
    layout: Layout,
    /// Save layout and pins (the Files tab; not a coding sidebar).
    saves: bool,
    /// Bumped per keystroke in a search box: only the last one starts a search.
    search_ticket: u64,
    _live: gpui_kit::Task<()>,
}

impl EventEmitter<FilesTabEvent> for FilesTab {}

/// A file system the places column offers: where, its name and its badge.
pub type Source = (FsRef, String, &'static str);

/// The display name of `fs` among `sources`.
fn source_name(sources: &[Source], fs: &FsRef) -> String {
    match fs {
        FsRef::Local => "This computer".into(),
        _ => sources
            .iter()
            .find(|(s, _, _)| s == fs)
            .map(|(_, n, _)| n.clone())
            .unwrap_or_default(),
    }
}

/// Text color for a git change badge.
fn change_color(c: ChangeKind, p: &Palette) -> gpui_kit::Hsla {
    match c {
        ChangeKind::Modified | ChangeKind::Renamed => p.stg,
        ChangeKind::Added | ChangeKind::Untracked => p.dev,
        ChangeKind::Deleted | ChangeKind::Conflicted => p.prod,
    }
}

fn fmt_time(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let (y, m, d) = switchyard_core::db::value::civil_from_days(days);
    let mins = ms.rem_euclid(86_400_000) / 60_000;
    format!("{y}-{m:02}-{d:02} {:02}:{:02}", mins / 60, mins % 60)
}

/// `dir/name`, with `/` on POSIX file systems (Hosts, storage, macOS and Linux).
fn join_path(posix: bool, dir: &Path, name: &str) -> PathBuf {
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

/// The last part of a path, either separator.
fn base_name(path: &Path) -> String {
    let s = path.to_string_lossy();
    s.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .to_owned()
}

impl FilesTab {
    /// One pane showing `start` (a Host, an FTP or storage connection, or this computer);
    /// the second pane, when shown, starts on this computer.
    pub fn new(
        core: RuntimeHandle,
        transfers: Entity<Transfers>,
        sources: Vec<Source>,
        start: FsRef,
        cx: &mut Context<Self>,
    ) -> Self {
        let first = Pane::new(start.clone(), source_name(&sources, &start));
        let live = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(LIVE_EVERY).await;
                if this.update(cx, |t, cx| t.live_tick(cx)).is_err() {
                    break;
                }
            }
        });
        let mut this = Self {
            core,
            transfers,
            panes: [first, Pane::new(FsRef::Local, "This computer".into())],
            active: 0,
            sources,
            local_places: (Vec::new(), Vec::new()),
            pinned: Vec::new(),
            show_places: true,
            rename: None,
            pending_rename: None,
            goto: None,
            search: [None, None],
            confirm_delete: None,
            op: None,
            new_file: None,
            error: None,
            focus: cx.focus_handle(),
            clis: Vec::new(),
            code_menu: None,
            menu: None,
            properties: None,
            clip: None,
            opening: Vec::new(),
            moving: Vec::new(),
            use_worktree: false,
            pending_worktree: None,
            seen: true,
            scroll: Default::default(),
            layout: Layout::Single,
            saves: true,
            search_ticket: 0,
            _live: live,
        };
        this.list(0, None, cx);
        this.core.send(Command::LoadSetting {
            key: FILES_SETTINGS_KEY.into(),
        });
        cx.spawn(async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(async { places::local_places() })
                .await;
            this.update(cx, |t, cx| {
                t.local_places = found;
                cx.notify();
            })
            .ok();
        })
        .detach();
        this
    }

    /// One narrow pane showing `dir` on `fs` (this computer or a Host), for the side of a
    /// coding terminal.
    pub fn sidebar(
        core: RuntimeHandle,
        transfers: Entity<Transfers>,
        fs: FsRef,
        dir: PathBuf,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self::new(core, transfers, Vec::new(), fs.clone(), cx);
        this.layout = Layout::Sidebar;
        this.saves = false;
        this.show_places = false;
        let label = crate::coding_clis::folder_name(&dir);
        this.panes[0] = Pane::new(fs, label);
        this.list(0, Some(dir), cx);
        this
    }

    /// The source the tab is about: the remote pane, else this computer.
    pub fn source(&self) -> &FsRef {
        let shown = if self.layout == Layout::Dual { 2 } else { 1 };
        self.panes[..shown]
            .iter()
            .map(|p| &p.fs)
            .find(|fs| fs.is_remote())
            .unwrap_or(&self.panes[0].fs)
    }

    /// The Host the tab shows, if any.
    pub fn host(&self) -> Option<&ProfileId> {
        match self.source() {
            FsRef::Host(h) => Some(h),
            FsRef::Local | FsRef::Conn(_) => None,
        }
    }

    /// Whether a pane on screen shows `fs`.
    pub fn shows(&self, fs: &FsRef) -> bool {
        let shown = if self.layout == Layout::Dual { 2 } else { 1 };
        self.panes[..shown].iter().any(|p| p.fs == *fs)
    }

    /// Show a Host, an FTP or storage connection, or this computer: in the one pane, or
    /// the right one of two.
    pub fn show(&mut self, fs: FsRef, cx: &mut Context<Self>) {
        let ix = if self.layout == Layout::Dual { 1 } else { 0 };
        self.set_source(ix, fs, None, cx);
    }

    /// Point pane `ix` at `fs`, at `path` (`None`: its home).
    fn set_source(&mut self, ix: usize, fs: FsRef, path: Option<PathBuf>, cx: &mut Context<Self>) {
        self.close_menus();
        self.active = ix;
        if self.panes[ix].fs == fs {
            return self.navigate(ix, path, cx);
        }
        let name = source_name(&self.sources, &fs);
        let hidden = self.panes[ix].show_hidden;
        self.panes[ix] = Pane::new(fs, name);
        self.panes[ix].show_hidden = hidden;
        self.list(ix, path, cx);
    }

    /// Hosts, FTP and cloud storage connections for the places column.
    pub fn set_sources(&mut self, sources: Vec<Source>, cx: &mut Context<Self>) {
        for pane in &mut self.panes {
            if pane.fs.is_remote()
                && let Some((_, n, _)) = sources.iter().find(|(fs, _, _)| *fs == pane.fs)
            {
                pane.label = n.clone();
            }
        }
        self.sources = sources;
        cx.notify();
    }

    /// The coding CLIs for the "Code" menu.
    pub fn set_clis(&mut self, clis: Vec<CliChoice>, cx: &mut Context<Self>) {
        self.clis = clis;
        cx.notify();
    }

    fn settings(&self) -> FilesSettings {
        FilesSettings {
            dual: self.layout == Layout::Dual,
            pinned: self.pinned.clone(),
            show_hidden: self.panes[0].show_hidden,
            hide_places: !self.show_places,
        }
    }

    fn save_settings(&self) {
        if !self.saves {
            return;
        }
        if let Ok(value) = serde_json::to_value(self.settings()) {
            self.core.send(Command::SetSetting {
                key: FILES_SETTINGS_KEY.into(),
                value,
            });
        }
    }

    fn apply_settings(&mut self, s: FilesSettings, cx: &mut Context<Self>) {
        self.pinned = s.pinned;
        if self.layout != Layout::Sidebar {
            self.show_places = !s.hide_places;
            self.set_layout(if s.dual { Layout::Dual } else { Layout::Single }, cx);
            for pane in &mut self.panes {
                pane.show_hidden = s.show_hidden;
            }
        }
        cx.notify();
    }

    /// One pane or two; the second lists its home the first time it shows.
    fn set_layout(&mut self, layout: Layout, cx: &mut Context<Self>) {
        self.layout = layout;
        self.active = 0;
        if layout == Layout::Dual && self.panes[1].path.is_none() && self.panes[1].request.is_none()
        {
            self.list(1, None, cx);
        }
        cx.notify();
    }

    /// Re-read local folders (and their git status) while the tab is on screen: a render
    /// since the last tick means it is.
    fn live_tick(&mut self, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.seen) {
            for ix in 0..2 {
                let pane = &self.panes[ix];
                let busy = pane.request.is_some()
                    || pane.git_request.is_some()
                    || pane.paged()
                    || self.rename.as_ref().is_some_and(|r| r.pane == ix);
                if pane.fs == FsRef::Local && pane.path.is_some() && pane.error.is_none() && !busy {
                    self.refresh(ix, cx);
                    self.panes[ix].quiet = true;
                }
            }
        }
        // Renders only when on screen; the next tick reads `seen`.
        cx.notify();
    }

    /// Ask for the git status of pane `ix`'s folder (this computer only).
    fn fetch_git(&mut self, ix: usize) {
        let pane = &mut self.panes[ix];
        let (FsRef::Local, Some(dir)) = (&pane.fs, &pane.path) else {
            pane.git = None;
            return;
        };
        let request = next_id();
        pane.git_request = Some(request);
        self.core.send(Command::GitStatus {
            request,
            dir: dir.clone(),
        });
    }

    /// Open a folder: its first page, without a search.
    fn list(&mut self, ix: usize, path: Option<PathBuf>, cx: &mut Context<Self>) {
        self.stop_find(ix);
        let pane = &mut self.panes[ix];
        pane.prefix.clear();
        pane.filter.clear();
        self.search[ix] = None;
        self.confirm_delete = None;
        self.fetch(ix, path, None, cx);
    }

    /// Go to a folder, remembering where from for Back.
    fn navigate(&mut self, ix: usize, path: Option<PathBuf>, cx: &mut Context<Self>) {
        let pane = &mut self.panes[ix];
        if let Some(cur) = pane.path.clone() {
            if path.as_ref() == Some(&cur) {
                return self.refresh(ix, cx);
            }
            pane.back.push(cur);
            pane.forward.clear();
        }
        self.list(ix, path, cx);
    }

    fn go_back(&mut self, ix: usize, cx: &mut Context<Self>) {
        let pane = &mut self.panes[ix];
        let Some(to) = pane.back.pop() else { return };
        if let Some(cur) = pane.path.clone() {
            pane.forward.push(cur);
        }
        self.list(ix, Some(to), cx);
    }

    fn go_forward(&mut self, ix: usize, cx: &mut Context<Self>) {
        let pane = &mut self.panes[ix];
        let Some(to) = pane.forward.pop() else { return };
        if let Some(cur) = pane.path.clone() {
            pane.back.push(cur);
        }
        self.list(ix, Some(to), cx);
    }

    /// The first page again, keeping the search.
    fn refresh(&mut self, ix: usize, cx: &mut Context<Self>) {
        let path = self.panes[ix].path.clone();
        self.fetch(ix, path, None, cx);
    }

    /// The next page, added below what is loaded.
    fn load_more(&mut self, ix: usize, cx: &mut Context<Self>) {
        let pane = &self.panes[ix];
        if pane.request.is_some() {
            return;
        }
        if let Some(cursor) = pane.next.clone() {
            let path = pane.path.clone();
            self.fetch(ix, path, Some(cursor), cx);
        }
    }

    fn fetch(
        &mut self,
        ix: usize,
        path: Option<PathBuf>,
        cursor: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let request = next_id();
        let pane = &mut self.panes[ix];
        pane.request = Some(request);
        pane.appending = cursor.is_some();
        pane.quiet = false;
        self.core.send(Command::ListDirPage {
            request,
            fs: pane.fs.clone(),
            path,
            prefix: pane.prefix.clone(),
            cursor,
            limit: PAGE,
        });
        cx.notify();
    }

    /// Make pane `ix`'s search box (it is always on screen).
    fn ensure_search(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.search[ix].is_some() {
            return;
        }
        let current = self.panes[ix].filter.clone();
        let place = match &self.panes[ix].path {
            Some(p) if !base_name(p).is_empty() => format!("Search {}", base_name(p)),
            _ => "Search".into(),
        };
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(place)
                .default_value(current)
        });
        let sub = cx.subscribe_in(
            &input,
            window,
            move |this, input, ev: &InputEvent, _, cx| match ev {
                InputEvent::Change => {
                    this.panes[ix].filter = input.read(cx).value().to_string();
                    this.active = ix;
                    this.find_soon(ix, cx);
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => this.search_server(ix, cx),
                _ => {}
            },
        );
        self.search[ix] = Some((input, sub));
    }

    /// Put the cursor in pane `ix`'s search box.
    fn focus_search(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.ensure_search(ix, window, cx);
        if let Some((input, _)) = &self.search[ix] {
            input.update(cx, |i, cx| i.focus(window, cx));
        }
    }

    /// Search the whole folder on the server by name prefix, when it pages.
    fn search_server(&mut self, ix: usize, cx: &mut Context<Self>) {
        let pane = &mut self.panes[ix];
        let text = pane.filter.trim().to_owned();
        if !pane.paged() || text == pane.prefix {
            return;
        }
        pane.prefix = text;
        self.refresh(ix, cx);
    }

    /// Search subfolders a moment after the last keystroke.
    fn find_soon(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.search_ticket += 1;
        let ticket = self.search_ticket;
        if self.panes[ix].filter.trim().is_empty() {
            return self.stop_find(ix);
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(220))
                .await;
            this.update(cx, |t, cx| {
                if t.search_ticket == ticket {
                    t.start_find(ix, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Search pane `ix`'s folder and its subfolders for the search box's text. Folders
    /// that page (object storage) search by name prefix on Enter instead.
    fn start_find(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.stop_find(ix);
        let pane = &self.panes[ix];
        let query = pane.filter.trim().to_owned();
        let (Some(root), false) = (pane.path.clone(), query.is_empty() || pane.paged()) else {
            return;
        };
        let request = next_id();
        self.core.send(Command::SearchFiles {
            request,
            fs: pane.fs.clone(),
            root: root.clone(),
            query,
            everything: pane.show_hidden,
        });
        let mut found = Found {
            request,
            root: root.clone(),
            hits: Vec::new(),
            seen: HashSet::new(),
            done: false,
            truncated: false,
            engine: "",
            error: None,
            selected: Vec::new(),
        };
        // The folder's own matches show at once; deeper ones stream in.
        let here: Vec<FileHit> = pane
            .visible()
            .iter()
            .map(|e| FileHit {
                dir: root.clone(),
                entry: e.clone(),
            })
            .collect();
        found.add(pane, here);
        self.panes[ix].found = Some(found);
        self.scroll[ix].scroll_to_item(0, gpui_kit::ScrollStrategy::Top);
        cx.notify();
    }

    /// Cancel pane `ix`'s search through subfolders, if any.
    fn stop_find(&mut self, ix: usize) {
        if let Some(f) = self.panes[ix].found.take()
            && !f.done
        {
            self.core
                .send(Command::CancelFileSearch { request: f.request });
        }
    }

    /// Drop the search (re-listing if the server applied it).
    fn clear_search(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.stop_find(ix);
        self.search[ix] = None;
        let pane = &mut self.panes[ix];
        pane.filter.clear();
        if !pane.prefix.is_empty() {
            pane.prefix.clear();
            self.refresh(ix, cx);
        }
        cx.notify();
    }

    /// Runtime events.
    pub fn on_event(&mut self, ev: &Event, cx: &mut Context<Self>) {
        match ev {
            Event::FsListing {
                request,
                path,
                result,
                next,
                ..
            } => {
                let Some(ix) = self.panes.iter().position(|p| p.request == Some(*request)) else {
                    return;
                };
                let pane = &mut self.panes[ix];
                pane.request = None;
                pane.quiet = false;
                let appending = std::mem::take(&mut pane.appending);
                match result {
                    Ok(entries) if appending => {
                        pane.entries.extend(entries.iter().cloned());
                        pane.next = next.clone();
                        pane.touch();
                    }
                    Ok(entries) => {
                        let moved = pane.path.as_ref() != Some(path);
                        if moved {
                            pane.selected.clear();
                            pane.anchor = None;
                            pane.git = None;
                            pane.changes_only = false;
                        }
                        pane.path = Some(path.clone());
                        // A live refresh that found nothing new changes nothing on screen.
                        if moved || pane.entries != *entries {
                            pane.entries = entries.clone();
                            pane.touch();
                        }
                        pane.next = next.clone();
                        pane.error = None;
                        // Drop selected names that are gone.
                        let names: HashSet<&str> =
                            entries.iter().map(|e| e.name.as_str()).collect();
                        pane.selected.retain(|n| names.contains(n.as_str()));
                        if moved {
                            self.scroll[ix].scroll_to_item(0, gpui_kit::ScrollStrategy::Top);
                        }
                        self.fetch_git(ix);
                    }
                    Err(e) if appending => {
                        self.error = Some(format!("Can't load more: {e}"));
                    }
                    // A folder that can't be opened leaves the current one on screen.
                    Err(e) if pane.path.is_some() && pane.error.is_none() => {
                        self.error = Some(format!("Can't open {}: {e}", path.to_string_lossy()));
                    }
                    Err(e) => pane.error = Some(e.clone()),
                }
            }
            Event::GitStatus {
                request,
                dir,
                result,
            } => {
                let Some(ix) = self
                    .panes
                    .iter()
                    .position(|p| p.git_request == Some(*request))
                else {
                    return;
                };
                let pane = &mut self.panes[ix];
                pane.git_request = None;
                if pane.path.as_ref() == Some(dir) {
                    // A failing `git` (a broken repository) just shows no status.
                    let git = result.clone().ok().flatten();
                    if git != pane.git {
                        pane.git = git;
                        pane.touch();
                    }
                    if pane.git.is_none() {
                        pane.changes_only = false;
                    }
                }
            }
            Event::GitWorktree { request, result }
                if self.pending_worktree.as_ref().map(|(r, _)| *r) == Some(*request) =>
            {
                let Some((_, cli)) = self.pending_worktree.take() else {
                    return;
                };
                match result {
                    Ok((dir, branch)) => cx.emit(FilesTabEvent::RunHere {
                        host: None,
                        dir: dir.clone(),
                        cli: Some(cli),
                        worktree: Some(branch.clone()),
                    }),
                    Err(e) => self.error = Some(format!("Can't make a worktree: {e}")),
                }
            }
            Event::FsOpDone {
                request, result, ..
            } if Some(*request) == self.op => {
                self.op = None;
                self.error = result.as_ref().err().cloned();
                if self.error.is_some() {
                    self.pending_rename = None;
                }
                self.refresh(0, cx);
                self.refresh(1, cx);
            }
            // Changed elsewhere (sidebar panel, editor): refresh panes on that file system.
            Event::FsOpDone { fs, .. } => {
                for ix in 0..2 {
                    if self.panes[ix].fs == *fs {
                        self.refresh(ix, cx);
                    }
                }
            }
            Event::TextFileSaved { request, result }
                if self.new_file.as_ref().map(|(r, ..)| *r) == Some(*request) =>
            {
                let Some((_, ix, name)) = self.new_file.take() else {
                    return;
                };
                match result {
                    Ok(_) => {
                        self.pending_rename = Some((ix, name));
                        self.refresh(ix, cx);
                    }
                    Err(switchyard_core::SaveError::Conflict(_)) => {
                        self.error = Some(format!("{name} already exists"));
                    }
                    Err(switchyard_core::SaveError::Failed(e)) => {
                        self.error = Some(format!("Can't make {name}: {e}"));
                    }
                }
            }
            Event::TransferDone { id, result } => {
                if let Some(at) = self.opening.iter().position(|o| o == id) {
                    self.opening.remove(at);
                    match result {
                        Ok(local) => cx.open_with_system(local),
                        Err(switchyard_core::TransferError::Failed(e)) => {
                            self.error = Some(format!("Could not download it: {e}"));
                        }
                        Err(_) => {}
                    }
                    return cx.notify();
                }
                if let Some(at) = self.moving.iter().position(|(m, ..)| m == id) {
                    let (_, fs, path) = self.moving.remove(at);
                    // A cut file is deleted only once its copy is complete.
                    if result.is_ok() {
                        self.core.send(Command::FsOp {
                            request: next_id(),
                            fs,
                            op: FsOp::Delete(path),
                        });
                    }
                }
                if result.is_ok() {
                    self.refresh(0, cx);
                    self.refresh(1, cx);
                }
            }
            Event::FileSearchHits { request, hits } => {
                let Some(ix) = self
                    .panes
                    .iter()
                    .position(|p| p.found.as_ref().map(|f| f.request) == Some(*request))
                else {
                    return;
                };
                let pane = &mut self.panes[ix];
                if let Some(mut f) = pane.found.take() {
                    f.add(pane, hits.iter().cloned());
                    pane.found = Some(f);
                }
            }
            Event::FileSearchDone { request, result } => {
                let Some(f) = self
                    .panes
                    .iter_mut()
                    .find_map(|p| p.found.as_mut().filter(|f| f.request == *request))
                else {
                    return;
                };
                f.done = true;
                match result {
                    Ok((truncated, engine)) => {
                        f.truncated = *truncated;
                        f.engine = engine;
                    }
                    Err(e) => f.error = Some(e.clone()),
                }
            }
            Event::Setting { key, value } if key == FILES_SETTINGS_KEY => {
                let s = value
                    .clone()
                    .and_then(|v| serde_json::from_value::<FilesSettings>(v).ok())
                    .unwrap_or_default();
                self.apply_settings(s, cx);
            }
            _ => return,
        }
        cx.notify();
    }

    /// Copy `paths` from `from` into `dir` (or the folder shown) of pane `to_ix`.
    fn copy_into(
        &mut self,
        from: FsRef,
        paths: Vec<PathBuf>,
        to_ix: usize,
        dir: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) -> Vec<u64> {
        let Some(dir) = dir.or_else(|| self.panes[to_ix].path.clone()) else {
            return Vec::new();
        };
        let to = self.panes[to_ix].fs.clone();
        let pane = &self.panes[to_ix];
        self.transfers.update(cx, |t, cx| {
            paths
                .into_iter()
                .map(|p| {
                    // Into the folder it is already in: keep both, like the OS does.
                    let same_dir = from == to && pane.parent_of(&p).as_deref() == Some(&dir);
                    let conflict = if same_dir {
                        OnConflict::KeepBoth
                    } else {
                        OnConflict::Ask
                    };
                    t.start(from.clone(), p, to.clone(), Some(dir.clone()), conflict, cx)
                })
                .collect()
        })
    }

    fn selected_paths(&self, ix: usize) -> Vec<PathBuf> {
        let pane = &self.panes[ix];
        if let Some(f) = &pane.found {
            return f.selected.clone();
        }
        let Some(dir) = &pane.path else {
            return Vec::new();
        };
        pane.selected.iter().map(|n| pane.join(dir, n)).collect()
    }

    fn copy_selection_across(&mut self, ix: usize, cx: &mut Context<Self>) {
        let paths = self.selected_paths(ix);
        if paths.is_empty() {
            return;
        }
        let from = self.panes[ix].fs.clone();
        self.copy_into(from, paths, 1 - ix, None, cx);
    }

    /// Put the selection on the clipboard: as paths for other apps, and for Paste here.
    fn clip_selection(&mut self, ix: usize, cut: bool, cx: &mut Context<Self>) {
        let paths = self.selected_paths(ix);
        if paths.is_empty() {
            return;
        }
        let text = paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(text));
        let n = paths.len();
        self.clip = Some(Clip {
            fs: self.panes[ix].fs.clone(),
            paths,
            cut,
        });
        cx.emit(FilesTabEvent::Toast(match (n, cut) {
            (1, true) => "Cut: paste it in another folder to move it".into(),
            (1, false) => "Copied".into(),
            (n, true) => format!("{n} items cut"),
            (n, false) => format!("{n} items copied"),
        }));
        cx.notify();
    }

    /// Paste the clipboard into `into` (a folder of pane `ix`), or the folder shown.
    fn paste(&mut self, ix: usize, into: Option<PathBuf>, cx: &mut Context<Self>) {
        let Some(clip) = self.clip.clone() else {
            return;
        };
        let pane = &self.panes[ix];
        let Some(dir) = into.or_else(|| pane.path.clone()) else {
            return;
        };
        let to = pane.fs.clone();
        if clip.cut && clip.fs == to {
            // A move on one file system is a rename.
            for p in &clip.paths {
                if pane.parent_of(p).as_deref() == Some(&dir) || dir.starts_with(p) {
                    continue;
                }
                let request = next_id();
                self.op = Some(request);
                self.core.send(Command::FsOp {
                    request,
                    fs: to.clone(),
                    op: FsOp::Rename(p.clone(), pane.join(&dir, &base_name(p))),
                });
            }
        } else {
            let ids = self.copy_into(clip.fs.clone(), clip.paths.clone(), ix, Some(dir), cx);
            if clip.cut {
                self.moving.extend(
                    ids.into_iter()
                        .zip(clip.paths.iter().cloned())
                        .map(|(id, p)| (id, clip.fs.clone(), p)),
                );
            }
        }
        if clip.cut {
            self.clip = None;
        }
        cx.notify();
    }

    /// Copies of the selection beside it (`name (1).ext`).
    fn duplicate(&mut self, ix: usize, cx: &mut Context<Self>) {
        let paths = self.selected_paths(ix);
        let fs = self.panes[ix].fs.clone();
        self.copy_into(fs, paths, ix, None, cx);
    }

    /// Select `name`, or toggle it (Ctrl/Cmd), or select the range from the anchor (Shift).
    fn select(&mut self, ix: usize, name: &str, toggle: bool, range: bool, cx: &mut Context<Self>) {
        self.active = ix;
        self.confirm_delete = None;
        let rows = self.panes[ix].visible();
        let pane = &mut self.panes[ix];
        let anchor = pane.anchor.clone();
        if range
            && let Some(anchor) = anchor
            && let (Some(a), Some(b)) = (
                rows.iter().position(|e| e.name == anchor),
                rows.iter().position(|e| e.name == name),
            )
        {
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            pane.selected = rows[lo..=hi].iter().map(|e| e.name.clone()).collect();
            // Keep the clicked row last: arrows move from it.
            pane.selected.retain(|n| n != name);
            pane.selected.push(name.to_owned());
        } else if toggle {
            if let Some(i) = pane.selected.iter().position(|n| n == name) {
                pane.selected.remove(i);
            } else {
                pane.selected.push(name.to_owned());
            }
            pane.anchor = Some(name.to_owned());
        } else {
            pane.selected = vec![name.to_owned()];
            pane.anchor = Some(name.to_owned());
        }
        cx.notify();
    }

    fn up(&mut self, ix: usize, cx: &mut Context<Self>) {
        let pane = &self.panes[ix];
        if let Some(up) = pane.path.as_ref().and_then(|p| pane.parent_of(p)) {
            self.navigate(ix, Some(up), cx);
        }
    }

    fn open_goto(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.panes[ix]
            .path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Folder path, ~ or a subfolder")
                .default_value(current)
        });
        let sub = cx.subscribe_in(
            &input,
            window,
            move |this, input, ev: &InputEvent, window, cx| match ev {
                InputEvent::PressEnter { .. } => {
                    let typed = input.read(cx).value().trim().to_owned();
                    this.goto = None;
                    let pane = &this.panes[ix];
                    let target = if typed.is_empty() {
                        None
                    } else if typed.starts_with('~')
                        || typed.starts_with('/')
                        || Path::new(&typed).is_absolute()
                    {
                        Some(PathBuf::from(&typed))
                    } else {
                        pane.path.as_deref().map(|d| pane.join(d, &typed))
                    };
                    if let Some(target) = target {
                        this.error = None;
                        this.navigate(ix, Some(target), cx);
                    }
                    window.focus(&this.focus, cx);
                    cx.notify();
                }
                InputEvent::Blur => {
                    this.goto = None;
                    cx.notify();
                }
                _ => {}
            },
        );
        input.update(cx, |i, cx| {
            i.focus(window, cx);
            i.select_all(window, cx);
        });
        self.goto = Some((ix, input, sub));
        cx.notify();
    }

    /// Rename the selected entry in place (F2).
    fn start_rename(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.panes[ix].selected.last().cloned() else {
            return;
        };
        self.rename_entry(ix, name, window, cx);
    }

    fn rename_entry(
        &mut self,
        ix: usize,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_menus();
        let input = cx.new(|cx| InputState::new(window, cx).default_value(name.clone()));
        let sub = cx.subscribe_in(
            &input,
            window,
            |this, _, ev: &InputEvent, window, cx| match ev {
                InputEvent::PressEnter { .. } | InputEvent::Blur => this.finish_rename(window, cx),
                _ => {}
            },
        );
        input.update(cx, |i, cx| {
            i.focus(window, cx);
            i.select_all(window, cx);
        });
        let pane = &mut self.panes[ix];
        pane.selected = vec![name.clone()];
        if let Some(row) = pane.visible().iter().position(|e| e.name == name) {
            self.scroll[ix].scroll_to_item(row, gpui_kit::ScrollStrategy::Nearest);
        }
        self.active = ix;
        self.rename = Some(Rename {
            pane: ix,
            old: name,
            input,
            _sub: sub,
        });
        cx.notify();
    }

    fn cancel_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.rename.take().is_some() {
            window.focus(&self.focus, cx);
            cx.notify();
        }
    }

    fn finish_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(r) = self.rename.take() else {
            return;
        };
        window.focus(&self.focus, cx);
        let name = r.input.read(cx).value().trim().to_owned();
        let pane = &mut self.panes[r.pane];
        let Some(dir) = pane.path.clone() else { return };
        if name.is_empty() || name == r.old {
            return cx.notify();
        }
        if name.contains('/') || (!pane.posix() && name.contains('\\')) {
            self.error = Some("A name can't contain a slash".into());
            return cx.notify();
        }
        pane.selected = vec![name.clone()];
        pane.anchor = Some(name.clone());
        let request = next_id();
        self.op = Some(request);
        self.core.send(Command::FsOp {
            request,
            fs: pane.fs.clone(),
            op: FsOp::Rename(pane.join(&dir, &r.old), pane.join(&dir, &name)),
        });
        cx.notify();
    }

    /// Make `New folder` (or `New folder (2)`) and rename it in place, like the OS.
    fn new_folder(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.close_menus();
        let pane = &self.panes[ix];
        let Some(dir) = pane.path.clone() else { return };
        let name = pane.unused_name("New folder", "");
        let request = next_id();
        self.op = Some(request);
        self.core.send(Command::FsOp {
            request,
            fs: pane.fs.clone(),
            op: FsOp::Mkdir(pane.join(&dir, &name)),
        });
        self.pending_rename = Some((ix, name));
        cx.notify();
    }

    /// Make an empty `Untitled.txt` and rename it in place.
    fn new_file(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.close_menus();
        let pane = &self.panes[ix];
        let Some(dir) = pane.path.clone() else { return };
        let name = pane.unused_name("Untitled", ".txt");
        let request = next_id();
        self.new_file = Some((request, ix, name.clone()));
        self.core.send(Command::WriteTextFile {
            request,
            fs: pane.fs.clone(),
            path: pane.join(&dir, &name),
            content: String::new(),
            expect_modified: None,
            force: false,
        });
        cx.notify();
    }

    fn delete(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.close_menus();
        if self.panes[ix].selected.is_empty() {
            return;
        }
        if self.confirm_delete != Some(ix) {
            self.confirm_delete = Some(ix);
            cx.notify();
            return;
        }
        self.confirm_delete = None;
        let fs = self.panes[ix].fs.clone();
        let paths = self.selected_paths(ix);
        let pane = &mut self.panes[ix];
        if let Some(f) = pane.found.as_mut() {
            let gone: HashSet<&PathBuf> = paths.iter().collect();
            let posix = pane.fs.is_remote() || cfg!(unix);
            f.hits
                .retain(|h| !gone.contains(&join_path(posix, &h.dir, &h.entry.name)));
            f.selected.clear();
        }
        for path in paths {
            let request = next_id();
            self.op = Some(request);
            self.core.send(Command::FsOp {
                request,
                fs: fs.clone(),
                op: FsOp::Delete(path),
            });
        }
        self.panes[ix].selected.clear();
        cx.notify();
    }

    /// The folder "Terminal" and "Code" use: the one selected folder, else the current one.
    fn run_dir(&self, ix: usize) -> Option<PathBuf> {
        let pane = &self.panes[ix];
        let dir = pane.path.as_ref()?;
        if let [name] = pane.selected.as_slice()
            && pane.entries.iter().any(|e| e.name == *name && e.is_dir())
        {
            return Some(pane.join(dir, name));
        }
        Some(dir.clone())
    }

    fn pane_host(&self, ix: usize) -> Option<ProfileId> {
        match &self.panes[ix].fs {
            FsRef::Host(h) => Some(h.clone()),
            FsRef::Local | FsRef::Conn(_) => None,
        }
    }

    /// Start a shell (`cli` `None`) or a coding CLI in pane `ix`'s folder; with the
    /// worktree option on (a repository on this computer), in a new worktree first.
    fn run_here(&mut self, ix: usize, cli: Option<CliChoice>, cx: &mut Context<Self>) {
        self.close_menus();
        let Some(dir) = self.run_dir(ix) else {
            return;
        };
        let host = self.pane_host(ix);
        if let Some(cli) = &cli
            && self.use_worktree
            && host.is_none()
            && self.panes[ix].git.is_some()
        {
            let request = next_id();
            self.pending_worktree = Some((request, cli.clone()));
            self.core.send(Command::GitWorktree {
                request,
                dir,
                label: cli.name.clone(),
            });
            cx.emit(FilesTabEvent::Toast(format!(
                "Making a worktree for {}…",
                cli.name
            )));
            cx.notify();
            return;
        }
        cx.emit(FilesTabEvent::RunHere {
            host,
            dir,
            cli,
            worktree: None,
        });
        cx.notify();
    }

    /// Copy the selected paths (or the folder's) to the clipboard, one per line.
    fn copy_paths(&mut self, ix: usize, names_only: bool, cx: &mut Context<Self>) {
        let mut paths = self.selected_paths(ix);
        if paths.is_empty() {
            paths.extend(self.panes[ix].path.clone());
        }
        if paths.is_empty() {
            return;
        }
        let text = paths
            .iter()
            .map(|p| {
                if names_only {
                    base_name(p)
                } else {
                    p.to_string_lossy().into_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(text));
        let n = paths.len();
        cx.emit(FilesTabEvent::Toast(match (n, names_only) {
            (1, true) => "Name copied".into(),
            (1, false) => "Path copied".into(),
            (n, true) => format!("{n} names copied"),
            (n, false) => format!("{n} paths copied"),
        }));
    }

    /// How to open a file.
    fn open_path(&mut self, ix: usize, path: PathBuf, how: OpenWith, cx: &mut Context<Self>) {
        let fs = self.panes[ix].fs.clone();
        let here = match how {
            OpenWith::Here => true,
            OpenWith::System => false,
            OpenWith::Default => FileKind::of(&base_name(&path)).opens_here(),
        };
        if here {
            cx.emit(FilesTabEvent::Open { fs, path });
        } else if fs == FsRef::Local {
            cx.open_with_system(&path);
        } else {
            // A remote file opens from a copy in a temp folder (progress in the drawer).
            let dir = switchyard_core::files::open_copy_dir(next_id());
            let id = self.transfers.update(cx, |t, cx| {
                t.start(fs, path, FsRef::Local, Some(dir), OnConflict::Replace, cx)
            });
            self.opening.push(id);
            cx.emit(FilesTabEvent::Toast(
                "Downloading a copy to open in its app…".into(),
            ));
        }
    }

    /// Open the selection: folders navigate, files open per [`OpenWith`].
    fn open_selection(&mut self, ix: usize, how: OpenWith, cx: &mut Context<Self>) {
        self.close_menus();
        if self.panes[ix].found.is_some() {
            return self.open_found(ix, how, false, cx);
        }
        let pane = &self.panes[ix];
        let Some(dir) = pane.path.clone() else { return };
        let picked: Vec<(String, bool)> = pane
            .selected_entries()
            .into_iter()
            .map(|e| (e.name.clone(), e.is_dir()))
            .collect();
        // One folder (the last selected, when several) navigates; files all open.
        if let Some((name, _)) = picked
            .iter()
            .rev()
            .find(|(n, d)| *d && pane.selected.last() == Some(n))
        {
            let full = pane.join(&dir, name);
            return self.navigate(ix, Some(full), cx);
        }
        for (name, is_dir) in picked {
            if !is_dir {
                let full = self.panes[ix].join(&dir, &name);
                self.open_path(ix, full, how, cx);
            }
        }
    }

    /// The selected search result: its folder and entry.
    fn found_cursor(&self, ix: usize) -> Option<(PathBuf, FileEntry)> {
        let pane = &self.panes[ix];
        let f = pane.found.as_ref()?;
        let last = f.selected.last()?;
        f.hits
            .iter()
            .find(|h| pane.join(&h.dir, &h.entry.name) == *last)
            .map(|h| (h.dir.clone(), h.entry.clone()))
    }

    /// Open the selected search results; `containing` opens the folder each is in,
    /// with it selected.
    fn open_found(&mut self, ix: usize, how: OpenWith, containing: bool, cx: &mut Context<Self>) {
        let Some((dir, entry)) = self.found_cursor(ix) else {
            return;
        };
        if containing || entry.is_dir() {
            let (to, select) = if containing {
                (dir, Some(entry.name))
            } else {
                (self.panes[ix].join(&dir, &entry.name), None)
            };
            self.clear_search(ix, cx);
            self.navigate(ix, Some(to), cx);
            if let Some(name) = select {
                let pane = &mut self.panes[ix];
                pane.selected = vec![name.clone()];
                pane.anchor = Some(name);
            }
            return;
        }
        for path in self.selected_paths(ix) {
            let is_file = self.panes[ix].found.as_ref().is_some_and(|f| {
                f.hits.iter().any(|h| {
                    !h.entry.is_dir() && self.panes[ix].join(&h.dir, &h.entry.name) == path
                })
            });
            if is_file {
                self.open_path(ix, path, how, cx);
            }
        }
    }

    /// Select search result `path` (toggle with Ctrl/Cmd, a range with Shift).
    fn select_found(
        &mut self,
        ix: usize,
        path: PathBuf,
        toggle: bool,
        range: bool,
        cx: &mut Context<Self>,
    ) {
        self.active = ix;
        let pane = &mut self.panes[ix];
        let posix = pane.posix();
        let Some(f) = pane.found.as_mut() else { return };
        let full = |h: &FileHit| join_path(posix, &h.dir, &h.entry.name);
        if range
            && let Some(anchor) = f.selected.last().cloned()
            && let (Some(a), Some(b)) = (
                f.hits.iter().position(|h| full(h) == anchor),
                f.hits.iter().position(|h| full(h) == path),
            )
        {
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            f.selected = f.hits[lo..=hi]
                .iter()
                .map(full)
                .filter(|p| *p != path)
                .collect();
            f.selected.push(path);
        } else if toggle {
            if let Some(i) = f.selected.iter().position(|p| *p == path) {
                f.selected.remove(i);
            } else {
                f.selected.push(path);
            }
        } else {
            f.selected = vec![path];
        }
        cx.notify();
    }

    /// Show the selection (or the folder) in the system file manager.
    fn reveal_in_os(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.close_menus();
        if self.panes[ix].fs != FsRef::Local {
            return;
        }
        match self.selected_paths(ix).first() {
            Some(p) => cx.reveal_path(p),
            None => {
                if let Some(d) = &self.panes[ix].path {
                    cx.open_with_system(d);
                }
            }
        }
    }

    /// Pin a folder on this computer to quick access, or unpin it.
    fn toggle_pin(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.close_menus();
        let s = path.to_string_lossy().into_owned();
        if let Some(i) = self.pinned.iter().position(|p| *p == s) {
            self.pinned.remove(i);
        } else {
            self.pinned.push(s);
        }
        self.save_settings();
        cx.notify();
    }

    fn close_menus(&mut self) {
        self.menu = None;
        self.code_menu = None;
    }

    /// Move the selection by `delta` rows (keyboard), keeping it in view.
    fn move_selection(&mut self, delta: isize, extend: bool, cx: &mut Context<Self>) {
        let ix = self.active;
        if self.panes[ix].found.is_some() {
            let pane = &self.panes[ix];
            let Some(f) = pane.found.as_ref() else { return };
            if f.hits.is_empty() {
                return;
            }
            let paths: Vec<PathBuf> = f
                .hits
                .iter()
                .map(|h| pane.join(&h.dir, &h.entry.name))
                .collect();
            let cur = f
                .selected
                .last()
                .and_then(|s| paths.iter().position(|p| p == s));
            let next = match cur {
                None if delta < 0 => paths.len() - 1,
                None => 0,
                Some(c) => (c as isize)
                    .saturating_add(delta)
                    .clamp(0, paths.len() as isize - 1) as usize,
            };
            let path = paths[next].clone();
            self.select_found(ix, path, false, extend, cx);
            self.scroll[ix].scroll_to_item(next, gpui_kit::ScrollStrategy::Nearest);
            return;
        }
        let rows = self.panes[ix].visible();
        if rows.is_empty() {
            return;
        }
        let pane = &mut self.panes[ix];
        let cur = pane
            .selected
            .last()
            .and_then(|n| rows.iter().position(|e| e.name == *n));
        let next = match cur {
            None if delta < 0 => rows.len() - 1,
            None => 0,
            Some(c) => (c as isize)
                .saturating_add(delta)
                .clamp(0, rows.len() as isize - 1) as usize,
        };
        let name = rows[next].name.clone();
        if extend {
            pane.selected.retain(|n| *n != name);
            pane.selected.push(name);
        } else {
            pane.anchor = Some(name.clone());
            pane.selected = vec![name];
        }
        self.confirm_delete = None;
        self.scroll[ix].scroll_to_item(next, gpui_kit::ScrollStrategy::Nearest);
        cx.notify();
    }

    /// Jump to the next row whose name starts with `c` (type-ahead, like the OS).
    fn jump_to_letter(&mut self, c: char, cx: &mut Context<Self>) {
        let ix = self.active;
        let rows = self.panes[ix].visible();
        let lower = c.to_lowercase().to_string();
        let cur = self.panes[ix]
            .selected
            .last()
            .and_then(|n| rows.iter().position(|e| e.name == *n));
        let start = cur.map_or(0, |c| c + 1);
        let hit = (0..rows.len())
            .map(|i| (start + i) % rows.len())
            .find(|&i| rows[i].name.to_lowercase().starts_with(&lower));
        if let Some(i) = hit {
            let name = rows[i].name.clone();
            let pane = &mut self.panes[ix];
            pane.anchor = Some(name.clone());
            pane.selected = vec![name];
            self.scroll[ix].scroll_to_item(i, gpui_kit::ScrollStrategy::Nearest);
            cx.notify();
        }
    }

    /// Keys while the tab itself (not one of its inputs) has focus. `true` when handled.
    fn on_nav_key(
        &mut self,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let ix = self.active;
        let m = &ev.keystroke.modifiers;
        let key = ev.keystroke.key.as_str();
        if self.confirm_delete == Some(ix) {
            match key {
                "enter" => {
                    self.delete(ix, cx);
                    return true;
                }
                "escape" => {
                    self.confirm_delete = None;
                    cx.notify();
                    return true;
                }
                _ => {}
            }
        }
        match key {
            "left" if m.alt => self.go_back(ix, cx),
            "right" if m.alt => self.go_forward(ix, cx),
            "[" if m.secondary() => self.go_back(ix, cx),
            "]" if m.secondary() => self.go_forward(ix, cx),
            "down" => self.move_selection(1, m.shift, cx),
            "up" if m.alt => self.up(ix, cx),
            "up" => self.move_selection(-1, m.shift, cx),
            "pagedown" => self.move_selection(20, m.shift, cx),
            "pageup" => self.move_selection(-20, m.shift, cx),
            "home" => self.move_selection(isize::MIN / 2, false, cx),
            "end" => self.move_selection(isize::MAX / 2, false, cx),
            "enter" if m.secondary() => self.open_selection(ix, OpenWith::System, cx),
            "enter" => self.open_selection(ix, OpenWith::Default, cx),
            "backspace" => self.up(ix, cx),
            "f2" => self.start_rename(ix, window, cx),
            "f5" => self.refresh(ix, cx),
            "delete" => self.delete(ix, cx),
            "tab" if self.layout == Layout::Dual => {
                self.active = 1 - ix;
                cx.notify();
            }
            "escape" => {
                self.panes[ix].selected.clear();
                self.close_menus();
                self.properties = None;
                self.confirm_delete = None;
                cx.notify();
            }
            "a" if m.secondary() => {
                let pane = &mut self.panes[ix];
                if let Some(f) = pane.found.as_mut() {
                    f.selected = f.seen.iter().cloned().collect();
                } else {
                    pane.selected = pane.visible().iter().map(|e| e.name.clone()).collect();
                }
                cx.notify();
            }
            "c" if m.secondary() && m.shift => self.copy_paths(ix, false, cx),
            "c" if m.secondary() => self.clip_selection(ix, false, cx),
            "x" if m.secondary() => self.clip_selection(ix, true, cx),
            "v" if m.secondary() => self.paste(ix, None, cx),
            "n" if m.secondary() && m.shift => self.new_folder(ix, cx),
            "i" if m.secondary() => {
                self.properties = Some(ix);
                cx.notify();
            }
            "`" if m.secondary() && self.panes[ix].has_shell() => self.run_here(ix, None, cx),
            "f" if m.secondary() => self.focus_search(ix, window, cx),
            "l" if m.secondary() => self.open_goto(ix, window, cx),
            k if !m.secondary() && !m.alt && !m.control && k.chars().count() == 1 => {
                let Some(c) = k.chars().next().filter(|c| c.is_alphanumeric()) else {
                    return false;
                };
                self.jump_to_letter(c, cx);
            }
            _ => return false,
        }
        true
    }
}

/// How "Open" treats a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OpenWith {
    /// Here for common files, else the default app.
    Default,
    /// In Switchyard (editor or viewer).
    Here,
    /// The computer's default app.
    System,
}

impl Focusable for FilesTab {
    fn focus_handle(&self, _cx: &gpui_kit::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for FilesTab {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.seen = true;
        let p = palette(cx);
        // A just created folder or file: rename it in place once it is listed.
        if let Some((ix, name)) = self.pending_rename.clone()
            && self.op.is_none()
            && self.panes[ix].entries.iter().any(|e| e.name == name)
        {
            self.pending_rename = None;
            self.rename_entry(ix, name, window, cx);
        }
        let panes = if self.layout == Layout::Dual { 2 } else { 1 };
        for ix in 0..panes {
            self.ensure_search(ix, window, cx);
        }
        let commands = (self.layout != Layout::Sidebar).then(|| self.render_command_bar(&p, cx));
        let places = (self.layout != Layout::Sidebar && self.show_places)
            .then(|| self.render_places(&p, cx));
        let left = self.render_pane(0, &p, cx);
        let right = (self.layout == Layout::Dual).then(|| self.render_pane(1, &p, cx));
        let busy = !self.transfers.read(cx).items.is_empty();
        let drawer = (self.layout != Layout::Sidebar && busy)
            .then(|| self.transfers.update(cx, |t, cx| t.render_drawer(&p, cx)));
        let menu = self.render_menu(&p, cx);
        let code_menu = self.render_code_menu(&p, cx);
        let properties = self.render_properties(&p, cx);
        div()
            .key_context("FilesTab")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                let ix = this.active;
                if this.rename.is_some() {
                    if ev.keystroke.key == "escape" {
                        this.cancel_rename(window, cx);
                        cx.stop_propagation();
                    }
                    return;
                }
                if this.focus.is_focused(window) && this.on_nav_key(ev, window, cx) {
                    cx.stop_propagation();
                    return;
                }
                // In the search box: Escape clears it, Down moves into the list.
                let in_search = this.search[ix]
                    .as_ref()
                    .is_some_and(|(i, _)| i.read(cx).focus_handle(cx).is_focused(window));
                if in_search {
                    match ev.keystroke.key.as_str() {
                        "escape" => {
                            this.clear_search(ix, cx);
                            window.focus(&this.focus, cx);
                        }
                        "down" => {
                            window.focus(&this.focus, cx);
                            this.move_selection(1, false, cx);
                        }
                        _ => {}
                    }
                }
            }))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(p.bg)
            .children(commands)
            .when_some(self.error.clone(), |d, e| {
                d.child(
                    div()
                        .id("files-error")
                        .px(rpx(12.))
                        .py(rpx(5.))
                        .flex()
                        .items_center()
                        .gap(rpx(8.))
                        .bg(p.prod_bg)
                        .text_color(p.prod)
                        .text_size(ts::BODY)
                        .child(div().flex_1().min_w_0().child(e))
                        .child(
                            div()
                                .id("files-error-close")
                                .cursor_pointer()
                                .px(rpx(4.))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.error = None;
                                    cx.notify();
                                }))
                                .child("✕"),
                        ),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .children(places)
                    .child(left)
                    .children(right),
            )
            .children(drawer)
            .children(menu)
            .children(code_menu)
            .children(properties)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use switchyard_core::remote::EntryKind;

    fn pane(entries: Vec<(&str, bool, u64)>) -> Pane {
        let mut p = Pane::new(FsRef::Local, "x".into());
        p.path = Some(PathBuf::from("/home/swy/app"));
        p.entries = entries
            .into_iter()
            .map(|(n, dir, size)| FileEntry {
                name: n.into(),
                kind: if dir { EntryKind::Dir } else { EntryKind::File },
                size,
                modified_ms: Some(size as i64),
                mode: None,
            })
            .collect();
        p
    }

    fn names(p: &Pane) -> Vec<String> {
        p.visible().iter().map(|e| e.name.clone()).collect()
    }

    #[test]
    fn sorting_keeps_folders_first() {
        let mut p = pane(vec![
            ("b.txt", false, 10),
            ("a.txt", false, 30),
            ("zdir", true, 0),
            (".env", false, 5),
        ]);
        assert_eq!(names(&p), ["zdir", "a.txt", "b.txt"]);
        p.sort = (SortBy::Size, false);
        assert_eq!(names(&p), ["zdir", "a.txt", "b.txt"]);
        p.sort = (SortBy::Size, true);
        p.show_hidden = true;
        assert_eq!(names(&p), ["zdir", ".env", "b.txt", "a.txt"]);
    }

    #[test]
    fn sorting_by_type_groups_kinds() {
        let mut p = pane(vec![
            ("z.png", false, 1),
            ("a.rs", false, 1),
            ("b.png", false, 1),
            ("docs", true, 0),
        ]);
        p.sort = (SortBy::Type, true);
        assert_eq!(names(&p), ["docs", "b.png", "z.png", "a.rs"]);
    }

    #[test]
    fn rows_are_cached_until_something_changes() {
        let mut p = pane(vec![("a", false, 1), ("b", false, 2)]);
        let first = p.visible();
        assert!(Rc::ptr_eq(&first, &p.visible()));
        p.entries
            .push(pane(vec![("c", false, 3)]).entries.remove(0));
        // Same key: still the old rows until the pane is touched.
        assert_eq!(names(&p), ["a", "b"]);
        p.touch();
        assert_eq!(names(&p), ["a", "b", "c"]);
        p.filter = "b".into();
        assert_eq!(names(&p), ["b"]);
    }

    #[test]
    fn search_filters_loaded_names_in_any_case() {
        let mut p = pane(vec![
            ("Report-2024.csv", false, 1),
            ("reports", true, 0),
            ("notes.txt", false, 2),
        ]);
        p.filter = " REPORT ".into();
        assert_eq!(names(&p), ["reports", "Report-2024.csv"]);
        assert!(!p.paged());
        p.next = Some("marker".into());
        assert!(p.paged());
    }

    #[test]
    fn breadcrumbs_and_parents() {
        let p = pane(vec![]);
        let c: Vec<_> = p.crumbs().into_iter().map(|(l, _)| l).collect();
        assert_eq!(c, ["/", "home", "swy", "app"]);
        assert_eq!(p.crumbs()[2].1, PathBuf::from("/home/swy"));
        assert_eq!(fmt_time(86_400_000 + 3_723_000), "1970-01-02 01:02");
        assert_eq!(
            p.parent_of(Path::new("/home/swy/app")),
            Some(PathBuf::from("/home/swy"))
        );
        assert_eq!(p.parent_of(Path::new("/home")), Some(PathBuf::from("/")));
        assert_eq!(p.parent_of(Path::new("/")), None);
    }

    #[test]
    fn new_names_do_not_clash() {
        let p = pane(vec![("New folder", true, 0), ("new folder (2)", true, 0)]);
        assert_eq!(p.unused_name("New folder", ""), "New folder (3)");
        assert_eq!(p.unused_name("Untitled", ".txt"), "Untitled.txt");
        assert_eq!(base_name(Path::new("/a/b/report.pdf")), "report.pdf");
        assert_eq!(base_name(Path::new("C:\\Users\\me\\")), "me");
    }

    #[test]
    fn settings_round_trip_with_defaults() {
        let s: FilesSettings = serde_json::from_str("{}").unwrap_or_default();
        assert_eq!(s, FilesSettings::default());
        let s = FilesSettings {
            dual: true,
            pinned: vec!["/srv".into()],
            show_hidden: true,
            hide_places: false,
        };
        let back: FilesSettings =
            serde_json::from_value(serde_json::to_value(&s).unwrap_or_default())
                .unwrap_or_default();
        assert_eq!(back, s);
    }
}
