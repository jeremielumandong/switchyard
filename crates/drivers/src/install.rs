//! Install strategies: a system package (exact command, run after confirmation with the
//! platform's elevation prompt), a signed vendor archive unpacked into the app-managed
//! directory, the same archive from a local file, and manual steps.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use data_encoding::HEXLOWER;
use futures::future::BoxFuture;
use ring::digest::{Context, SHA256};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::error::{DriverError, Result};
use crate::manifest::{ComponentSpec, Os, Platform};

/// What "Install automatically" would do here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstallPlan {
    /// Part of the OS; nothing to do.
    Builtin,
    /// A system package.
    Package {
        /// Package manager (`apt`, `dnf`, `pacman`, `zypper`, `brew`, `winget`).
        manager: String,
        /// Package name.
        package: String,
        /// Approximate size.
        size: Option<String>,
        /// The command to run, without elevation.
        argv: Vec<String>,
        /// The exact command as the user would type it (`sudo apt-get install -y …`).
        display: String,
    },
    /// A vendor archive into the app-managed directory.
    Archive {
        /// Version.
        version: String,
        /// Download URL (mirror applied).
        url: String,
        /// Expected SHA-256.
        sha256: String,
        /// Size in bytes.
        size: u64,
    },
    /// Steps the user follows.
    Manual {
        /// Instructions.
        steps: Vec<String>,
    },
    /// Nothing known for this machine.
    Unavailable {
        /// Why.
        reason: String,
    },
}

/// Package managers per OS, in preference order.
fn managers(os: Os) -> &'static [&'static str] {
    match os {
        Os::Linux => &["apt", "dnf", "pacman", "zypper"],
        Os::Macos => &["brew"],
        Os::Windows => &["winget"],
    }
}

/// The executable that proves a manager is present.
fn manager_binary(manager: &str) -> &str {
    match manager {
        "apt" => "apt-get",
        other => other,
    }
}

/// Install command for one package, without elevation.
pub fn package_argv(manager: &str, package: &str) -> Vec<String> {
    let v: &[&str] = match manager {
        "apt" => &["apt-get", "install", "-y"],
        "dnf" => &["dnf", "install", "-y"],
        "pacman" => &["pacman", "-S", "--noconfirm"],
        "zypper" => &["zypper", "--non-interactive", "install"],
        "brew" => &["brew", "install"],
        "winget" => &[
            "winget",
            "install",
            "--exact",
            "--accept-source-agreements",
            "--accept-package-agreements",
            "--id",
        ],
        _ => &[],
    };
    v.iter()
        .map(|s| (*s).to_owned())
        .chain(std::iter::once(package.to_owned()))
        .collect()
}

/// Whether the manager needs admin rights (Homebrew must not run as root; winget asks for
/// elevation itself).
pub fn needs_elevation(manager: &str) -> bool {
    !matches!(manager, "brew" | "winget")
}

/// Replace an archive URL's origin with an internal mirror (keeps the file name).
pub fn apply_mirror(url: &str, mirror: Option<&str>) -> String {
    match mirror.map(str::trim).filter(|m| !m.is_empty()) {
        Some(m) => {
            let file = url.rsplit('/').next().unwrap_or(url);
            format!("{}/{file}", m.trim_end_matches('/'))
        }
        None => url.to_owned(),
    }
}

