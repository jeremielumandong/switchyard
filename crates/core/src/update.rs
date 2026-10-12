//! In-app updates (M6-4), the same way Emulsion does them: a launch-time check of the latest
//! GitHub release, a verified background download, and installation when Switchyard
//! restarts.
//!
//! The latest published release is compared with the running version. When it is newer and
//! this copy was installed from a release package ([`Install`]), the matching asset is
//! downloaded and checked against the SHA-256 digest GitHub records for it (or the
//! `.sha256` file published beside it) before anything is replaced. When a minisign public
//! key was configured at build time (`SWITCHYARD_UPDATE_PUBKEY`) and the release carries a
//! `.minisig`, the signature must verify too. Development builds, packaged Linux installs and
//! unpacked copies are only told about the release; they are never modified.
//!
//! Runs on the core runtime; the UI sees [`crate::Event::UpdateStatus`],
//! [`crate::Event::UpdateProgress`] and [`crate::Event::UpdateDownloaded`].

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

use serde::Deserialize;
use sha2::Digest as _;
use tokio::io::AsyncWriteExt as _;

use crate::components::HttpFetcher;

/// GitHub repository the releases come from.
pub const REPOSITORY: &str = "jeremielumandong/switchyard";

/// Settings key: `false` turns off the check at startup ("Check for Updates" still works).
pub const CHECK_SETTING: &str = "updates.check";

/// Settings key: `false` stops the startup check from downloading a newer release by itself
/// (it is still announced).
pub const AUTO_DOWNLOAD_SETTING: &str = "updates.auto_download";

/// Optional minisign public key (base64), set at build time. When set, a release asset that
/// has a `.minisig` beside it must also pass the signature check.
pub const UPDATE_PUBLIC_KEY: Option<&str> = option_env!("SWITCHYARD_UPDATE_PUBKEY");

/// The running version.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Why a check or download failed.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    /// Network or HTTP failure.
    #[error("could not reach GitHub: {0}")]
    Http(String),
    /// GitHub answered something unexpected.
    #[error("unexpected release data: {0}")]
    BadRelease(String),
    /// The release has no download for this installation.
    #[error("the release has no {0} download")]
    MissingAsset(String),
    /// Neither GitHub nor a `.sha256` file gives the asset's checksum.
    #[error("the release publishes no checksum for {0}, so it was not installed")]
    MissingChecksum(String),
    /// The download's size or SHA-256 differs from the published one (it is deleted).
    #[error("{0} did not match its published checksum and was discarded")]
    Checksum(String),
    /// A downloaded file failed its minisign signature check (it is deleted).
    #[error("the download failed its signature check: {0}")]
    Signature(String),
    /// The user cancelled the download.
    #[error("download cancelled")]
    Cancelled,
    /// Local file error.
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// How this copy of Switchyard was installed, which decides how it updates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Install {
    /// A Linux AppImage at this path, replaced in place.
    AppImage(PathBuf),
    /// A Windows per-user installation (NSIS); the new installer runs after exit.
    WindowsInstaller,
    /// A macOS application bundle; the new disk image is opened.
    MacApp(PathBuf),
    /// A development build, a distribution package, Flatpak or an unpacked copy: nothing is
    /// replaced, the release page is offered instead.
    Unmanaged,
}

impl Install {
    /// This process's installation.
    pub fn detect() -> Self {
        if cfg!(debug_assertions) {
            return Install::Unmanaged;
        }
        let exe = std::env::current_exe().ok();
        Self::from_environment(
            exe.as_deref(),
            std::env::var_os("APPIMAGE").map(PathBuf::from),
            std::env::var_os("FLATPAK_ID").is_some(),
        )
    }

    fn from_environment(exe: Option<&Path>, appimage: Option<PathBuf>, flatpak: bool) -> Self {
        if cfg!(target_os = "linux") {
            if flatpak {
                return Install::Unmanaged;
            }
            return match appimage {
                Some(path) if path.is_absolute() && path.is_file() => Install::AppImage(path),
                _ => Install::Unmanaged,
            };
        }
        let Some(exe) = exe else {
            return Install::Unmanaged;
        };
        if cfg!(windows) {
            // The NSIS installer (packaging/windows/switchyard.nsi) writes its uninstaller
            // beside the executable.
            return match exe.parent() {
                Some(dir) if dir.join("uninstall.exe").is_file() => Install::WindowsInstaller,
                _ => Install::Unmanaged,
            };
        }
        if cfg!(target_os = "macos") {
            // …/Switchyard.app/Contents/MacOS/switchyard
            return match exe.ancestors().nth(3) {
                Some(app) if app.extension().is_some_and(|e| e == "app") => {
                    Install::MacApp(app.to_path_buf())
                }
                _ => Install::Unmanaged,
            };
        }
        Install::Unmanaged
    }

