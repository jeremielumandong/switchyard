//! Git for the Files tab and coding CLI runs on this computer: a folder's working-tree
//! status (which files a coding CLI changed) and a fresh worktree so several CLIs can work
//! on one repository side by side. Runs the user's `git`; a folder outside a repository,
//! or no `git` installed, is "no status", not an error.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long one `git` call may take.
const GIT_TIMEOUT: Duration = Duration::from_secs(10);

/// What happened to a path, from `git status --porcelain`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    /// Changed in the index or the working tree.
    Modified,
    /// New in the index.
    Added,
    /// Removed.
    Deleted,
    /// Renamed or copied.
    Renamed,
    /// Not tracked.
    Untracked,
    /// Unmerged (a conflict).
    Conflicted,
}

impl ChangeKind {
    /// One-letter badge: `M`, `A`, `D`, `R`, `?`, `!`.
    pub fn badge(self) -> &'static str {
        match self {
            ChangeKind::Modified => "M",
            ChangeKind::Added => "A",
            ChangeKind::Deleted => "D",
            ChangeKind::Renamed => "R",
            ChangeKind::Untracked => "?",
            ChangeKind::Conflicted => "!",
        }
    }
}

/// One changed path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitChange {
    /// Path relative to the repository root, `/`-separated.
    pub path: String,
    /// What happened.
    pub kind: ChangeKind,
}

/// A repository's working-tree status.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitStatus {
    /// The repository's top folder.
    pub root: PathBuf,
    /// Current branch; `None` when detached.
    pub branch: Option<String>,
    /// Commits ahead of / behind the upstream.
    pub ahead: u32,
    /// Commits behind the upstream.
    pub behind: u32,
    /// Changed paths.
    pub changes: Vec<GitChange>,
}

impl GitStatus {
    /// The change for `path` (absolute), or for anything under it when it is a folder:
    /// a folder holding changes reports [`ChangeKind::Modified`].
    pub fn change_at(&self, path: &Path) -> Option<ChangeKind> {
        let rel = path.strip_prefix(&self.root).ok()?;
        let rel = rel.to_string_lossy().replace('\\', "/");
        let rel = rel.trim_end_matches('/');
        if rel.is_empty() {
            return None;
        }
        let mut inside = false;
        for c in &self.changes {
            let p = c.path.trim_end_matches('/');
            if p == rel {
                return Some(c.kind);
            }
            // An untracked folder is listed once; what is inside it is untracked too.
            if rel.len() > p.len() && rel.starts_with(p) && rel.as_bytes()[p.len()] == b'/' {
                return Some(c.kind);
            }
            if p.len() > rel.len() && p.starts_with(rel) && p.as_bytes()[rel.len()] == b'/' {
                inside = true;
            }
        }
        inside.then_some(ChangeKind::Modified)
    }

    /// "main ↑1 · 3 changed".
    pub fn summary(&self) -> String {
        let mut s = self.branch.clone().unwrap_or_else(|| "detached".into());
        if self.ahead > 0 {
            s.push_str(&format!(" ↑{}", self.ahead));
        }
        if self.behind > 0 {
            s.push_str(&format!(" ↓{}", self.behind));
        }
        match self.changes.len() {
            0 => s.push_str(" · clean"),
            n => s.push_str(&format!(" · {n} changed")),
        }
        s
    }
}

fn kind_of(x: u8, y: u8) -> ChangeKind {
    match (x, y) {
        (b'?', _) => ChangeKind::Untracked,
        (b'U', _) | (_, b'U') | (b'A', b'A') | (b'D', b'D') => ChangeKind::Conflicted,
        (b'R' | b'C', _) => ChangeKind::Renamed,
        (b'A', _) => ChangeKind::Added,
        (b'D', _) | (_, b'D') => ChangeKind::Deleted,
        _ => ChangeKind::Modified,
    }
}

