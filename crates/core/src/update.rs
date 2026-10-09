//! Update checks (M6-4): ask GitHub Releases for the latest published version, compare it
//! with the running one, and, when a minisign public key was configured at build time
//! (`SWITCHYARD_UPDATE_PUBKEY`), download this platform's installer and check its
//! signature before offering it. Without a key the app only links to the release page.
//!
//! Runs on the core runtime; the UI sees [`crate::Event::UpdateStatus`].

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use switchyard_drivers::install::Fetcher as _;

use crate::components::HttpFetcher;

/// GitHub repository the releases come from.
pub const REPOSITORY: &str = "jeremielumandong/switchyard";

/// Settings key: `false` turns off the check at startup ("Check for Updates" still works).
pub const CHECK_SETTING: &str = "updates.check";

/// minisign public key (base64) for update installers, set at build time. Without it the
/// app only notifies.
pub const UPDATE_PUBLIC_KEY: Option<&str> = option_env!("SWITCHYARD_UPDATE_PUBKEY");

/// The running version.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Why a check failed.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    /// Network or HTTP failure.
    #[error("could not reach GitHub: {0}")]
    Http(String),
    /// GitHub answered something unexpected.
    #[error("unexpected release data: {0}")]
    BadRelease(String),
    /// A downloaded installer failed its signature check (it is deleted).
    #[error("the downloaded installer failed its signature check: {0}")]
    Signature(String),
    /// Local file error.
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// A newer release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateInfo {
    /// Version without the `v` (`0.6.0`).
    pub version: String,
    /// The release page on github.com.
    pub release_url: String,
    /// This platform's installer, downloaded and signature-checked; `None` when no key is
    /// configured, the platform has no installer (Linux), or the release lacks one.
    pub installer: Option<PathBuf>,
}

/// Outcome of a check ([`crate::Event::UpdateStatus`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateStatus {
    /// The running version is the latest.
    UpToDate {
        /// The running version.
        current: String,
    },
    /// A newer release exists.
    Available(UpdateInfo),
    /// The check failed.
    Failed(String),
}

/// A `major.minor.patch[-pre]` version. Build metadata is ignored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    core: [u64; 3],
    pre: Vec<String>,
}

impl Version {
    /// Parse `1.2.3`, `v1.2.3` or `1.2.3-beta.1`; `None` for anything else.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let text = text.strip_prefix('v').unwrap_or(text);
        let text = text.split('+').next().unwrap_or(text);
        let (core, pre) = match text.split_once('-') {
            Some((c, p)) if !p.is_empty() => (c, p.split('.').map(str::to_owned).collect()),
            Some(_) => return None,
            None => (text, Vec::new()),
        };
        let mut parts = core.split('.');
        let mut out = [0u64; 3];
        for slot in &mut out {
            let part = parts.next()?;
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            *slot = part.parse().ok()?;
        }
        if parts.next().is_some() {
            return None;
        }
        Some(Self { core: out, pre })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    /// Semantic-versioning precedence: a pre-release sorts before its release.
    fn cmp(&self, other: &Self) -> Ordering {
        self.core
            .cmp(&other.core)
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    for (a, b) in self.pre.iter().zip(&other.pre) {
                        let ord = match (a.parse::<u64>(), b.parse::<u64>()) {
                            (Ok(x), Ok(y)) => x.cmp(&y),
                            (Ok(_), Err(_)) => Ordering::Less,
                            (Err(_), Ok(_)) => Ordering::Greater,
                            (Err(_), Err(_)) => a.cmp(b),
                        };
                        if ord != Ordering::Equal {
                            return ord;
                        }
                    }
                    self.pre.len().cmp(&other.pre.len())
                }
            })
    }
}

/// The parts of GitHub's release JSON the check reads.
#[derive(Debug, Deserialize)]
pub struct Release {
    /// `v0.6.0`.
    pub tag_name: String,
    /// Release page.
    pub html_url: String,
    /// Drafts are never offered.
    #[serde(default)]
    pub draft: bool,
    /// Pre-releases are never offered.
    #[serde(default)]
    pub prerelease: bool,
    /// Attached files.
    #[serde(default)]
    pub assets: Vec<Asset>,
}