    /// The stable-named release asset this installation updates from, if it can update (see
    /// `scripts/upload-release-assets.py`).
    pub fn asset_name(&self) -> Option<&'static str> {
        match self {
            Install::AppImage(_) if cfg!(target_arch = "x86_64") => {
                Some("Switchyard-linux-x86_64.AppImage")
            }
            Install::WindowsInstaller if cfg!(target_arch = "x86_64") => {
                Some("Switchyard-windows-x64-setup.exe")
            }
            Install::MacApp(_) => Some("Switchyard-macos-universal.dmg"),
            _ => None,
        }
    }
}

/// Where to fetch an update from and how to check it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateAsset {
    /// File name.
    pub name: String,
    /// Download URL.
    pub url: String,
    /// Size GitHub reports.
    pub size: u64,
    /// SHA-256 (lowercase hex) GitHub recorded at upload, when it did.
    pub sha256: Option<String>,
    /// The `<name>.sha256` file, when published.
    pub checksum_url: Option<String>,
    /// The `<name>.minisig` file, when published.
    pub signature_url: Option<String>,
}

/// A newer release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateInfo {
    /// Version without the `v` (`0.6.0`).
    pub version: String,
    /// The release page on github.com.
    pub release_url: String,
    /// This installation's download; `None` when it can't update itself (the release page
    /// is offered instead).
    pub asset: Option<UpdateAsset>,
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

/// A downloaded, verified update waiting for a restart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadyUpdate {
    /// Its version.
    pub version: String,
    /// The release page.
    pub release_url: String,
    /// What to start after Switchyard exits: the replaced AppImage, the Windows installer,
    /// or the macOS disk image (opened, not run).
    pub file: PathBuf,
    /// How this copy is installed.
    pub install: Install,
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

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.core[0], self.core[1], self.core[2])?;
        if !self.pre.is_empty() {
            write!(f, "-{}", self.pre.join("."))?;
        }
        Ok(())
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
    /// Size in bytes.
    #[serde(default)]
    pub size: u64,
    /// `sha256:<hex>`, recorded by GitHub at upload.
    #[serde(default)]
    pub digest: Option<String>,
}

