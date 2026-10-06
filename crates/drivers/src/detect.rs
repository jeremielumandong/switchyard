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
        }
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
}
