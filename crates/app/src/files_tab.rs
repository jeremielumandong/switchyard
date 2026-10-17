//! Files tab: dual pane (this computer on the left, a Host or this computer on the right),
//! breadcrumbs, sortable columns, hidden-file toggle, new folder / rename / delete, drag
//! between panes and from the OS, and the transfer drawer at the bottom.
//!
//! Folders load a page at a time: object storage (Azure Blob, S3, R2) answers [`PAGE`] entries
//! and a "Load more" footer fetches the next page; other file systems answer in one page.
//! Search filters the loaded rows as you type; Enter also searches the server by name
//! prefix when the folder has more pages than are loaded.
//!
//! The keyboard drives it like a file manager (arrows, Enter, Backspace, F2, Delete, F5,
//! Tab between panes, Ctrl/Cmd+A, Ctrl/Cmd+Shift+C copies paths). A folder in a git
//! repository on this computer shows its branch and what changed (badges per row, a
//! "Changes" filter) and refreshes while the tab is on screen, so a coding CLI's edits
//! show up as they happen. "Terminal" and "Code" start a shell or a coding CLI in the
//! folder (on this computer or the Host), optionally in a new git worktree.

use std::path::{Path, PathBuf};

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, EventEmitter, ExternalPaths, FocusHandle,
    Focusable, FontWeight, InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement as _,
    Render, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Window,
    deferred, div, px, uniform_list,
};
use switchyard_core::git::{ChangeKind, GitStatus};
use switchyard_core::remote::{EntryKind, FileEntry};
use switchyard_core::store::ProfileId;
use switchyard_core::{Command, Event, FsOp, FsRef, OnConflict, RequestId, RuntimeHandle};

use crate::app_state::next_id;
use crate::appearance::{rpx, ts};
use crate::coding_clis::CliChoice;
use crate::remote_files::human;
use crate::theme::{MONO, Palette, palette};
use crate::transfers::Transfers;
use crate::ui::{self, Kind};

/// Entries per page on file systems that page (object storage).
const PAGE: u32 = 100;
/// How often a folder on this computer is re-read while the tab is on screen.
const LIVE_EVERY: std::time::Duration = std::time::Duration::from_secs(3);

/// What the tab asks the workspace to do.
pub enum FilesTabEvent {
    /// Open a file in an editor tab.
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
    /// One pane, for browsing.
    Single,
    /// One narrow pane beside a coding terminal: names and git badges only.
    Sidebar,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SortBy {
    Name,
    Size,
    Modified,
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
            .rounded(px(5.))
            .bg(p.elev)
            .border_1()
            .border_color(p.acc)
            .text_size(ts::BODY)
            .text_color(p.fg)
            .child(self.0.clone())
    }
}

struct Pane {
    fs: FsRef,
    label: String,
    path: Option<PathBuf>,
    entries: Vec<FileEntry>,
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
    last_click: Option<(String, std::time::Instant)>,
    /// The folder's git status (this computer only).
    git: Option<GitStatus>,
    git_request: Option<RequestId>,
    /// Show only changed entries.
    changes_only: bool,
    /// The pending listing is a background refresh: no loading bar.
    quiet: bool,
}