/// What installing `spec` would do on `os`. `has` says whether an executable is on PATH.
pub fn plan(
    spec: &ComponentSpec,
    os: Os,
    mirror: Option<&str>,
    has: &dyn Fn(&str) -> bool,
) -> InstallPlan {
    match spec.platforms.get(os) {
        None => InstallPlan::Unavailable {
            reason: format!("{} is not available on this platform", spec.name),
        },
        Some(Platform::Builtin) => InstallPlan::Builtin,
        Some(Platform::Manual { steps }) => InstallPlan::Manual {
            steps: steps.clone(),
        },
        Some(Platform::Archive {
            version,
            url,
            sha256,
            size,
            ..
        }) => InstallPlan::Archive {
            version: version.clone(),
            url: apply_mirror(url, mirror),
            sha256: sha256.to_ascii_lowercase(),
            size: *size,
        },
        Some(Platform::Package { packages, size }) => {
            let found = managers(os)
                .iter()
                .find(|m| packages.contains_key(**m) && has(manager_binary(m)));
            match found {
                Some(m) => {
                    let package = packages[*m].clone();
                    let argv = package_argv(m, &package);
                    let display = if needs_elevation(m) {
                        format!("sudo {}", argv.join(" "))
                    } else {
                        argv.join(" ")
                    };
                    InstallPlan::Package {
                        manager: (*m).to_owned(),
                        package,
                        size: size.clone(),
                        argv,
                        display,
                    }
                }
                None => InstallPlan::Unavailable {
                    reason: format!(
                        "no supported package manager found; install one of: {}",
                        packages
                            .iter()
                            .map(|(m, p)| format!("{p} ({m})"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                },
            }
        }
    }
}

/// Whether `name` is an executable on PATH.
pub fn on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    let exts: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd"]
    } else {
        &[""]
    };
    std::env::split_paths(&path)
        .any(|d| exts.iter().any(|e| d.join(format!("{name}{e}")).is_file()))
}

/// Install progress, for the card and Settings → Drivers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstallProgress {
    /// Downloading.
    Downloading {
        /// Bytes so far.
        done: u64,
        /// Total, when known.
        total: Option<u64>,
    },
    /// Checking the SHA-256.
    Verifying,
    /// Unpacking.
    Unpacking,
    /// Running a package manager.
    Running {
        /// The command.
        command: String,
    },
}

/// Progress callback.
pub type ProgressFn = Arc<dyn Fn(InstallProgress) + Send + Sync>;

/// Downloads a URL to a file (core implements it with `reqwest`; tests with a stub).
pub trait Fetcher: Send + Sync {
    /// Write `url` to `dest`, reporting (bytes so far, total).
    fn fetch<'a>(
        &'a self,
        url: &'a str,
        dest: &'a Path,
        progress: &'a (dyn Fn(u64, Option<u64>) + Send + Sync),
    ) -> BoxFuture<'a, Result<()>>;
}

/// Output of a package-manager run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandOutput {
    /// Exit status 0.
    pub success: bool,
    /// Combined stdout and stderr.
    pub output: String,
}

/// Runs package-manager commands (tests use a stub).
pub trait CommandRunner: Send + Sync {
    /// Run `argv`, elevated when `elevate` is set.
    fn run<'a>(&'a self, argv: &'a [String], elevate: bool)
    -> BoxFuture<'a, Result<CommandOutput>>;
}

/// The real runner: `pkexec` for elevation on Linux (the desktop's password dialog).
#[derive(Debug, Default)]
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run<'a>(
        &'a self,
        argv: &'a [String],
        elevate: bool,
    ) -> BoxFuture<'a, Result<CommandOutput>> {
        Box::pin(async move {
            let mut full: Vec<String> = Vec::new();
            if elevate {
                if cfg!(target_os = "linux") && on_path("pkexec") {
                    full.push("pkexec".into());
                } else {
                    return Err(DriverError::NeedsTerminal(format!(
                        "sudo {}",
                        argv.join(" ")
                    )));
                }
            }
            full.extend(argv.iter().cloned());
            let (bin, args) = full
                .split_first()
                .ok_or_else(|| DriverError::Command("empty command".into()))?;
            let mut cmd = tokio::process::Command::new(bin);
            cmd.args(args).stdin(std::process::Stdio::null());
            // Output is captured; don't flash a console window on Windows.
            #[cfg(windows)]
            cmd.creation_flags(crate::detect::CREATE_NO_WINDOW);
            if let Some(original) = crate::registry::original_loader_path() {
                match original {
                    Some(v) => cmd.env("LD_LIBRARY_PATH", v),
                    None => cmd.env_remove("LD_LIBRARY_PATH"),
                };
            }
            let out = cmd
                .output()
                .await
                .map_err(|e| DriverError::Command(format!("could not run {bin}: {e}")))?;
            let mut output = String::from_utf8_lossy(&out.stdout).into_owned();
            output.push_str(&String::from_utf8_lossy(&out.stderr));
            Ok(CommandOutput {
                success: out.status.success(),
                output,
            })
        })
    }
}

