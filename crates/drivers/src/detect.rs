//! Finding components: a path the user chose, the app-managed directory, the OS (built in),
//! environment variables, and the system library directories, in that order.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::manifest::{ComponentSpec, Os, Platform};
use crate::version;

/// Where a component was found.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Source {
    /// A path the user chose ("Use existing path").
    UserPath,
    /// `<data>/drivers/<id>/<version>/`, installed by Switchyard.
    AppManaged,
    /// Part of the operating system.
    Builtin,
    /// An environment variable (`SSH_AUTH_SOCK`).
    Environment,
    /// A system library directory (package manager install).
    System,
}

/// Detected state of a component.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ComponentStatus {
    /// Found and usable.
    Installed {
        /// Version, if known.
        version: Option<String>,
        /// Library file, directory, or a description (`built in`, `$SSH_AUTH_SOCK`).
        location: String,
        /// How it was found.
        source: Source,
    },
    /// Found, but a newer version than Switchyard supports (an untested major).
    TooNew {
        /// Version found.
        version: String,
        /// First unsupported version.
        supported_below: String,
        /// Where.
        location: String,
    },
    /// Found, but older than the manifest's minimum.
    TooOld {
        /// Version found.
        version: String,
        /// Minimum required.
        required: String,
        /// Where.
        location: String,
    },
    /// Not found.
    Missing,
}

impl ComponentStatus {
    /// Whether the feature can be used.
    pub fn is_installed(&self) -> bool {
        matches!(self, ComponentStatus::Installed { .. })
    }
}

/// Reads an environment variable.
pub type EnvLookup = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Finds an executable by name.
pub type ProgramLookup = Box<dyn Fn(&str) -> Option<PathBuf> + Send + Sync>;

/// A program's `--version` output.
pub type VersionProbe = Box<dyn Fn(&Path) -> Option<String> + Send + Sync>;

/// What detection looks at; tests replace every part of it.
pub struct DetectEnv {
    /// App-managed driver directory.
    pub drivers_dir: PathBuf,
    /// System library directories, searched in order.
    pub search_dirs: Vec<PathBuf>,
    /// Environment lookup.
    pub var: EnvLookup,
    /// Paths chosen by the user, per component id.
    pub overrides: HashMap<String, PathBuf>,
    /// Operating system.
    pub os: Os,
    /// Executable lookup (PATH and common install folders).
    pub program: ProgramLookup,
    /// Runs `<program> --version`.
    pub version_of: VersionProbe,
}

impl std::fmt::Debug for DetectEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DetectEnv")
            .field("drivers_dir", &self.drivers_dir)
            .field("search_dirs", &self.search_dirs)
            .field("os", &self.os)
            .finish_non_exhaustive()
    }
}

impl DetectEnv {
    /// The real machine.
    pub fn system(drivers_dir: PathBuf) -> Self {
        let os = Os::current();
        let mut search_dirs: Vec<PathBuf> = Vec::new();
        let path_var = match os {
            Os::Linux => "LD_LIBRARY_PATH",
            Os::Macos => "DYLD_LIBRARY_PATH",
            Os::Windows => "PATH",
        };
        if let Some(v) = std::env::var_os(path_var) {
            search_dirs.extend(std::env::split_paths(&v));
        }
        let fixed: &[&str] = match os {
            Os::Linux => &[
                "/usr/lib/x86_64-linux-gnu",
                "/usr/lib/aarch64-linux-gnu",
                "/lib/x86_64-linux-gnu",
                "/lib/aarch64-linux-gnu",
                "/usr/lib64",
                "/lib64",
                "/usr/lib",
                "/usr/local/lib",
            ],
            Os::Macos => &["/opt/homebrew/lib", "/usr/local/lib", "/usr/lib"],
            Os::Windows => &[],
        };
        search_dirs.extend(fixed.iter().map(PathBuf::from));
        Self {
            drivers_dir,
            search_dirs,
            var: Box::new(|k| std::env::var(k).ok()),
            overrides: HashMap::new(),
            os,
            program: Box::new(find_program),
            version_of: Box::new(program_version),
        }
    }
}

/// Folders searched after PATH: desktop-launched apps often miss npm and Homebrew.
fn program_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    dirs.extend(["/usr/local/bin", "/opt/homebrew/bin"].map(PathBuf::from));
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    if let Some(h) = home.map(PathBuf::from) {
        for d in [
            ".local/bin",
            ".npm-global/bin",
            ".claude/local",
            ".volta/bin",
            ".bun/bin",
        ] {
            dirs.push(h.join(d));
        }
    }
    if let Some(a) = std::env::var_os("APPDATA") {
        dirs.push(PathBuf::from(a).join("npm"));
    }
    dirs
}