/// A release file.
#[derive(Debug, Deserialize)]
pub struct Asset {
    /// File name.
    pub name: String,
    /// Download URL.
    pub browser_download_url: String,
}

/// The stable asset name of this platform's installer (see `scripts/upload-release-assets.py`).
/// Linux has none: package managers and AppImage tools handle updates there.
pub fn installer_asset_name() -> Option<&'static str> {
    if cfg!(target_os = "macos") {
        Some("Switchyard-macos-universal.dmg")
    } else if cfg!(all(windows, target_arch = "x86_64")) {
        Some("Switchyard-windows-x64-setup.exe")
    } else {
        None
    }
}

/// Decide what a release means for `current`. `Ok(None)`: up to date.
pub fn newer_release(release: &Release, current: &str) -> Result<Option<Version>, UpdateError> {
    if release.draft || release.prerelease {
        return Ok(None);
    }
    let latest = Version::parse(&release.tag_name)
        .ok_or_else(|| UpdateError::BadRelease(format!("tag {:?}", release.tag_name)))?;
    let running = Version::parse(current)
        .ok_or_else(|| UpdateError::BadRelease(format!("running version {current:?}")))?;
    // The link is opened in a browser: only ever a github.com page.
    if !release.html_url.starts_with("https://github.com/") {
        return Err(UpdateError::BadRelease(format!(
            "release page {:?} is not on github.com",
            release.html_url
        )));
    }
    Ok((latest > running).then_some(latest))
}

/// Check `data` (an installer) against a `.minisig` signature with `public_key`.
pub fn verify_installer(data: &[u8], signature: &str, public_key: &str) -> Result<(), UpdateError> {
    switchyard_drivers::signature::verify(data, signature, public_key)
        .map_err(|e| UpdateError::Signature(e.to_string()))
}

/// Run one check. `updates_dir` receives verified installers.
pub async fn check(updates_dir: &Path) -> UpdateStatus {
    match check_inner(updates_dir).await {
        Ok(status) => status,
        Err(e) => {
            tracing::warn!(error = %e, "update check failed");
            UpdateStatus::Failed(e.to_string())
        }
    }
}

async fn check_inner(updates_dir: &Path) -> Result<UpdateStatus, UpdateError> {
    let fetcher = HttpFetcher::new().map_err(|e| UpdateError::Http(e.to_string()))?;
    let url = format!("https://api.github.com/repos/{REPOSITORY}/releases/latest");
    let resp = fetcher
        .client()
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| UpdateError::Http(e.to_string()))?;
    if resp.status().as_u16() == 404 {
        // No published release yet.
        return Ok(UpdateStatus::UpToDate {
            current: CURRENT_VERSION.into(),
        });
    }
    if !resp.status().is_success() {
        return Err(UpdateError::Http(format!(
            "GitHub answered HTTP {}",
            resp.status().as_u16()
        )));
    }
    let release: Release = resp
        .json()
        .await
        .map_err(|e| UpdateError::BadRelease(e.to_string()))?;
    let Some(latest) = newer_release(&release, CURRENT_VERSION)? else {
        return Ok(UpdateStatus::UpToDate {
            current: CURRENT_VERSION.into(),
        });
    };
    let version = format!(
        "{}.{}.{}{}",
        latest.core[0],
        latest.core[1],
        latest.core[2],
        if latest.pre.is_empty() {
            String::new()
        } else {
            format!("-{}", latest.pre.join("."))
        }
    );
    tracing::info!(%version, "update available");
    let key = UPDATE_PUBLIC_KEY.filter(|k| !k.trim().is_empty());
    let installer = match (key, installer_asset_name()) {
        (Some(key), Some(name)) => {
            download_verified(&fetcher, &release, name, key, &updates_dir.join(&version)).await?
        }
        _ => None,
    };
    Ok(UpdateStatus::Available(UpdateInfo {
        version,
        release_url: release.html_url,
        installer,
    }))
}