/// Run the package manager for a [`InstallPlan::Package`].
pub async fn install_package(
    plan: &InstallPlan,
    runner: &dyn CommandRunner,
    progress: &ProgressFn,
) -> Result<()> {
    let InstallPlan::Package {
        manager,
        argv,
        display,
        ..
    } = plan
    else {
        return Err(DriverError::NoStrategy("not a package install".into()));
    };
    progress(InstallProgress::Running {
        command: display.clone(),
    });
    let shown: &str = display;
    info!(command = shown, "running package manager");
    let out = runner.run(argv, needs_elevation(manager)).await?;
    if out.success {
        Ok(())
    } else {
        // pkexec: 126 = dialog dismissed, 127 = not authorized; either way show the tail.
        let tail: Vec<&str> = out.output.lines().rev().take(6).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        Err(DriverError::Command(format!(
            "{display} failed{}",
            if tail.is_empty() {
                String::new()
            } else {
                format!(":\n{}", tail.join("\n"))
            }
        )))
    }
}

fn sha256_file(path: &Path) -> Result<String> {
    use std::io::Read as _;
    let mut f = std::fs::File::open(path)?;
    let mut ctx = Context::new(&SHA256);
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        ctx.update(&buf[..n]);
    }
    Ok(HEXLOWER.encode(ctx.finish().as_ref()))
}

fn staging_name(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!(".{prefix}-{}-{nanos}", std::process::id())
}

/// Verify `archive` against `sha256`, unpack it and move it to
/// `<drivers_dir>/<id>/<version>/`. The archive file is left for the caller.
async fn verify_and_unpack(
    id: &str,
    version: &str,
    sha256: &str,
    archive: PathBuf,
    drivers_dir: &Path,
    progress: &ProgressFn,
) -> Result<PathBuf> {
    progress(InstallProgress::Verifying);
    let expected = sha256.to_ascii_lowercase();
    let file = archive
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default();
    let a = archive.clone();
    let actual = tokio::task::spawn_blocking(move || sha256_file(&a))
        .await
        .map_err(|e| DriverError::Io(e.to_string()))??;
    if actual != expected {
        return Err(DriverError::Checksum {
            file,
            expected,
            actual,
        });
    }
    progress(InstallProgress::Unpacking);
    let component_dir = drivers_dir.join(id);
    let staging = component_dir.join(staging_name("unpack"));
    let final_dir = component_dir.join(version);
    let (s, a) = (staging.clone(), archive.clone());
    let unpacked =
        tokio::task::spawn_blocking(move || -> Result<()> { crate::archive::extract(&a, &s) })
            .await
            .map_err(|e| DriverError::Io(e.to_string()))?;
    if let Err(e) = unpacked {
        let _ = tokio::fs::remove_dir_all(&staging).await;
        return Err(e);
    }
    if tokio::fs::try_exists(&final_dir).await.unwrap_or(false) {
        tokio::fs::remove_dir_all(&final_dir).await?;
    }
    tokio::fs::rename(&staging, &final_dir).await?;
    info!(%id, %version, dir = %final_dir.display(), "component installed");
    Ok(final_dir)
}

/// Download an [`InstallPlan::Archive`], verify it and unpack it.
pub async fn install_archive(
    id: &str,
    plan: &InstallPlan,
    drivers_dir: &Path,
    fetcher: &dyn Fetcher,
    progress: &ProgressFn,
) -> Result<PathBuf> {
    let InstallPlan::Archive {
        version,
        url,
        sha256,
        size,
    } = plan
    else {
        return Err(DriverError::NoStrategy("not an archive install".into()));
    };
    let component_dir = drivers_dir.join(id);
    tokio::fs::create_dir_all(&component_dir).await?;
    let download = component_dir.join(staging_name("download"));
    progress(InstallProgress::Downloading {
        done: 0,
        total: Some(*size),
    });
    let p = progress.clone();
    let report =
        move |done: u64, total: Option<u64>| p(InstallProgress::Downloading { done, total });
    let fetched = fetcher.fetch(url, &download, &report).await;
    let r = match fetched {
        Ok(()) => verify_and_unpack(id, version, sha256, download.clone(), drivers_dir, progress)
            .await
            // Name the download, not the temporary file it was saved to.
            .map_err(|e| match e {
                DriverError::Checksum {
                    expected, actual, ..
                } => DriverError::Checksum {
                    file: url.rsplit('/').next().unwrap_or(url).to_owned(),
                    expected,
                    actual,
                },
                other => other,
            }),
        Err(e) => Err(e),
    };
    let _ = tokio::fs::remove_file(&download).await;
    r
}