/// Parse `git status --porcelain=v1 -z --branch` output (the root is filled in later).
pub fn parse_porcelain(out: &[u8]) -> GitStatus {
    let mut st = GitStatus::default();
    let mut fields = out.split(|b| *b == 0).filter(|f| !f.is_empty());
    while let Some(f) = fields.next() {
        let f = String::from_utf8_lossy(f);
        if let Some(head) = f.strip_prefix("## ") {
            parse_branch(head, &mut st);
            continue;
        }
        if f.len() < 4 {
            continue;
        }
        let b = f.as_bytes();
        let kind = kind_of(b[0], b[1]);
        if b[0] == b'!' {
            continue;
        }
        st.changes.push(GitChange {
            path: f[3..].to_owned(),
            kind,
        });
        // A rename or copy is followed by its old path.
        if matches!(b[0], b'R' | b'C') {
            fields.next();
        }
    }
    st
}

/// `main...origin/main [ahead 1, behind 2]`, `HEAD (no branch)`, `No commits yet on main`.
fn parse_branch(head: &str, st: &mut GitStatus) {
    let (name, rest) = match head.split_once(" [") {
        Some((n, r)) => (n, r.trim_end_matches(']')),
        None => (head, ""),
    };
    let name = name.strip_prefix("No commits yet on ").unwrap_or(name);
    let name = name.split("...").next().unwrap_or(name).trim();
    st.branch = (!name.starts_with("HEAD (")).then(|| name.to_owned());
    for part in rest.split(", ") {
        if let Some(n) = part.strip_prefix("ahead ") {
            st.ahead = n.parse().unwrap_or(0);
        } else if let Some(n) = part.strip_prefix("behind ") {
            st.behind = n.parse().unwrap_or(0);
        }
    }
}

async fn git(dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    match tokio::time::timeout(GIT_TIMEOUT, cmd.output()).await {
        Err(_) => Err("git did not answer".into()),
        Ok(Err(e)) => Err(e.to_string()),
        Ok(Ok(o)) if o.status.success() => Ok(o.stdout),
        Ok(Ok(o)) => Err(String::from_utf8_lossy(&o.stderr).trim().to_owned()),
    }
}

/// The repository root holding `dir`, or `None` outside one (or without `git`).
async fn toplevel(dir: &Path) -> Option<PathBuf> {
    let out = git(dir, &["rev-parse", "--show-toplevel"]).await.ok()?;
    let s = String::from_utf8_lossy(&out).trim().to_owned();
    (!s.is_empty()).then(|| PathBuf::from(s))
}

/// The status of the repository holding `dir`; `None` outside one.
pub async fn status(dir: &Path) -> Result<Option<GitStatus>, String> {
    let Some(root) = toplevel(dir).await else {
        return Ok(None);
    };
    let out = git(
        &root,
        &["status", "--porcelain=v1", "-z", "--branch", "--untracked-files=normal"],
    )
    .await?;
    let mut st = parse_porcelain(&out);
    st.root = root;
    Ok(Some(st))
}

/// A folder and branch name for a new worktree of the repository at `root`:
/// `<parent>/<repo>.worktrees/<label>-<n>` on branch `switchyard/<label>-<n>`.
pub fn worktree_place(root: &Path, label: &str, n: u64) -> (PathBuf, String) {
    let slug: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let slug = if slug.is_empty() { "run".into() } else { slug };
    let repo = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    let parent = root.parent().unwrap_or(root);
    let name = format!("{slug}-{n}");
    (
        parent.join(format!("{repo}.worktrees")).join(&name),
        format!("switchyard/{name}"),
    )
}