/// `name` on PATH or in a common install folder (Windows: `.exe`, `.cmd`, `.bat`).
pub fn find_program(name: &str) -> Option<PathBuf> {
    let exts: &[&str] = if cfg!(windows) {
        &["exe", "cmd", "bat"]
    } else {
        &[""]
    };
    program_dirs().into_iter().find_map(|d| {
        exts.iter()
            .map(|e| {
                let p = d.join(name);
                if e.is_empty() { p } else { p.with_extension(e) }
            })
            .find(|p| p.is_file())
    })
}

/// Windows `CREATE_NO_WINDOW`: run a console program without opening a console window.
#[cfg(windows)]
pub(crate) const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// `<program> --version`, given up after 10 s.
pub fn program_version(program: &Path) -> Option<String> {
    let mut command = std::process::Command::new(program);
    // A GUI app starting a console program (or an npm `.cmd` shim through cmd.exe)
    // otherwise flashes a console window on Windows.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut out = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut out).ok()?;
    version::parse_version(&out)
}

/// A coding CLI: found on PATH (or the user's path) and checked against the version range.
fn detect_program(spec: &ComponentSpec, env: &DetectEnv) -> ComponentStatus {
    let found = env
        .overrides
        .get(&spec.id)
        .filter(|p| p.is_file())
        .map(|p| (p.clone(), Source::UserPath))
        .or_else(|| {
            spec.detect
                .programs
                .iter()
                .find_map(|n| (env.program)(n))
                .map(|p| (p, Source::System))
        });
    let Some((path, source)) = found else {
        return ComponentStatus::Missing;
    };
    let location = path.display().to_string();
    let version = (env.version_of)(&path);
    if let Some(v) = &version {
        if let Some(min) = spec.detect.min_version.as_deref()
            && !version::at_least(v, min)
        {
            return ComponentStatus::TooOld {
                version: v.clone(),
                required: min.to_owned(),
                location,
            };
        }
        if let Some(below) = spec.detect.below_version.as_deref()
            && version::at_least(v, below)
        {
            return ComponentStatus::TooNew {
                version: v.clone(),
                supported_below: below.to_owned(),
                location,
            };
        }
    }
    ComponentStatus::Installed {
        version,
        location,
        source,
    }
}

/// The first of `names` inside `dir` (or `dir` itself when it is one of them).
pub fn library_in(dir: &Path, names: &[String]) -> Option<PathBuf> {
    if dir.is_file() {
        let file = dir.file_name()?.to_string_lossy();
        return names.iter().any(|n| *n == file).then(|| dir.to_owned());
    }
    names.iter().map(|n| dir.join(n)).find(|p| p.exists())
}

/// Versions installed under `<drivers_dir>/<id>/`, newest first.
pub fn app_managed_versions(drivers_dir: &Path, id: &str) -> Vec<(String, PathBuf)> {
    let Ok(rd) = std::fs::read_dir(drivers_dir.join(id)) else {
        return Vec::new();
    };
    let mut v: Vec<(String, PathBuf)> = rd
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            // Staging folders start with a dot.
            (!name.starts_with('.')).then(|| (name, e.path()))
        })
        .collect();
    v.sort_by(|a, b| version::compare(&b.0, &a.0));
    v
}

/// The library folder of an app-managed install.
fn lib_dir(spec: &ComponentSpec, os: Os, root: &Path) -> PathBuf {
    match spec.platforms.get(os) {
        Some(Platform::Archive {
            lib_dir: Some(d), ..
        }) => root.join(d),
        _ => root.to_owned(),
    }
}