impl Pane {
    fn new(fs: FsRef, label: String) -> Self {
        Self {
            fs,
            label,
            path: None,
            entries: Vec::new(),
            request: None,
            appending: false,
            next: None,
            prefix: String::new(),
            filter: String::new(),
            error: None,
            sort: (SortBy::Name, true),
            show_hidden: false,
            selected: Vec::new(),
            last_click: None,
            git: None,
            git_request: None,
            changes_only: false,
            quiet: false,
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
        if self.posix() {
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

    fn visible(&self) -> Vec<FileEntry> {
        let (by, asc) = self.sort;
        let needle = self.filter.trim().to_lowercase();
        // Lowercase each name once, not once per comparison.
        let mut v: Vec<(String, &FileEntry)> = self
            .entries
            .iter()
            .filter(|e| self.show_hidden || !e.is_hidden())
            .filter(|e| needle.is_empty() || e.name.to_lowercase().contains(&needle))
            .filter(|e| !self.changes_only || self.change(&e.name).is_some())
            .map(|e| match by {
                SortBy::Name => (e.name.to_lowercase(), e),
                _ => (String::new(), e),
            })
            .collect();
        v.sort_by(|(ka, a), (kb, b)| {
            // Folders stay on top whatever the column.
            let dirs = b.is_dir().cmp(&a.is_dir());
            let ord = match by {
                SortBy::Name => ka.cmp(kb),
                SortBy::Size => a.size.cmp(&b.size),
                SortBy::Modified => a.modified_ms.cmp(&b.modified_ms),
            };
            dirs.then(if asc { ord } else { ord.reverse() })
        });
        v.into_iter().map(|(_, e)| e.clone()).collect()
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
}

enum Edit {
    NewFolder,
    Rename(String),
}

/// The files tab.
pub struct FilesTab {
    core: RuntimeHandle,
    transfers: Entity<Transfers>,
    panes: [Pane; 2],
    active: usize,
    /// What the right pane can show: Hosts (SFTP) and FTP connections, with names.
    sources: Vec<Source>,
    picker_open: bool,
    edit: Option<(usize, Edit, Entity<InputState>)>,
    /// A pane's "go to folder" box while it is open.
    goto: Option<(usize, Entity<InputState>, Subscription)>,
    /// Each pane's search box while it is open.
    search: [Option<(Entity<InputState>, Subscription)>; 2],
    confirm_delete: Option<usize>,
    op: Option<RequestId>,
    error: Option<String>,
    focus: FocusHandle,
    /// Coding CLIs the "Code" menu offers.
    clis: Vec<CliChoice>,
    /// Pane whose "Code" menu is open.
    code_menu: Option<usize>,
    /// Make a new git worktree for the next coding CLI run.
    use_worktree: bool,
    /// A worktree being made for a run: (request, CLI).
    pending_worktree: Option<(RequestId, CliChoice)>,
    /// Rendered since the last live refresh: the tab is on screen.
    seen: bool,
    scroll: [gpui_kit::UniformListScrollHandle; 2],
    layout: Layout,
    _live: gpui_kit::Task<()>,
}

impl EventEmitter<FilesTabEvent> for FilesTab {}

/// A file system the right pane can show: where, its name and its badge.
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

impl FilesTab {
    /// Local on the left; `right` is a Host, an FTP connection or this computer.
    pub fn new(
        core: RuntimeHandle,
        transfers: Entity<Transfers>,
        sources: Vec<Source>,
        right: FsRef,
        cx: &mut Context<Self>,
    ) -> Self {
        let right_pane = Pane::new(right.clone(), source_name(&sources, &right));
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
            panes: [Pane::new(FsRef::Local, "This computer".into()), right_pane],
            active: 0,
            sources,
            picker_open: false,
            edit: None,
            goto: None,
            search: [None, None],
            confirm_delete: None,
            op: None,
            error: None,
            focus: cx.focus_handle(),
            clis: Vec::new(),
            code_menu: None,
            use_worktree: false,
            pending_worktree: None,
            seen: true,
            scroll: Default::default(),
            layout: Layout::Dual,
            _live: live,
        };
        this.list(0, None, cx);
        this.list(1, None, cx);
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
        let mut this = Self::new(core, transfers, Vec::new(), FsRef::Local, cx);
        this.layout = Layout::Sidebar;
        // `new` listed home in both panes; the right pane is unused here.
        this.panes[1].request = None;
        let label = crate::coding_clis::folder_name(&dir);
        this.panes[0] = Pane::new(fs, label);
        this.list(0, Some(dir), cx);
        this
    }

    /// The Host on the right, if any.
    pub fn right_host(&self) -> Option<&ProfileId> {
        match &self.panes[1].fs {
            FsRef::Host(h) => Some(h),
            FsRef::Local | FsRef::Conn(_) => None,
        }
    }

    /// What the right pane shows.
    pub fn right_source(&self) -> &FsRef {
        &self.panes[1].fs
    }

    /// Point the right pane at a Host, an FTP connection or this computer.
    pub fn show(&mut self, fs: FsRef, cx: &mut Context<Self>) {
        self.picker_open = false;
        let name = source_name(&self.sources, &fs);
        self.panes[1] = Pane::new(fs, name);
        self.search[1] = None;
        self.list(1, None, cx);
    }

    /// Hosts, FTP and cloud storage connections for the right pane's picker.
    pub fn set_sources(&mut self, sources: Vec<Source>, cx: &mut Context<Self>) {
        if self.panes[1].fs.is_remote()
            && let Some((_, n, _)) = sources.iter().find(|(fs, _, _)| *fs == self.panes[1].fs)
        {
            self.panes[1].label = n.clone();
        }
        self.sources = sources;
        cx.notify();
    }

    /// The coding CLIs for the "Code" menu.
    pub fn set_clis(&mut self, clis: Vec<CliChoice>, cx: &mut Context<Self>) {
        self.clis = clis;
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
                    || self.edit.as_ref().is_some_and(|(i, ..)| *i == ix);
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
        let pane = &mut self.panes[ix];
        pane.prefix.clear();
        pane.filter.clear();
        self.search[ix] = None;
        self.fetch(ix, path, None, cx);
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

    /// Show the search box under pane `ix`'s path bar and focus it.
    fn open_search(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((input, _)) = &self.search[ix] {
            input.update(cx, |i, cx| i.focus(window, cx));
            return;
        }
        let current = self.panes[ix].filter.clone();
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Search by name")
                .default_value(current)
        });
        let sub = cx.subscribe_in(
            &input,
            window,
            move |this, input, ev: &InputEvent, _, cx| match ev {
                InputEvent::Change => {
                    this.panes[ix].filter = input.read(cx).value().to_string();
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => this.search_server(ix, cx),
                _ => {}
            },
        );
        input.update(cx, |i, cx| i.focus(window, cx));
        self.search[ix] = Some((input, sub));
        cx.notify();
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

    /// Close the search box and drop the search (re-listing if the server applied it).
    fn close_search(&mut self, ix: usize, cx: &mut Context<Self>) {
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
                    }
                    Ok(entries) => {
                        if pane.path.as_ref() != Some(path) {
                            pane.selected.clear();
                            pane.git = None;
                            pane.changes_only = false;
                        }
                        pane.path = Some(path.clone());
                        pane.entries = entries.clone();
                        pane.next = next.clone();
                        pane.error = None;
                        // Drop selected names that are gone.
                        let names: std::collections::HashSet<&str> =
                            entries.iter().map(|e| e.name.as_str()).collect();
                        pane.selected.retain(|n| names.contains(n.as_str()));
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
                    pane.git = result.clone().ok().flatten();
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
            Event::TransferDone { result: Ok(_), .. } => {
                self.refresh(0, cx);
                self.refresh(1, cx);
            }
            _ => return,
        }
        cx.notify();
    }

    /// Copy `paths` from `from` into the folder shown in pane `to_ix`.
    fn copy_into(
        &mut self,
        from: FsRef,
        paths: Vec<PathBuf>,
        to_ix: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(dir) = self.panes[to_ix].path.clone() else {
            return;
        };
        let to = self.panes[to_ix].fs.clone();
        self.transfers.update(cx, |t, cx| {
            for p in paths {
                t.start(
                    from.clone(),
                    p,
                    to.clone(),
                    Some(dir.clone()),
                    OnConflict::Ask,
                    cx,
                );
            }
        });
    }

    fn selected_paths(&self, ix: usize) -> Vec<PathBuf> {
        let pane = &self.panes[ix];
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
        self.copy_into(from, paths, 1 - ix, cx);
    }

    fn click(&mut self, ix: usize, e: &FileEntry, toggle: bool, cx: &mut Context<Self>) {
        self.active = ix;
        self.confirm_delete = None;
        let now = std::time::Instant::now();
        let pane = &mut self.panes[ix];
        let double = pane
            .last_click
            .as_ref()
            .is_some_and(|(n, t)| *n == e.name && now.duration_since(*t).as_millis() < 450);
        pane.last_click = Some((e.name.clone(), now));
        if toggle {
            if let Some(i) = pane.selected.iter().position(|n| *n == e.name) {
                pane.selected.remove(i);
            } else {
                pane.selected.push(e.name.clone());
            }
        } else {
            pane.selected = vec![e.name.clone()];
        }
        if double && let Some(dir) = pane.path.clone() {
            let full = pane.join(&dir, &e.name);
            if e.is_dir() {
                self.list(ix, Some(full), cx);
            } else {
                let fs = pane.fs.clone();
                cx.emit(FilesTabEvent::Open { fs, path: full });
            }
        }
        cx.notify();
    }

    fn up(&mut self, ix: usize, cx: &mut Context<Self>) {
        let up = self.panes[ix].path.as_ref().and_then(|p| {
            let s = p.to_string_lossy().replace('\\', "/");
            let s = s.trim_end_matches('/').to_owned();
            if self.panes[ix].posix() {
                match s.rfind('/') {
                    Some(0) if s.len() > 1 => Some(PathBuf::from("/")),
                    Some(i) if i > 0 => Some(PathBuf::from(&s[..i])),
                    _ => None,
                }
            } else {
                p.parent().map(Path::to_path_buf)
            }
        });
        if let Some(up) = up {
            self.list(ix, Some(up), cx);
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
            move |this, input, ev: &InputEvent, _, cx| match ev {
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
                        this.list(ix, Some(target), cx);
                    }
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

    fn start_edit(&mut self, ix: usize, rename: bool, window: &mut Window, cx: &mut Context<Self>) {
        let (edit, value) = if rename {
            let Some(name) = self.panes[ix].selected.first().cloned() else {
                return;
            };
            (Edit::Rename(name.clone()), name)
        } else {
            (Edit::NewFolder, String::new())
        };
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Folder name")
                .default_value(value)
        });
        input.update(cx, |i, cx| i.focus(window, cx));
        self.edit = Some((ix, edit, input));
        cx.notify();
    }

    fn finish_edit(&mut self, cx: &mut Context<Self>) {
        let Some((ix, edit, input)) = self.edit.take() else {
            return;
        };
        let name = input.read(cx).value().trim().to_owned();
        let pane = &self.panes[ix];
        let Some(dir) = pane.path.clone() else { return };
        if name.is_empty() || name.contains('/') {
            cx.notify();
            return;
        }
        let op = match edit {
            Edit::NewFolder => FsOp::Mkdir(pane.join(&dir, &name)),
            Edit::Rename(old) if old != name => {
                FsOp::Rename(pane.join(&dir, &old), pane.join(&dir, &name))
            }
            Edit::Rename(_) => {
                cx.notify();
                return;
            }
        };
        let request = next_id();
        self.op = Some(request);
        self.core.send(Command::FsOp {
            request,
            fs: pane.fs.clone(),
            op,
        });
        cx.notify();
    }

    fn delete(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.confirm_delete != Some(ix) {
            self.confirm_delete = Some(ix);
            cx.notify();
            return;
        }
        self.confirm_delete = None;
        let fs = self.panes[ix].fs.clone();
        for path in self.selected_paths(ix) {
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
        self.code_menu = None;
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
    fn copy_paths(&mut self, ix: usize, cx: &mut Context<Self>) {
        let mut paths = self.selected_paths(ix);
        if paths.is_empty() {
            paths.extend(self.panes[ix].path.clone());
        }
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
        cx.emit(FilesTabEvent::Toast(if n == 1 {
            "Path copied".into()
        } else {
            format!("{n} paths copied")
        }));
    }

    /// Open the selection on this computer with its default app.
    fn open_with_system(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.panes[ix].fs != FsRef::Local {
            return;
        }
        for p in self.selected_paths(ix) {
            cx.open_with_system(&p);
        }
    }

    /// Show the selection (or the folder) in the system file manager.
    fn reveal_in_os(&mut self, ix: usize, cx: &mut Context<Self>) {
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

    /// Move the selection by `delta` rows (keyboard), keeping it in view.
    fn move_selection(&mut self, delta: isize, extend: bool, cx: &mut Context<Self>) {
        let ix = self.active;
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
            Some(c) => (c as isize + delta).clamp(0, rows.len() as isize - 1) as usize,
        };
        let name = rows[next].name.clone();
        if extend {
            pane.selected.retain(|n| *n != name);
            pane.selected.push(name);
        } else {
            pane.selected = vec![name];
        }
        self.confirm_delete = None;
        self.scroll[ix].scroll_to_item(next, gpui_kit::ScrollStrategy::Nearest);
        cx.notify();
    }

    /// Enter: open the selected folder or file.
    fn open_selected(&mut self, cx: &mut Context<Self>) {
        let ix = self.active;
        let pane = &self.panes[ix];
        let (Some(dir), Some(name)) = (pane.path.clone(), pane.selected.last().cloned()) else {
            return;
        };
        let Some(entry) = pane.entries.iter().find(|e| e.name == name) else {
            return;
        };
        let full = pane.join(&dir, &name);
        if entry.is_dir() {
            self.list(ix, Some(full), cx);
        } else {
            let fs = pane.fs.clone();
            cx.emit(FilesTabEvent::Open { fs, path: full });
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
        match key {
            "down" => self.move_selection(1, m.shift, cx),
            "up" if m.alt => self.up(ix, cx),
            "up" => self.move_selection(-1, m.shift, cx),
            "pagedown" => self.move_selection(20, m.shift, cx),
            "pageup" => self.move_selection(-20, m.shift, cx),
            "home" => self.move_selection(isize::MIN / 2, false, cx),
            "end" => self.move_selection(isize::MAX / 2, false, cx),
            "enter" => self.open_selected(cx),
            "backspace" => self.up(ix, cx),
            "f2" => self.start_edit(ix, true, window, cx),
            "f5" => self.refresh(ix, cx),
            "delete" => self.delete(ix, cx),
            "tab" if self.layout == Layout::Dual => {
                self.active = 1 - ix;
                cx.notify();
            }
            "escape" => {
                self.panes[ix].selected.clear();
                self.code_menu = None;
                self.confirm_delete = None;
                cx.notify();
            }
            "a" if m.secondary() => {
                let names = self.panes[ix]
                    .visible()
                    .into_iter()
                    .map(|e| e.name)
                    .collect();
                self.panes[ix].selected = names;
                cx.notify();
            }
            "c" if m.secondary() && m.shift => self.copy_paths(ix, cx),
            "`" if m.secondary() && self.panes[ix].has_shell() => self.run_here(ix, None, cx),
            "f" if m.secondary() => self.open_search(ix, window, cx),
            "l" if m.secondary() => self.open_goto(ix, window, cx),
            _ => return false,
        }
        true
    }

    fn render_pane(&mut self, ix: usize, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let pane = &self.panes[ix];
        let rows = pane.visible();
        let count = rows.len();
        // A background refresh (live folders) shows no loading state.
        let loading = pane.request.is_some() && !pane.quiet;
        let active = self.active == ix;
        let n_sel = pane.selected.len();
        let full = self.layout != Layout::Sidebar;
        let tool = |id: String, label: &'static str| {
            div()
                .id(SharedString::from(id))
                .h(rpx(22.))
                .px(rpx(7.))
                .flex()
                .items_center()
                .rounded(px(4.))
                .text_size(ts::LABEL)
                .text_color(p.fg2)
                .hover(|s| s.bg(p.hover))
                .child(label)
        };
        let arrow = if ix == 0 { "Copy →" } else { "← Copy" };
        let source: AnyElement = if ix == 0 {
            div()
                .text_size(ts::BODY)
                .font_weight(FontWeight::SEMIBOLD)
                .child(pane.label.clone())
                .into_any_element()
        } else {
            div()
                .id("files-source")
                .flex()
                .items_center()
                .gap(rpx(4.))
                .px(rpx(6.))
                .h(rpx(22.))
                .rounded(px(4.))
                .border_1()
                .border_color(p.bd2)
                .text_size(ts::BODY)
                .font_weight(FontWeight::SEMIBOLD)
                .hover(|s| s.bg(p.hover))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.picker_open = !this.picker_open;
                    cx.notify();
                }))
                .child(pane.label.clone())
                .child(div().text_color(p.fg3).text_size(ts::TINY).child("▾"))
                .into_any_element()
        };
        let toolbar =
            div()
                .h(rpx(32.))
                .flex_none()
                .px(rpx(8.))
                .flex()
                .items_center()
                .gap(rpx(2.))
                .border_b_1()
                .border_color(p.bd)
                .bg(p.panel)
                .child(source)
                .when(full && pane.has_shell() && pane.path.is_some(), |d| {
                    d.child(div().w(rpx(6.)))
                        .child(
                            tool(format!("f{ix}-term"), ">_ Terminal")
                                .tooltip(|window, cx| {
                                    gpui_kit::component::tooltip::Tooltip::new(
                                        "A terminal in this folder (or the selected one), \
                                         files beside it · Ctrl/Cmd+`",
                                    )
                                    .build(window, cx)
                                })
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.run_here(ix, None, cx)),
                                ),
                        )
                        .child(
                            tool(format!("f{ix}-code"), "Code ▾")
                                .text_color(p.acc)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.code_menu = if this.code_menu == Some(ix) {
                                        None
                                    } else {
                                        Some(ix)
                                    };
                                    cx.notify();
                                })),
                        )
                })
                .child(div().flex_1())
                .child(
                    tool(format!("f{ix}-up"), "↑")
                        .on_click(cx.listener(move |this, _, _, cx| this.up(ix, cx))),
                )
                .child(
                    tool(format!("f{ix}-refresh"), "⟳")
                        .on_click(cx.listener(move |this, _, _, cx| this.refresh(ix, cx))),
                )
                .when(ix == 0 && full, |d| {
                    let single = self.layout == Layout::Single;
                    d.child(
                        tool(
                            format!("f{ix}-layout"),
                            if single { "2 panes" } else { "1 pane" },
                        )
                        .tooltip(move |window, cx| {
                            gpui_kit::component::tooltip::Tooltip::new(if single {
                                "Two panes (copy between computers)"
                            } else {
                                "One pane"
                            })
                            .build(window, cx)
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.layout = if this.layout == Layout::Single {
                                Layout::Dual
                            } else {
                                Layout::Single
                            };
                            this.active = 0;
                            cx.notify();
                        })),
                    )
                })
                .child(
                    tool(
                        format!("f{ix}-search"),
                        if pane.filter.is_empty() {
                            "Search"
                        } else {
                            "Search ●"
                        },
                    )
                    .on_click(cx.listener(move |this, _, w, cx| this.open_search(ix, w, cx))),
                )
                .when(full, |d| {
                    d.child(
                        tool(
                            format!("f{ix}-hidden"),
                            if pane.show_hidden {
                                "Hidden ●"
                            } else {
                                "Hidden ○"
                            },
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.panes[ix].show_hidden = !this.panes[ix].show_hidden;
                            cx.notify();
                        })),
                    )
                })
                .when(full, |d| {
                    d.child(tool(format!("f{ix}-mkdir"), "New folder").on_click(
                        cx.listener(move |this, _, w, cx| this.start_edit(ix, false, w, cx)),
                    ))
                })
                .when(full && n_sel == 1, |d| {
                    d.child(tool(format!("f{ix}-rename"), "Rename").on_click(
                        cx.listener(move |this, _, w, cx| this.start_edit(ix, true, w, cx)),
                    ))
                })
                .when(full && n_sel > 0, |d| {
                    let confirming = self.confirm_delete == Some(ix);
                    d.child(
                        tool(
                            format!("f{ix}-delete"),
                            if confirming { "Delete?" } else { "Delete" },
                        )
                        .when(confirming, |t| t.text_color(p.prod))
                        .on_click(cx.listener(move |this, _, _, cx| this.delete(ix, cx))),
                    )
                    .when(self.layout == Layout::Dual, |d| {
                        d.child(
                            ui::button(
                                SharedString::from(format!("f{ix}-copy")),
                                arrow,
                                Kind::Primary,
                                p,
                            )
                            .h(rpx(22.))
                            .text_size(ts::LABEL)
                            .on_click(
                                cx.listener(move |this, _, _, cx| {
                                    this.copy_selection_across(ix, cx)
                                }),
                            ),
                        )
                    })
                });
        let crumbs = pane.crumbs();
        let goto = self
            .goto
            .as_ref()
            .filter(|(i, ..)| *i == ix)
            .map(|(_, input, _)| input.clone());
        let crumb_bar = div()
            .id(SharedString::from(format!("f{ix}-path")))
            .h(rpx(26.))
            .flex_none()
            .px(rpx(10.))
            .flex()
            .items_center()
            .gap(rpx(2.))
            .overflow_hidden()
            .border_b_1()
            .border_color(p.bd)
            .font_family(MONO)
            .text_size(ts::SMALL)
            .when_some(goto, |d, input| {
                d.child(
                    div()
                        .flex_1()
                        .h(rpx(20.))
                        .flex()
                        .items_center()
                        .px(rpx(6.))
                        .border_1()
                        .border_color(p.acc)
                        .rounded(px(4.))
                        .child(Input::new(&input).appearance(false).text_size(ts::LABEL)),
                )
            })
            .when(self.goto.as_ref().is_none_or(|(i, ..)| *i != ix), |d| {
                d.cursor_text()
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.open_goto(ix, window, cx)),
                    )
                    .children(crumbs.into_iter().enumerate().map(|(i, (label, path))| {
                        div()
                            .flex()
                            .items_center()
                            .gap(rpx(2.))
                            .when(i > 1, |d| d.child(div().text_color(p.fg3).child("/")))
                            .child(
                                div()
                                    .id(SharedString::from(format!("f{ix}-crumb-{i}")))
                                    .px(rpx(3.))
                                    .rounded(px(3.))
                                    .text_color(p.fg2)
                                    .hover(|s| s.bg(p.hover).text_color(p.fg))
                                    .cursor_pointer()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.list(ix, Some(path.clone()), cx)
                                    }))
                                    .child(label),
                            )
                    }))
                    .child(div().flex_1().h_full())
                    .child(
                        div()
                            .id(SharedString::from(format!("f{ix}-home")))
                            .px(rpx(5.))
                            .rounded(px(3.))
                            .cursor_pointer()
                            .text_color(p.fg3)
                            .hover(|s| s.bg(p.hover).text_color(p.fg))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.list(ix, Some(PathBuf::from("~")), cx)
                            }))
                            .child("~"),
                    )
            });
        let (by, asc) = pane.sort;
        let head = |id: &'static str, label: &'static str, col: SortBy| {
            let mark = if by == col {
                if asc { " ↑" } else { " ↓" }
            } else {
                ""
            };
            div()
                .id(SharedString::from(format!("f{ix}-{id}")))
                .hover(|s| s.text_color(p.fg))
                .on_click(cx.listener(move |this, _, _, cx| {
                    let s = &mut this.panes[ix].sort;
                    *s = if s.0 == col { (col, !s.1) } else { (col, true) };
                    cx.notify();
                }))
                .child(format!("{label}{mark}"))
        };
        let header = div()
            .h(rpx(24.))
            .flex_none()
            .flex()
            .items_center()
            .px(rpx(10.))
            .gap(rpx(8.))
            .text_size(ts::SMALL)
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(p.fg2)
            .border_b_1()
            .border_color(p.bd)
            .child(
                div()
                    .flex_1()
                    .pl(rpx(18.))
                    .child(head("sort-name", "Name", SortBy::Name)),
            )
            .when(full, |d| {
                d.child(div().w(rpx(72.)).flex().justify_end().child(head(
                    "sort-size",
                    "Size",
                    SortBy::Size,
                )))
                .child(
                    div()
                        .w(rpx(118.))
                        .child(head("sort-mod", "Modified", SortBy::Modified)),
                )
                .child(div().w(rpx(76.)).child("Mode"))
            });

        let search_row: Option<AnyElement> = self.search[ix].as_ref().map(|(input, _)| {
            let filter = pane.filter.trim();
            let hint: Option<String> = if !pane.prefix.is_empty() && filter == pane.prefix {
                Some(format!("Names starting with “{}”", pane.prefix))
            } else if pane.paged() && !filter.is_empty() {
                Some("Enter searches the whole folder".into())
            } else {
                None
            };
            div()
                .flex_none()
                .px(rpx(10.))
                .py(rpx(4.))
                .flex()
                .items_center()
                .gap(rpx(6.))
                .border_b_1()
                .border_color(p.bd)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h(rpx(22.))
                        .flex()
                        .items_center()
                        .px(rpx(7.))
                        .border_1()
                        .border_color(p.bd2)
                        .rounded(px(4.))
                        .child(Input::new(input).appearance(false).text_size(ts::BODY)),
                )
                .children(hint.map(|h| {
                    div()
                        .flex_none()
                        .text_size(ts::SMALL)
                        .text_color(p.fg3)
                        .child(h)
                }))
                .child(
                    tool(format!("f{ix}-search-close"), "✕")
                        .on_click(cx.listener(move |this, _, _, cx| this.close_search(ix, cx))),
                )
                .into_any_element()
        });
        let loading_bar = (loading && !pane.appending && pane.path.is_some())
            .then(|| ui::loading_bar(SharedString::from(format!("f{ix}-loading")), p));
        let footer: Option<AnyElement> = pane.path.is_some().then(|| {
            let loaded = pane.entries.len();
            let more = if pane.next.is_some() { "+" } else { "" };
            let status: AnyElement = if loading {
                div()
                    .flex()
                    .items_center()
                    .gap(rpx(6.))
                    .child(ui::pulse_dot(
                        SharedString::from(format!("f{ix}-busy")),
                        p.acc,
                        6.,
                    ))
                    .child(if pane.appending {
                        "Loading more…"
                    } else {
                        "Loading…"
                    })
                    .into_any_element()
            } else if pane.filter.trim().is_empty() {
                format!("{loaded}{more} items").into_any_element()
            } else {
                format!("{count} of {loaded}{more} items").into_any_element()
            };
            div()
                .h(rpx(28.))
                .flex_none()
                .px(rpx(10.))
                .flex()
                .items_center()
                .gap(rpx(8.))
                .border_t_1()
                .border_color(p.bd)
                .text_size(ts::SMALL)
                .text_color(p.fg3)
                .overflow_hidden()
                // Beside a terminal the footer has room for git only (or loading).
                .when(full || loading, |d| d.child(status))
                .when(full && n_sel > 0, |d| {
                    let local = pane.fs == FsRef::Local;
                    d.child(div().text_color(p.fg3).child(format!("· {n_sel} selected")))
                        .child(
                            tool(format!("f{ix}-copypath"), "Copy path").on_click(
                                cx.listener(move |this, _, _, cx| this.copy_paths(ix, cx)),
                            ),
                        )
                        .when(local, |d| {
                            d.child(tool(format!("f{ix}-sysopen"), "Open ↗").on_click(
                                cx.listener(move |this, _, _, cx| this.open_with_system(ix, cx)),
                            ))
                            .child(
                                tool(format!("f{ix}-reveal"), "Reveal").on_click(
                                    cx.listener(move |this, _, _, cx| this.reveal_in_os(ix, cx)),
                                ),
                            )
                        })
                })
                .child(div().flex_1())
                .when_some(pane.git.as_ref().map(GitStatus::summary), |d, summary| {
                    let on = pane.changes_only;
                    d.child(
                        div()
                            .font_family(MONO)
                            .text_color(p.fg2)
                            .child(format!("⎇ {summary}")),
                    )
                    .child(
                        tool(
                            format!("f{ix}-changes"),
                            if on { "Changes ●" } else { "Changes ○" },
                        )
                        .when(on, |t| t.text_color(p.acc))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let pane = &mut this.panes[ix];
                            pane.changes_only = !pane.changes_only;
                            cx.notify();
                        })),
                    )
                })
                .when(pane.next.is_some() && !loading, |d| {
                    d.child(
                        ui::button(
                            SharedString::from(format!("f{ix}-more")),
                            format!("Load {PAGE} more"),
                            Kind::Secondary,
                            p,
                        )
                        .h(rpx(22.))
                        .text_size(ts::LABEL)
                        .on_click(cx.listener(move |this, _, _, cx| this.load_more(ix, cx))),
                    )
                })
                .into_any_element()
        });

        let edit_row: Option<AnyElement> =
            self.edit
                .as_ref()
                .filter(|(i, _, _)| *i == ix)
                .map(|(_, e, input)| {
                    div()
                        .flex_none()
                        .px(rpx(10.))
                        .py(rpx(5.))
                        .flex()
                        .items_center()
                        .gap(rpx(6.))
                        .border_b_1()
                        .border_color(p.bd)
                        .bg(p.surface)
                        .child(div().text_size(ts::LABEL).text_color(p.fg2).child(match e {
                            Edit::NewFolder => "New folder:",
                            Edit::Rename(_) => "Rename to:",
                        }))
                        .child(
                            div()
                                .flex_1()
                                .h(rpx(24.))
                                .flex()
                                .items_center()
                                .px(rpx(7.))
                                .border_1()
                                .border_color(p.acc)
                                .rounded(px(4.))
                                .child(Input::new(input).appearance(false).text_size(ts::BODY)),
                        )
                        .child(
                            ui::button("f-edit-ok", "OK", Kind::Primary, p)
                                .h(rpx(22.))
                                .on_click(cx.listener(|this, _, _, cx| this.finish_edit(cx))),
                        )
                        .child(
                            ui::button("f-edit-cancel", "Cancel", Kind::Ghost, p)
                                .h(rpx(22.))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.edit = None;
                                    cx.notify();
                                })),
                        )
                        .into_any_element()
                });

        let body: AnyElement = if let Some(e) = &pane.error {
            div()
                .flex_1()
                .p(rpx(14.))
                .text_size(ts::BODY)
                .text_color(p.prod)
                .child(e.clone())
                .into_any_element()
        } else if pane.path.is_none() || (count == 0 && loading) {
            let label = match pane.fs {
                _ if pane.path.is_some() => "Loading…",
                FsRef::Host(_) => "Opening SFTP on the Host's session…",
                FsRef::Conn(_) => "Connecting…",
                FsRef::Local => "Loading…",
            };
            div()
                .flex_1()
                .p(rpx(14.))
                .flex()
                .flex_col()
                .gap(rpx(10.))
                .text_size(ts::BODY)
                .text_color(p.fg3)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(rpx(6.))
                        .child(ui::pulse_dot(
                            SharedString::from(format!("f{ix}-opening")),
                            p.acc,
                            6.,
                        ))
                        .child(label),
                )
                .children(
                    [220., 160., 190., 130., 170.]
                        .into_iter()
                        .map(|w| ui::shimmer(w, p)),
                )
                .into_any_element()
        } else if count == 0 {
            let filter = pane.filter.trim();
            div()
                .flex_1()
                .p(rpx(14.))
                .text_size(ts::BODY)
                .text_color(p.fg3)
                .child(if filter.is_empty() {
                    "Empty folder".to_owned()
                } else if pane.paged() && filter != pane.prefix {
                    format!(
                        "No loaded names contain “{filter}”. Press Enter to search the whole folder."
                    )
                } else {
                    format!("No names contain “{filter}”")
                })
                .into_any_element()
        } else {
            let selected = pane.selected.clone();
            let fs = pane.fs.clone();
            let dir = pane.path.clone().unwrap_or_default();
            let posix = pane.posix();
            let changes: Vec<Option<ChangeKind>> = if pane.git.is_some() {
                rows.iter().map(|e| pane.change(&e.name)).collect()
            } else {
                Vec::new()
            };
            uniform_list(
                SharedString::from(format!("f{ix}-list")),
                count,
                cx.processor(move |_this, range: std::ops::Range<usize>, _w, cx| {
                    let p = palette(cx);
                    range
                        .filter_map(|i| rows.get(i).cloned().map(|e| (i, e)))
                        .map(|(i, e)| {
                            let change = changes.get(i).copied().flatten();
                            let is_sel = selected.contains(&e.name);
                            let (icon, color) = match &e.kind {
                                EntryKind::Dir => ("▸", p.acc),
                                EntryKind::Symlink(_) => ("↪", p.fg3),
                                EntryKind::File => ("·", p.fg3),
                            };
                            // Drag the whole selection when the row is part of it.
                            let names = if is_sel {
                                selected.clone()
                            } else {
                                vec![e.name.clone()]
                            };
                            let paths: Vec<PathBuf> = names
                                .iter()
                                .map(|n| {
                                    if posix {
                                        let d = dir.to_string_lossy().replace('\\', "/");
                                        PathBuf::from(format!("{}/{n}", d.trim_end_matches('/')))
                                    } else {
                                        dir.join(n)
                                    }
                                })
                                .collect();
                            let label = if names.len() == 1 {
                                names[0].clone()
                            } else {
                                format!("{} items", names.len())
                            };
                            let drag = DraggedFiles {
                                pane: ix,
                                fs: fs.clone(),
                                paths,
                            };
                            let entry = e.clone();
                            div()
                                .id(SharedString::from(format!("f{ix}-row-{}", e.name)))
                                .w_full()
                                .h(rpx(24.))
                                .px(rpx(10.))
                                .flex()
                                .items_center()
                                .gap(rpx(8.))
                                .text_size(ts::UI)
                                .when(is_sel, |d| d.bg(p.sel))
                                .when(!is_sel, |d| d.hover(|s| s.bg(p.hover)))
                                .on_click(cx.listener(
                                    move |this, ev: &gpui_kit::ClickEvent, window, cx| {
                                        // Arrow keys and Enter work after a click.
                                        let focus = this.focus.clone();
                                        window.focus(&focus, cx);
                                        let toggle = ev.modifiers().secondary();
                                        this.click(ix, &entry, toggle, cx)
                                    },
                                ))
                                .on_drag(drag, move |_, _, _, cx| {
                                    let label = label.clone();
                                    cx.new(|_| DragPreview(label))
                                })
                                .child(div().w(rpx(10.)).text_color(color).child(icon))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .when_some(change, |d, c| d.text_color(change_color(c, &p)))
                                        .child(e.name.clone()),
                                )
                                .child(
                                    div()
                                        .w(rpx(12.))
                                        .flex_none()
                                        .font_family(MONO)
                                        .text_size(ts::SMALL)
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .when_some(change, |d, c| {
                                            d.text_color(change_color(c, &p)).child(c.badge())
                                        }),
                                )
                                .when(full, |d| {
                                    d.child(
                                        div()
                                            .w(rpx(72.))
                                            .flex()
                                            .justify_end()
                                            .font_family(MONO)
                                            .text_size(ts::SMALL)
                                            .text_color(p.fg2)
                                            .child(if e.is_dir() {
                                                String::new()
                                            } else {
                                                human(e.size)
                                            }),
                                    )
                                    .child(
                                        div()
                                            .w(rpx(118.))
                                            .font_family(MONO)
                                            .text_size(ts::SMALL)
                                            .text_color(p.fg3)
                                            .child(e.modified_ms.map(fmt_time).unwrap_or_default()),
                                    )
                                    .child(
                                        div()
                                            .w(rpx(76.))
                                            .font_family(MONO)
                                            .text_size(ts::CAPTION_PLUS)
                                            .text_color(p.fg3)
                                            .child(e.mode_string()),
                                    )
                                })
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .track_scroll(&self.scroll[ix])
            .flex_1()
            .into_any_element()
        };

        let code_menu: Option<AnyElement> = (self.code_menu == Some(ix)).then(|| {
            let repo = pane.fs == FsRef::Local && pane.git.is_some();
            let worktree = self.use_worktree && repo;
            let target = self
                .run_dir(ix)
                .map(|d| crate::coding_clis::folder_name(&d))
                .unwrap_or_default();
            deferred(
                div()
                    .id(SharedString::from(format!("f{ix}-code-menu")))
                    .absolute()
                    .top(rpx(30.))
                    .left(rpx(8.))
                    .w(rpx(280.))
                    .max_h(rpx(360.))
                    .overflow_y_scroll()
                    .py(rpx(4.))
                    .bg(p.elev)
                    .border_1()
                    .border_color(p.bd2)
                    .rounded(px(6.))
                    .shadow(ui::shadow(p))
                    .occlude()
                    .child(
                        div()
                            .px(rpx(10.))
                            .py(rpx(4.))
                            .text_size(ts::SMALL)
                            .text_color(p.fg3)
                            .child(format!("Run a coding CLI in {target}")),
                    )
                    .children(self.clis.iter().cloned().enumerate().map(|(i, c)| {
                        let status = c.status.clone();
                        let missing = c.missing;
                        div()
                            .id(SharedString::from(format!("f{ix}-cli-{i}")))
                            .px(rpx(10.))
                            .h(rpx(26.))
                            .flex()
                            .items_center()
                            .gap(rpx(8.))
                            .text_size(ts::BODY)
                            .hover(|s| s.bg(p.hover))
                            .child(div().flex_1().child(c.name.clone()))
                            .children(status.map(|s| {
                                div()
                                    .text_size(ts::SMALL)
                                    .text_color(if missing { p.stg } else { p.fg3 })
                                    .child(s)
                            }))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.run_here(ix, Some(c.clone()), cx)
                            }))
                    }))
                    .when(repo, |d| {
                        d.child(div().my(rpx(4.)).h(px(1.)).bg(p.bd)).child(
                            div()
                                .id(SharedString::from(format!("f{ix}-worktree")))
                                .px(rpx(10.))
                                .py(rpx(4.))
                                .flex()
                                .flex_col()
                                .gap(rpx(2.))
                                .hover(|s| s.bg(p.hover))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.use_worktree = !this.use_worktree;
                                    cx.notify();
                                }))
                                .child(div().text_size(ts::BODY).child(if worktree {
                                    "● In a new git worktree"
                                } else {
                                    "○ In a new git worktree"
                                }))
                                .child(div().text_size(ts::SMALL).text_color(p.fg3).child(
                                    "Its own branch and folder, so several CLIs \
                                                 can work on this repository at once",
                                )),
                        )
                    }),
            )
            .into_any_element()
        });

        let picker: Option<AnyElement> = (ix == 1 && self.picker_open).then(|| {
            let mut items: Vec<Source> = vec![(FsRef::Local, "This computer".into(), "FS")];
            items.extend(self.sources.iter().cloned());
            deferred(
                div()
                    .id("files-picker")
                    .absolute()
                    .top(rpx(30.))
                    .left(rpx(8.))
                    .w(rpx(220.))
                    .max_h(rpx(360.))
                    .overflow_y_scroll()
                    .occlude()
                    .py(rpx(4.))
                    .bg(p.elev)
                    .border_1()
                    .border_color(p.bd2)
                    .rounded(px(6.))
                    .shadow(ui::shadow(p))
                    .children(items.into_iter().enumerate().map(|(i, (fs, name, badge))| {
                        div()
                            .id(SharedString::from(format!("files-pick-{i}")))
                            .px(rpx(10.))
                            .h(rpx(26.))
                            .flex()
                            .items_center()
                            .gap(rpx(6.))
                            .text_size(ts::BODY)
                            .hover(|s| s.bg(p.hover))
                            .on_click(cx.listener(move |this, _, _, cx| this.show(fs.clone(), cx)))
                            .child(
                                div()
                                    .text_color(p.fg3)
                                    .font_family(MONO)
                                    .text_size(ts::TINY)
                                    .child(badge),
                            )
                            .child(name)
                    })),
            )
            .into_any_element()
        });

        let drop_bg = p.sel;
        div()
            .id(SharedString::from(format!("files-pane-{ix}")))
            .relative()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .when(ix == 0 && self.layout == Layout::Dual, |d| {
                d.border_r_1().border_color(p.bd)
            })
            .when(active && self.layout == Layout::Dual, |d| d.bg(p.surface))
            .on_mouse_down(
                gpui_kit::MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    if this.active != ix {
                        this.active = ix;
                        cx.notify();
                    }
                }),
            )
            .drag_over::<DraggedFiles>(
                move |s, d, _, _| if d.pane != ix { s.bg(drop_bg) } else { s },
            )
            .drag_over::<ExternalPaths>(move |s, _, _, _| s.bg(drop_bg))
            .on_drop(cx.listener(move |this, d: &DraggedFiles, _, cx| {
                if d.pane != ix {
                    this.copy_into(d.fs.clone(), d.paths.clone(), ix, cx);
                }
            }))
            .on_drop(cx.listener(move |this, paths: &ExternalPaths, _, cx| {
                this.copy_into(FsRef::Local, paths.paths().to_vec(), ix, cx);
            }))
            .child(toolbar)
            .child(crumb_bar)
            .children(loading_bar)
            .children(search_row)
            .children(edit_row)
            .child(header)
            .child(body)
            .children(footer)
            .children(picker)
            .children(code_menu)
            .into_any_element()
    }
}