/// Add a worktree of the repository holding `dir` on a new branch from `HEAD`, for a run
/// named `label`; answers its folder and branch.
pub async fn add_worktree(dir: &Path, label: &str) -> Result<(PathBuf, String), String> {
    let root = toplevel(dir)
        .await
        .ok_or_else(|| format!("{} is not in a git repository", dir.display()))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    // Seconds since the epoch, shortened; bump on the rare clash.
    let mut n = stamp % 100_000;
    loop {
        let (path, branch) = worktree_place(&root, label, n);
        if !path.exists() {
            let p = path.to_string_lossy().into_owned();
            git(&root, &["worktree", "add", "-b", &branch, &p, "HEAD"]).await?;
            return Ok((path, branch));
        }
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn porcelain_with_branch_rename_and_untracked() {
        let out = b"## main...origin/main [ahead 2, behind 1]\0 M src/lib.rs\0A  new.rs\0R  b.rs\0a.rs\0?? notes/todo.md\0UU merge.rs\0 D gone.txt\0";
        let st = parse_porcelain(out);
        assert_eq!(st.branch.as_deref(), Some("main"));
        assert_eq!((st.ahead, st.behind), (2, 1));
        let got: Vec<_> = st
            .changes
            .iter()
            .map(|c| (c.path.as_str(), c.kind))
            .collect();
        assert_eq!(
            got,
            [
                ("src/lib.rs", ChangeKind::Modified),
                ("new.rs", ChangeKind::Added),
                ("b.rs", ChangeKind::Renamed),
                ("notes/todo.md", ChangeKind::Untracked),
                ("merge.rs", ChangeKind::Conflicted),
                ("gone.txt", ChangeKind::Deleted),
            ]
        );
    }

    #[test]
    fn branch_lines() {
        let st = parse_porcelain(b"## HEAD (no branch)\0");
        assert_eq!(st.branch, None);
        let st = parse_porcelain(b"## No commits yet on dev\0");
        assert_eq!(st.branch.as_deref(), Some("dev"));
        let st = parse_porcelain(b"## feature/x\0");
        assert_eq!(st.branch.as_deref(), Some("feature/x"));
        assert_eq!(st.summary(), "feature/x · clean");
    }

    #[test]
    fn change_at_files_and_folders() {
        let mut st = parse_porcelain(b"## main\0 M src/app/main.rs\0?? docs/\0");
        st.root = PathBuf::from("/w/repo");
        assert_eq!(
            st.change_at(Path::new("/w/repo/src/app/main.rs")),
            Some(ChangeKind::Modified)
        );
        assert_eq!(
            st.change_at(Path::new("/w/repo/src")),
            Some(ChangeKind::Modified)
        );
        assert_eq!(
            st.change_at(Path::new("/w/repo/docs")),
            Some(ChangeKind::Untracked)
        );
        assert_eq!(
            st.change_at(Path::new("/w/repo/docs/guide/a.md")),
            Some(ChangeKind::Untracked)
        );
        assert_eq!(st.change_at(Path::new("/w/repo/sr")), None);
        assert_eq!(st.change_at(Path::new("/w/repo")), None);
        assert_eq!(st.change_at(Path::new("/elsewhere/x")), None);
        assert_eq!(st.summary(), "main · 2 changed");
    }

    #[test]
    fn worktree_names() {
        let (p, b) = worktree_place(Path::new("/w/shop"), "Claude Code", 42);
        assert_eq!(p, PathBuf::from("/w/shop.worktrees/claude-code-42"));
        assert_eq!(b, "switchyard/claude-code-42");
        let (_, b) = worktree_place(Path::new("/w/shop"), "  ", 1);
        assert_eq!(b, "switchyard/run-1");
    }

    #[tokio::test]
    async fn status_and_worktree_in_a_real_repo() {
        if std::process::Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("proj");
        std::fs::create_dir(&repo).unwrap();
        let run = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@example.test"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(repo.join("a.txt"), "a").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);
        std::fs::write(repo.join("a.txt"), "b").unwrap();
        std::fs::write(repo.join("new.txt"), "n").unwrap();

        assert_eq!(status(dir.path()).await.unwrap(), None);
        let st = status(&repo).await.unwrap().unwrap();
        assert_eq!(st.branch.as_deref(), Some("main"));
        let root = st.root.clone();
        assert_eq!(
            st.change_at(&root.join("a.txt")),
            Some(ChangeKind::Modified)
        );
        assert_eq!(
            st.change_at(&root.join("new.txt")),
            Some(ChangeKind::Untracked)
        );

        let (wt, branch) = add_worktree(&repo, "Codex").await.unwrap();
        assert!(wt.join("a.txt").is_file());
        assert!(branch.starts_with("switchyard/codex-"));
        let st = status(&wt).await.unwrap().unwrap();
        assert_eq!(st.branch.as_deref(), Some(branch.as_str()));
        assert!(st.changes.is_empty());
    }
}