/// Detect one component.
pub fn detect(spec: &ComponentSpec, env: &DetectEnv) -> ComponentStatus {
    if !spec.detect.programs.is_empty() {
        return detect_program(spec, env);
    }
    let libs = &spec.detect.libraries;
    let min = spec.detect.min_version.as_deref();

    if let Some(p) = env.overrides.get(&spec.id)
        && let Some(found) = library_in(p, libs)
    {
        return ComponentStatus::Installed {
            version: None,
            location: found.display().to_string(),
            source: Source::UserPath,
        };
    }

    let mut too_old = None;
    for (version, root) in app_managed_versions(&env.drivers_dir, &spec.id) {
        let dir = lib_dir(spec, env.os, &root);
        let Some(found) = library_in(&dir, libs) else {
            continue;
        };
        match min {
            Some(m) if !version::at_least(&version, m) => {
                too_old.get_or_insert(ComponentStatus::TooOld {
                    version,
                    required: m.to_owned(),
                    location: found.display().to_string(),
                });
            }
            _ => {
                return ComponentStatus::Installed {
                    version: Some(version),
                    location: found.display().to_string(),
                    source: Source::AppManaged,
                };
            }
        }
    }

    if spec.platforms.get(env.os) == Some(&Platform::Builtin) && spec.detect.env.is_empty() {
        return ComponentStatus::Installed {
            version: None,
            location: "built in".into(),
            source: Source::Builtin,
        };
    }

    for name in &spec.detect.env {
        if let Some(v) = (env.var)(name)
            && !v.is_empty()
            && Path::new(&v).exists()
        {
            return ComponentStatus::Installed {
                version: None,
                location: format!("${name}"),
                source: Source::Environment,
            };
        }
    }

    for p in &spec.detect.paths {
        let full = match p.strip_prefix("~/") {
            Some(rest) => match (env.var)("HOME").or_else(|| (env.var)("USERPROFILE")) {
                Some(home) => Path::new(&home).join(rest),
                None => continue,
            },
            None => PathBuf::from(p),
        };
        if full.exists() {
            return ComponentStatus::Installed {
                version: None,
                location: p.clone(),
                source: Source::Environment,
            };
        }
    }

    if !libs.is_empty() {
        for dir in &env.search_dirs {
            if let Some(found) = library_in(dir, libs) {
                return ComponentStatus::Installed {
                    version: None,
                    location: found.display().to_string(),
                    source: Source::System,
                };
            }
        }
    }

    too_old.unwrap_or(ComponentStatus::Missing)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::manifest::Manifest;

    fn env(dir: &Path) -> DetectEnv {
        DetectEnv {
            drivers_dir: dir.join("drivers"),
            search_dirs: vec![dir.join("system")],
            var: Box::new(|_| None),
            overrides: HashMap::new(),
            os: Os::Linux,
            program: Box::new(|_| None),
            version_of: Box::new(|_| None),
        }
    }

    fn demo() -> ComponentSpec {
        let m = Manifest::parse(
            std::fs::read(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/manifest.json"
            ))
            .unwrap()
            .as_slice(),
        )
        .unwrap();
        m.component("demo-client").unwrap().clone()
    }

    fn touch(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"").unwrap();
    }

    #[test]
    fn missing_when_nowhere() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(detect(&demo(), &env(t.path())), ComponentStatus::Missing);
    }

    #[test]
    fn present_in_a_system_dir() {
        let t = tempfile::tempdir().unwrap();
        touch(&t.path().join("system/libdemo.so"));
        let s = detect(&demo(), &env(t.path()));
        assert!(
            matches!(
                &s,
                ComponentStatus::Installed {
                    source: Source::System,
                    ..
                }
            ),
            "{s:?}"
        );
    }

    #[test]
    fn app_managed_newest_version_wins_and_minimum_applies() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("drivers/demo-client");
        touch(&d.join("1.5/demo/libdemo.so"));
        let s = detect(&demo(), &env(t.path()));
        assert!(
            matches!(&s, ComponentStatus::TooOld { version, required, .. } if version == "1.5" && required == "2.0"),
            "{s:?}"
        );
        touch(&d.join("2.1/demo/libdemo.so"));
        touch(&d.join(".staging-x/demo/libdemo.so"));
        let s = detect(&demo(), &env(t.path()));
        assert!(
            matches!(&s, ComponentStatus::Installed { version: Some(v), source: Source::AppManaged, .. } if v == "2.1"),
            "{s:?}"
        );
    }

    #[test]
    fn user_path_and_env_var() {
        let t = tempfile::tempdir().unwrap();
        touch(&t.path().join("custom/libdemo.so"));
        let mut e = env(t.path());
        e.overrides
            .insert("demo-client".into(), t.path().join("custom"));
        let s = detect(&demo(), &e);
        assert!(
            matches!(
                &s,
                ComponentStatus::Installed {
                    source: Source::UserPath,
                    ..
                }
            ),
            "{s:?}"
        );

        let agent = Manifest::bundled().component("ssh-agent").unwrap().clone();
        let sock = t.path().join("agent.sock");
        touch(&sock);
        let mut e = env(t.path());
        assert_eq!(detect(&agent, &e), ComponentStatus::Missing);
        let s = sock.display().to_string();
        e.var = Box::new(move |k| (k == "SSH_AUTH_SOCK").then(|| s.clone()));
        assert!(detect(&agent, &e).is_installed());

        // No SSH_AUTH_SOCK, but 1Password's socket is there.
        let home = t.path().join("home");
        touch(&home.join(".1password/agent.sock"));
        let h = home.display().to_string();
        e.var = Box::new(move |k| (k == "HOME").then(|| h.clone()));
        assert!(
            matches!(detect(&agent, &e), ComponentStatus::Installed { location, .. } if location.contains("1password")),
        );
    }

    #[test]
    fn builtin_on_windows() {
        let t = tempfile::tempdir().unwrap();
        let mut e = env(t.path());
        e.os = Os::Windows;
        let g = Manifest::bundled().component("gssapi").unwrap().clone();
        assert!(matches!(
            detect(&g, &e),
            ComponentStatus::Installed {
                source: Source::Builtin,
                ..
            }
        ));
    }

    /// A machine where `programs` maps CLI names to (path, version output).
    fn cli_env(programs: &[(&'static str, &'static str, &'static str)]) -> DetectEnv {
        let t = std::env::temp_dir();
        let mut e = env(&t);
        let found: Vec<(String, PathBuf, String)> = programs
            .iter()
            .map(|(n, p, v)| ((*n).to_owned(), PathBuf::from(p), (*v).to_owned()))
            .collect();
        let f2 = found.clone();
        e.program = Box::new(move |name| {
            found
                .iter()
                .find(|(n, _, _)| n == name)
                .map(|(_, p, _)| p.clone())
        });
        e.version_of = Box::new(move |path| {
            f2.iter()
                .find(|(_, p, _)| p == path)
                .and_then(|(_, _, v)| version::parse_version(v))
        });
        e
    }

    #[test]
    fn coding_clis_installed_missing_and_unsupported() {
        let m = Manifest::bundled();
        let spec = |id: &str| m.component(id).unwrap().clone();
        let (claude, codex, gemini) = (spec("claude-code"), spec("codex-cli"), spec("gemini-cli"));

        let none = cli_env(&[]);
        for s in [&claude, &codex, &gemini] {
            assert_eq!(detect(s, &none), ComponentStatus::Missing, "{}", s.id);
        }

        let good = cli_env(&[
            ("claude", "/usr/bin/claude", "2.1.292 (Claude Code)"),
            ("codex", "/usr/bin/codex", "codex-cli 0.160.1"),
            ("gemini", "/usr/bin/gemini", "0.63.0"),
        ]);
        for (s, v) in [
            (&claude, "2.1.292"),
            (&codex, "0.160.1"),
            (&gemini, "0.63.0"),
        ] {
            assert_eq!(
                detect(s, &good),
                ComponentStatus::Installed {
                    version: Some(v.into()),
                    location: format!("/usr/bin/{}", s.detect.programs[0]),
                    source: Source::System,
                },
                "{}",
                s.id
            );
        }

        let old = cli_env(&[
            ("claude", "/usr/bin/claude", "1.0.40 (Claude Code)"),
            ("codex", "/usr/bin/codex", "codex-cli 0.12.0"),
            ("gemini", "/usr/bin/gemini", "0.9.1"),
        ]);
        for s in [&claude, &codex, &gemini] {
            assert!(
                matches!(detect(s, &old), ComponentStatus::TooOld { .. }),
                "{}",
                s.id
            );
        }

        let new = cli_env(&[
            ("claude", "/usr/bin/claude", "3.0.1 (Claude Code)"),
            ("codex", "/usr/bin/codex", "codex-cli 1.2.0"),
            ("gemini", "/usr/bin/gemini", "1.0.0"),
        ]);
        for s in [&claude, &codex, &gemini] {
            assert!(
                matches!(detect(s, &new), ComponentStatus::TooNew { .. }),
                "{}",
                s.id
            );
        }

        // A version that cannot be read still counts as installed (shown without one).
        let odd = cli_env(&[("codex", "/usr/bin/codex", "codex (dev build)")]);
        assert!(matches!(
            detect(&codex, &odd),
            ComponentStatus::Installed { version: None, .. }
        ));
    }

    #[test]
    fn a_chosen_cli_path_wins() {
        let t = tempfile::tempdir().unwrap();
        let mine = t.path().join("my-claude");
        std::fs::write(&mine, "").unwrap();
        let mut e = cli_env(&[("claude", "/usr/bin/claude", "2.1.0")]);
        e.overrides.insert("claude-code".into(), mine.clone());
        let mine2 = mine.clone();
        e.version_of = Box::new(move |p| (p == mine2).then(|| "2.2.0".into()));
        let s = Manifest::bundled()
            .component("claude-code")
            .unwrap()
            .clone();
        assert_eq!(
            detect(&s, &e),
            ComponentStatus::Installed {
                version: Some("2.2.0".into()),
                location: mine.display().to_string(),
                source: Source::UserPath,
            }
        );
    }
}