/// Download `name` and `name.minisig` from the release into `dir` and keep the installer only
/// if the signature verifies. `Ok(None)` when the release lacks either file.
async fn download_verified(
    fetcher: &HttpFetcher,
    release: &Release,
    name: &str,
    key: &str,
    dir: &Path,
) -> Result<Option<PathBuf>, UpdateError> {
    let sig_name = format!("{name}.minisig");
    let find = |n: &str| {
        release
            .assets
            .iter()
            .find(|a| a.name == n)
            .map(|a| a.browser_download_url.clone())
    };
    let (Some(file_url), Some(sig_url)) = (find(name), find(&sig_name)) else {
        tracing::info!(asset = name, "release has no signed installer; notify only");
        return Ok(None);
    };
    tokio::fs::create_dir_all(dir).await?;
    let file = dir.join(name);
    let sig = dir.join(&sig_name);
    let none = |_: u64, _: Option<u64>| {};
    for (url, dest) in [(&file_url, &file), (&sig_url, &sig)] {
        fetcher
            .fetch(url, dest, &none)
            .await
            .map_err(|e| UpdateError::Http(e.to_string()))?;
    }
    let data = tokio::fs::read(&file).await?;
    let signature = tokio::fs::read_to_string(&sig).await?;
    let verified = tokio::task::spawn_blocking({
        let key = key.to_owned();
        move || verify_installer(&data, &signature, &key)
    })
    .await
    .map_err(|e| UpdateError::Signature(e.to_string()))?;
    if let Err(e) = verified {
        let _ = tokio::fs::remove_file(&file).await;
        let _ = tokio::fs::remove_file(&sig).await;
        return Err(e);
    }
    Ok(Some(file))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn parses_versions() {
        assert_eq!(v("v1.2.3"), v("1.2.3"));
        assert_eq!(v("1.2.3+build.5"), v("1.2.3"));
        for bad in ["", "1.2", "1.2.3.4", "1.x.3", "v", "1.2.3-", "-1.2.3"] {
            assert!(Version::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn orders_like_semver() {
        let ordered = [
            "0.9.9",
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0",
            "1.0.1",
            "1.10.0",
            "2.0.0",
        ];
        for pair in ordered.windows(2) {
            assert!(v(pair[0]) < v(pair[1]), "{} < {}", pair[0], pair[1]);
        }
    }

    fn release(tag: &str) -> Release {
        serde_json::from_value(serde_json::json!({
            "tag_name": tag,
            "html_url": format!("https://github.com/{REPOSITORY}/releases/tag/{tag}"),
            "assets": [{"name": "Switchyard-macos-universal.dmg",
                        "browser_download_url": "https://example.invalid/x.dmg"}],
        }))
        .unwrap()
    }

    #[test]
    fn newer_tags_are_offered() {
        assert_eq!(
            newer_release(&release("v0.6.0"), "0.5.2").unwrap(),
            Some(v("0.6.0"))
        );
        assert_eq!(newer_release(&release("v0.5.2"), "0.5.2").unwrap(), None);
        assert_eq!(newer_release(&release("v0.5.1"), "0.5.2").unwrap(), None);
    }

    #[test]
    fn drafts_prereleases_and_foreign_links_are_ignored() {
        let mut r = release("v9.0.0");
        r.prerelease = true;
        assert_eq!(newer_release(&r, "0.1.0").unwrap(), None);
        let mut r = release("v9.0.0");
        r.html_url = "https://evil.example/releases".into();
        assert!(newer_release(&r, "0.1.0").is_err());
        assert!(newer_release(&release("nightly"), "0.1.0").is_err());
    }

    const FIX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../drivers/tests/fixtures/");

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("{FIX}{name}")).unwrap()
    }

    #[test]
    fn installer_signature_is_checked() {
        let data = fixture("manifest.json");
        let sig = fixture("manifest.json.minisig");
        let key = fixture("test.pub");
        verify_installer(data.as_bytes(), &sig, &key).unwrap();
        let tampered = format!("{data} ");
        assert!(matches!(
            verify_installer(tampered.as_bytes(), &sig, &key),
            Err(UpdateError::Signature(_))
        ));
    }
}