/// "Install from file": a pre-downloaded archive, checked against the manifest's SHA-256.
pub async fn install_from_file(
    id: &str,
    plan: &InstallPlan,
    file: &Path,
    drivers_dir: &Path,
    progress: &ProgressFn,
) -> Result<PathBuf> {
    let InstallPlan::Archive {
        version, sha256, ..
    } = plan
    else {
        return Err(DriverError::NoStrategy(
            "this component is not installed from an archive".into(),
        ));
    };
    verify_and_unpack(id, version, sha256, file.to_owned(), drivers_dir, progress).await
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use super::*;
    use crate::archive::tests::tar_gz;
    use crate::manifest::{Detect, Platforms};

    fn spec(linux: Platform) -> ComponentSpec {
        ComponentSpec {
            id: "demo".into(),
            name: "Demo".into(),
            needed_for: "tests".into(),
            required_by: vec![],
            license: None,
            detect: Detect {
                libraries: vec!["libdemo.so".into()],
                ..Detect::default()
            },
            platforms: Platforms {
                linux: Some(linux),
                ..Platforms::default()
            },
        }
    }

    fn archive_spec(bytes: &[u8]) -> ComponentSpec {
        let sha = HEXLOWER.encode(ring::digest::digest(&SHA256, bytes).as_ref());
        spec(Platform::Archive {
            version: "2.1".into(),
            url: "https://downloads.example.com/v/demo-2.1.tar.gz".into(),
            sha256: sha,
            size: bytes.len() as u64,
            lib_dir: Some("demo".into()),
        })
    }

    struct Stub(Vec<u8>, Mutex<Vec<String>>);

    impl Fetcher for Stub {
        fn fetch<'a>(
            &'a self,
            url: &'a str,
            dest: &'a Path,
            progress: &'a (dyn Fn(u64, Option<u64>) + Send + Sync),
        ) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                self.1.lock().unwrap().push(url.to_owned());
                progress(self.0.len() as u64 / 2, Some(self.0.len() as u64));
                tokio::fs::write(dest, &self.0).await?;
                progress(self.0.len() as u64, Some(self.0.len() as u64));
                Ok(())
            })
        }
    }

    fn recorder() -> (ProgressFn, Arc<Mutex<Vec<InstallProgress>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();
        (Arc::new(move |p| s.lock().unwrap().push(p)), seen)
    }

    #[test]
    fn package_plan_picks_the_available_manager() {
        let s = spec(Platform::Package {
            packages: BTreeMap::from([
                ("apt".into(), "libgssapi-krb5-2".into()),
                ("dnf".into(), "krb5-libs".into()),
            ]),
            size: Some("1.8 MB".into()),
        });
        let p = plan(&s, Os::Linux, None, &|b| b == "dnf");
        let InstallPlan::Package { display, argv, .. } = p else {
            panic!("{p:?}")
        };
        assert_eq!(display, "sudo dnf install -y krb5-libs");
        assert_eq!(argv[0], "dnf");
        let p = plan(&s, Os::Linux, None, &|_| false);
        assert!(matches!(p, InstallPlan::Unavailable { .. }), "{p:?}");
        assert!(matches!(
            plan(&s, Os::Windows, None, &|_| true),
            InstallPlan::Unavailable { .. }
        ));
    }

    #[test]
    fn mirror_keeps_the_file_name() {
        assert_eq!(
            apply_mirror(
                "https://download.oracle.com/a/b/instantclient-21.zip",
                Some("https://mirror.corp/drivers/")
            ),
            "https://mirror.corp/drivers/instantclient-21.zip"
        );
        assert_eq!(
            apply_mirror("https://x/y.tgz", Some(" ")),
            "https://x/y.tgz"
        );
    }

    #[tokio::test]
    async fn archive_download_verifies_and_unpacks() {
        let t = tempfile::tempdir().unwrap();
        let bytes = tar_gz(&[("demo/libdemo.so", b'0', b"lib")]);
        let s = archive_spec(&bytes);
        let p = plan(&s, Os::Linux, Some("https://mirror.corp"), &|_| false);
        let stub = Stub(bytes, Mutex::default());
        let (progress, seen) = recorder();
        let dir = install_archive("demo", &p, t.path(), &stub, &progress)
            .await
            .unwrap();
        assert_eq!(dir, t.path().join("demo/2.1"));
        assert!(dir.join("demo/libdemo.so").is_file());
        assert_eq!(
            stub.1.lock().unwrap()[0],
            "https://mirror.corp/demo-2.1.tar.gz"
        );
        let seen = seen.lock().unwrap();
        assert!(matches!(
            seen[0],
            InstallProgress::Downloading { done: 0, .. }
        ));
        assert!(seen.contains(&InstallProgress::Verifying));
        assert_eq!(seen.last(), Some(&InstallProgress::Unpacking));
        // Nothing left behind but the version folder.
        let names: Vec<_> = std::fs::read_dir(t.path().join("demo"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["2.1"]);
    }

    #[tokio::test]
    async fn bad_checksum_is_refused_and_cleaned_up() {
        let t = tempfile::tempdir().unwrap();
        let good = tar_gz(&[("demo/libdemo.so", b'0', b"lib")]);
        let s = archive_spec(&good);
        let p = plan(&s, Os::Linux, None, &|_| false);
        let evil = tar_gz(&[("demo/libdemo.so", b'0', b"evil")]);
        let (progress, _) = recorder();
        let err = install_archive(
            "demo",
            &p,
            t.path(),
            &Stub(evil, Mutex::default()),
            &progress,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&err, DriverError::Checksum { file, .. } if file == "demo-2.1.tar.gz"),
            "{err}"
        );
        assert_eq!(std::fs::read_dir(t.path().join("demo")).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn install_from_file_checks_the_manifest_hash() {
        let t = tempfile::tempdir().unwrap();
        let bytes = tar_gz(&[("demo/libdemo.so", b'0', b"lib")]);
        let s = archive_spec(&bytes);
        let p = plan(&s, Os::Linux, None, &|_| false);
        let file = t.path().join("demo-2.1.tar.gz");
        std::fs::write(&file, &bytes).unwrap();
        let (progress, _) = recorder();
        let dir = install_from_file("demo", &p, &file, &t.path().join("drivers"), &progress)
            .await
            .unwrap();
        assert!(dir.join("demo/libdemo.so").is_file());
        std::fs::write(&file, b"something else").unwrap();
        let err = install_from_file("demo", &p, &file, &t.path().join("drivers"), &progress)
            .await
            .unwrap_err();
        assert!(matches!(err, DriverError::Checksum { .. }), "{err}");
    }

    struct FakeRunner(bool, Mutex<Vec<(Vec<String>, bool)>>);

    impl CommandRunner for FakeRunner {
        fn run<'a>(
            &'a self,
            argv: &'a [String],
            elevate: bool,
        ) -> BoxFuture<'a, Result<CommandOutput>> {
            Box::pin(async move {
                self.1.lock().unwrap().push((argv.to_vec(), elevate));
                Ok(CommandOutput {
                    success: self.0,
                    output: "Reading package lists...\nE: Unable to locate package".into(),
                })
            })
        }
    }

    #[tokio::test]
    async fn package_install_runs_elevated_and_reports_failures() {
        let s = spec(Platform::Package {
            packages: BTreeMap::from([("apt".into(), "libgssapi-krb5-2".into())]),
            size: None,
        });
        let p = plan(&s, Os::Linux, None, &|_| true);
        let (progress, seen) = recorder();
        let ok = FakeRunner(true, Mutex::default());
        install_package(&p, &ok, &progress).await.unwrap();
        assert_eq!(
            ok.1.lock().unwrap()[0],
            (
                vec![
                    "apt-get".to_owned(),
                    "install".into(),
                    "-y".into(),
                    "libgssapi-krb5-2".into()
                ],
                true
            )
        );
        assert!(
            matches!(&seen.lock().unwrap()[0], InstallProgress::Running { command } if command == "sudo apt-get install -y libgssapi-krb5-2")
        );
        let err = install_package(&p, &FakeRunner(false, Mutex::default()), &progress)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("Unable to locate package"),
            "{err}"
        );
    }
}