impl Release {
    fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }

    /// The download for `name`, with whatever checks the release publishes for it.
    pub fn update_asset(&self, name: &str) -> Option<UpdateAsset> {
        let asset = self.asset(name)?;
        let url_of = |n: String| self.asset(&n).map(|a| a.browser_download_url.clone());
        Some(UpdateAsset {
            name: asset.name.clone(),
            url: asset.browser_download_url.clone(),
            size: asset.size,
            sha256: asset
                .digest
                .as_deref()
                .and_then(|d| d.strip_prefix("sha256:"))
                .filter(|hex| is_sha256(hex))
                .map(str::to_ascii_lowercase),
            checksum_url: url_of(format!("{name}.sha256")),
            signature_url: url_of(format!("{name}.minisig")),
        })
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

/// Check `data` against a `.minisig` signature with `public_key`.
pub fn verify_signature(data: &[u8], signature: &str, public_key: &str) -> Result<(), UpdateError> {
    switchyard_drivers::signature::verify(data, signature, public_key)
        .map_err(|e| UpdateError::Signature(e.to_string()))
}

fn is_sha256(hex: &str) -> bool {
    hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `<hex>  <name>` as written by sha256sum / shasum, for exactly this file.
pub fn parse_checksum_file(text: &str, name: &str) -> Option<String> {
    let mut fields = text.trim_start_matches('\u{feff}').split_whitespace();
    let hex = fields.next()?;
    let file = fields.next()?.trim_start_matches('*');
    (is_sha256(hex) && file == name).then(|| hex.to_ascii_lowercase())
}

/// Run one check for `install`.
pub async fn check(install: &Install) -> UpdateStatus {
    match check_inner(install).await {
        Ok(status) => status,
        Err(e) => {
            tracing::warn!(error = %e, "update check failed");
            UpdateStatus::Failed(e.to_string())
        }
    }
}

async fn check_inner(install: &Install) -> Result<UpdateStatus, UpdateError> {
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
    status_for(&release, install, CURRENT_VERSION)
}

/// What `release` means for this installation.
pub fn status_for(
    release: &Release,
    install: &Install,
    current: &str,
) -> Result<UpdateStatus, UpdateError> {
    let Some(latest) = newer_release(release, current)? else {
        return Ok(UpdateStatus::UpToDate {
            current: current.into(),
        });
    };
    let version = latest.to_string();
    tracing::info!(%version, "update available");
    Ok(UpdateStatus::Available(UpdateInfo {
        version,
        release_url: release.html_url.clone(),
        asset: install.asset_name().and_then(|n| release.update_asset(n)),
    }))
}

/// Download `info`'s asset into `dir` (only one pending update is kept there), verify it,
/// and stage it for `install`. `progress` gets (bytes, total); `cancel` stops the download.
pub async fn download(
    info: &UpdateInfo,
    install: &Install,
    dir: &Path,
    progress: &(dyn Fn(u64, u64) + Send + Sync),
    cancel: &Arc<AtomicBool>,
) -> Result<ReadyUpdate, UpdateError> {
    let asset = info
        .asset
        .as_ref()
        .ok_or_else(|| UpdateError::MissingAsset(std::env::consts::OS.into()))?;
    let fetcher = HttpFetcher::new().map_err(|e| UpdateError::Http(e.to_string()))?;
    let http = fetcher.client();
    let expected = match &asset.sha256 {
        Some(hex) => hex.clone(),
        None => {
            let url = asset
                .checksum_url
                .as_deref()
                .ok_or_else(|| UpdateError::MissingChecksum(asset.name.clone()))?;
            let text = get_small(http, url).await?;
            parse_checksum_file(&text, &asset.name)
                .ok_or_else(|| UpdateError::MissingChecksum(asset.name.clone()))?
        }
    };
    let key = UPDATE_PUBLIC_KEY.filter(|k| !k.trim().is_empty());
    let signature = match (key, &asset.signature_url) {
        (Some(_), Some(url)) => Some(get_small(http, url).await?),
        _ => None,
    };

    let _ = tokio::fs::remove_dir_all(dir).await;
    tokio::fs::create_dir_all(dir).await?;
    let dest = dir.join(&asset.name);
    let part = dir.join(format!("{}.part", asset.name));
    if let Err(e) = fetch(http, asset, &part, &expected, progress, cancel).await {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(e);
    }
    if let (Some(key), Some(sig)) = (key, signature) {
        let data = tokio::fs::read(&part).await?;
        let key = key.to_owned();
        let verified = tokio::task::spawn_blocking(move || verify_signature(&data, &sig, &key))
            .await
            .map_err(|e| UpdateError::Signature(e.to_string()))?;
        if let Err(e) = verified {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(e);
        }
    }
    tokio::fs::rename(&part, &dest).await?;
    let file = match install {
        Install::AppImage(target) => {
            let (dest, target) = (dest.clone(), target.clone());
            tokio::task::spawn_blocking(move || replace_appimage(&dest, &target))
                .await
                .map_err(|e| UpdateError::Io(std::io::Error::other(e.to_string())))??
        }
        _ => dest,
    };
    Ok(ReadyUpdate {
        version: info.version.clone(),
        release_url: info.release_url.clone(),
        file,
        install: install.clone(),
    })
}

/// A checksum or signature file (at most 4 KB).
async fn get_small(http: &reqwest::Client, url: &str) -> Result<String, UpdateError> {
    let resp = http
        .get(url)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| UpdateError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(UpdateError::Http(format!(
            "{url} answered HTTP {}",
            resp.status().as_u16()
        )));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| UpdateError::Http(e.to_string()))?;
    if bytes.len() > 4096 {
        return Err(UpdateError::BadRelease(format!("{url} is too large")));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

async fn fetch(
    http: &reqwest::Client,
    asset: &UpdateAsset,
    part: &Path,
    expected: &str,
    progress: &(dyn Fn(u64, u64) + Send + Sync),
    cancel: &Arc<AtomicBool>,
) -> Result<(), UpdateError> {
    let http_err = |e: reqwest::Error| UpdateError::Http(e.to_string());
    let mut resp = http.get(&asset.url).send().await.map_err(http_err)?;
    if !resp.status().is_success() {
        return Err(UpdateError::Http(format!(
            "{} answered HTTP {}",
            asset.name,
            resp.status().as_u16()
        )));
    }
    let total = if asset.size > 0 {
        asset.size
    } else {
        resp.content_length().unwrap_or(0)
    };
    let mut out = tokio::io::BufWriter::new(tokio::fs::File::create(part).await?);
    let mut hasher = sha2::Sha256::new();
    let (mut got, mut reported) = (0u64, 0u64);
    while let Some(chunk) = resp.chunk().await.map_err(http_err)? {
        if cancel.load(AtomicOrdering::Relaxed) {
            return Err(UpdateError::Cancelled);
        }
        got += chunk.len() as u64;
        if total > 0 && got > total {
            return Err(UpdateError::Checksum(asset.name.clone()));
        }
        out.write_all(&chunk).await?;
        hasher.update(&chunk);
        if got - reported >= 1 << 20 {
            reported = got;
            progress(got, total);
        }
    }
    out.flush().await?;
    out.into_inner().sync_all().await?;
    progress(got, total);
    let digest: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if (total > 0 && got != total) || digest != expected {
        return Err(UpdateError::Checksum(asset.name.clone()));
    }
    Ok(())
}

/// Atomically replace the AppImage at `target` with the verified `download`. The running
/// copy keeps its open file, so it is unaffected until restart. Returns `target`.
pub fn replace_appimage(download: &Path, target: &Path) -> Result<PathBuf, UpdateError> {
    let dir = target
        .parent()
        .ok_or_else(|| std::io::Error::other("the AppImage has no parent folder"))?;
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Switchyard.AppImage".into());
    let staged = dir.join(format!(".{name}.update-{}", std::process::id()));
    let result = (|| {
        std::fs::copy(download, &staged)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
        }
        std::fs::File::open(&staged)?.sync_all()?;
        std::fs::rename(&staged, target)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    result?;
    let _ = std::fs::remove_file(download);
    Ok(target.to_path_buf())
}

/// Start what a staged update left for after exit: the relaunched AppImage or the Windows
/// installer. Called by the app binary once its event loop has returned.
pub fn run_after_exit(ready: &ReadyUpdate) {
    if matches!(ready.install, Install::MacApp(_) | Install::Unmanaged) {
        return;
    }
    if let Err(error) = std::process::Command::new(&ready.file)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        tracing::warn!(%error, program = %ready.file.display(), "could not start the update");
    }
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
            "assets": [
                {"name": "Switchyard-macos-universal.dmg", "size": 3,
                 "browser_download_url": "https://example.invalid/x.dmg",
                 "digest": "sha256:BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD"},
                {"name": "Switchyard-windows-x64-setup.exe", "size": 3,
                 "browser_download_url": "https://example.invalid/s.exe", "digest": null},
                {"name": "Switchyard-windows-x64-setup.exe.sha256", "size": 99,
                 "browser_download_url": "https://example.invalid/s.exe.sha256"},
                {"name": "Switchyard-windows-x64-setup.exe.minisig", "size": 99,
                 "browser_download_url": "https://example.invalid/s.exe.minisig"}
            ],
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
        verify_signature(data.as_bytes(), &sig, &key).unwrap();
        let tampered = format!("{data} ");
        assert!(matches!(
            verify_signature(tampered.as_bytes(), &sig, &key),
            Err(UpdateError::Signature(_))
        ));
    }

    #[test]
    fn assets_carry_their_checks() {
        let r = release("v0.6.0");
        let dmg = r.update_asset("Switchyard-macos-universal.dmg").unwrap();
        assert_eq!(dmg.size, 3);
        assert_eq!(
            dmg.sha256.as_deref(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
        assert_eq!(dmg.checksum_url, None);
        let exe = r.update_asset("Switchyard-windows-x64-setup.exe").unwrap();
        assert_eq!(exe.sha256, None);
        assert_eq!(
            exe.checksum_url.as_deref(),
            Some("https://example.invalid/s.exe.sha256")
        );
        assert_eq!(
            exe.signature_url.as_deref(),
            Some("https://example.invalid/s.exe.minisig")
        );
        assert!(r.update_asset("Switchyard-linux-x86_64.AppImage").is_none());
    }

    #[test]
    fn status_offers_a_download_only_to_managed_installs() {
        let r = release("v0.6.0");
        let UpdateStatus::Available(info) =
            status_for(&r, &Install::MacApp("/A/Switchyard.app".into()), "0.5.0").unwrap()
        else {
            panic!("expected an update");
        };
        assert_eq!(info.version, "0.6.0");
        assert_eq!(
            info.asset.map(|a| a.name),
            Some("Switchyard-macos-universal.dmg".to_owned())
        );
        let UpdateStatus::Available(info) = status_for(&r, &Install::Unmanaged, "0.5.0").unwrap()
        else {
            panic!("expected an update");
        };
        assert_eq!(info.asset, None);
        assert_eq!(
            status_for(&r, &Install::Unmanaged, "0.6.0").unwrap(),
            UpdateStatus::UpToDate {
                current: "0.6.0".into()
            }
        );
    }

    #[test]
    fn checksum_files_name_exactly_this_file() {
        let hex = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(
            parse_checksum_file(&format!("{hex}  a.exe\n"), "a.exe").as_deref(),
            Some(hex)
        );
        assert_eq!(
            parse_checksum_file(&format!("\u{feff}{hex} *a.exe"), "a.exe").as_deref(),
            Some(hex)
        );
        assert_eq!(parse_checksum_file(&format!("{hex}  b.exe"), "a.exe"), None);
        assert_eq!(parse_checksum_file("abc  a.exe", "a.exe"), None);
    }

    #[test]
    fn versions_print_without_the_v() {
        assert_eq!(v("v1.2.3").to_string(), "1.2.3");
        assert_eq!(v("1.0.0-rc.1").to_string(), "1.0.0-rc.1");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_updates_only_appimages() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("Switchyard.AppImage");
        std::fs::write(&image, b"x").unwrap();
        assert_eq!(
            Install::from_environment(None, Some(image.clone()), false),
            Install::AppImage(image.clone())
        );
        assert_eq!(
            Install::from_environment(None, Some(image), true),
            Install::Unmanaged
        );
        assert_eq!(
            Install::from_environment(None, Some("relative.AppImage".into()), false),
            Install::Unmanaged
        );
        assert_eq!(
            Install::from_environment(None, None, false),
            Install::Unmanaged
        );
    }

    #[cfg(unix)]
    #[test]
    fn appimage_is_replaced_in_place() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("Switchyard.AppImage");
        let download = dir.path().join("new.AppImage");
        std::fs::write(&target, b"old").unwrap();
        std::fs::write(&download, b"new").unwrap();
        assert_eq!(replace_appimage(&download, &target).unwrap(), target);
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert!(!download.exists());
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
    }

    /// Serves `body` for every request on 127.0.0.1; returns the base URL.
    async fn serve(body: &'static [u8]) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body).await;
                let _ = sock.shutdown().await;
            }
        });
        format!("http://{addr}")
    }

    fn info(url: String, sha256: &str) -> UpdateInfo {
        UpdateInfo {
            version: "9.0.0".into(),
            release_url: "https://github.com/x/y/releases/tag/v9.0.0".into(),
            asset: Some(UpdateAsset {
                name: "Switchyard-windows-x64-setup.exe".into(),
                url,
                size: 3,
                sha256: Some(sha256.into()),
                checksum_url: None,
                signature_url: None,
            }),
        }
    }

    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[tokio::test]
    async fn downloads_are_verified_before_staging() {
        let url = serve(b"abc").await;
        let dir = tempfile::tempdir().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let ready = download(
            &info(url.clone(), ABC),
            &Install::WindowsInstaller,
            &dir.path().join("updates"),
            &|_, _| {},
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(&ready.file).unwrap(), b"abc");
        assert_eq!(ready.version, "9.0.0");

        let wrong = "0".repeat(64);
        let err = download(
            &info(url, &wrong),
            &Install::WindowsInstaller,
            &dir.path().join("updates"),
            &|_, _| {},
            &cancel,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, UpdateError::Checksum(_)), "{err}");
        let left: Vec<_> = std::fs::read_dir(dir.path().join("updates"))
            .unwrap()
            .collect();
        assert!(left.is_empty(), "a rejected download is deleted");
    }

    #[tokio::test]
    async fn unmanaged_installs_have_nothing_to_download() {
        let dir = tempfile::tempdir().unwrap();
        let mut i = info("http://127.0.0.1:9".into(), ABC);
        i.asset = None;
        let err = download(
            &i,
            &Install::Unmanaged,
            dir.path(),
            &|_, _| {},
            &Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, UpdateError::MissingAsset(_)));
    }
}