impl Focusable for FilesTab {
    fn focus_handle(&self, _cx: &gpui_kit::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for FilesTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.seen = true;
        let p = palette(cx);
        let left = self.render_pane(0, &p, cx);
        let right = (self.layout == Layout::Dual).then(|| self.render_pane(1, &p, cx));
        let drawer = (self.layout != Layout::Sidebar)
            .then(|| self.transfers.update(cx, |t, cx| t.render_drawer(&p, cx)));
        div()
            .key_context("FilesTab")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                let ix = this.active;
                if this.edit.is_none()
                    && this.focus.is_focused(window)
                    && this.on_nav_key(ev, window, cx)
                {
                    cx.stop_propagation();
                    return;
                }
                if this.edit.is_none() && this.search[ix].is_some() && ev.keystroke.key == "escape"
                {
                    this.close_search(ix, cx);
                }
                if this.edit.is_some() {
                    match ev.keystroke.key.as_str() {
                        "enter" => this.finish_edit(cx),
                        "escape" => {
                            this.edit = None;
                            cx.notify();
                        }
                        _ => {}
                    }
                }
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(p.bg)
            .when_some(self.error.clone(), |d, e| {
                d.child(
                    div()
                        .px(rpx(12.))
                        .py(rpx(5.))
                        .bg(p.prod_bg)
                        .text_color(p.prod)
                        .text_size(ts::BODY)
                        .child(e),
                )
            })
            .child(div().flex_1().min_h_0().flex().child(left).children(right))
            .children(drawer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn sorting_keeps_folders_first() {
        let mut p = pane(vec![
            ("b.txt", false, 10),
            ("a.txt", false, 30),
            ("zdir", true, 0),
            (".env", false, 5),
        ]);
        let names = |p: &Pane| {
            p.visible()
                .iter()
                .map(|e| e.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&p), ["zdir", "a.txt", "b.txt"]);
        p.sort = (SortBy::Size, false);
        assert_eq!(names(&p), ["zdir", "a.txt", "b.txt"]);
        p.sort = (SortBy::Size, true);
        p.show_hidden = true;
        assert_eq!(names(&p), ["zdir", ".env", "b.txt", "a.txt"]);
    }

    #[test]
    fn search_filters_loaded_names_in_any_case() {
        let mut p = pane(vec![
            ("Report-2024.csv", false, 1),
            ("reports", true, 0),
            ("notes.txt", false, 2),
        ]);
        p.filter = " REPORT ".into();
        let names: Vec<_> = p.visible().into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["reports", "Report-2024.csv"]);
        assert!(!p.paged());
        p.next = Some("marker".into());
        assert!(p.paged());
    }

    #[test]
    fn breadcrumbs() {
        let p = pane(vec![]);
        let c: Vec<_> = p.crumbs().into_iter().map(|(l, _)| l).collect();
        assert_eq!(c, ["/", "home", "swy", "app"]);
        assert_eq!(p.crumbs()[2].1, PathBuf::from("/home/swy"));
        assert_eq!(fmt_time(86_400_000 + 3_723_000), "1970-01-02 01:02");
    }
}
