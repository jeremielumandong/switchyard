//! Driver Manager commands: detect, install (package, archive, file), use an existing path,
//! remove. Downloads use the app's rustls configuration; progress goes out on the bus.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::future::BoxFuture;
use switchyard_drivers::install::{
    self, CommandRunner, Fetcher, InstallPlan, ProgressFn, SystemRunner,
};
use switchyard_drivers::{DriverError, Registry};
use tokio::io::AsyncWriteExt as _;
use tracing::warn;

use crate::bus::Event;
use crate::runtime::EventSender;

/// Settings key for the enterprise download mirror.
pub const MIRROR_SETTING: &str = "drivers.mirror";

/// `reqwest` downloads with the app's TLS settings (certificate verification on).
pub(crate) struct HttpFetcher {
    http: reqwest::Client,
}

impl HttpFetcher {
    pub(crate) fn new() -> Result<Self, DriverError> {
        let tls = switchyard_db::tls::client_config()
            .map_err(|e| DriverError::Download(e.to_string()))?;
        let http = reqwest::Client::builder()
            .tls_backend_preconfigured(tls)
            .connect_timeout(std::time::Duration::from_secs(15))
            .user_agent(format!("Switchyard/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| DriverError::Download(e.to_string()))?;
        Ok(Self { http })
    }
}

impl Fetcher for HttpFetcher {
    fn fetch<'a>(
        &'a self,
        url: &'a str,
        dest: &'a Path,
        progress: &'a (dyn Fn(u64, Option<u64>) + Send + Sync),
    ) -> BoxFuture<'a, Result<(), DriverError>> {
        Box::pin(async move {
            let dl = |e: reqwest::Error| DriverError::Download(e.to_string());
            let mut resp = self.http.get(url).send().await.map_err(dl)?;
            if !resp.status().is_success() {
                return Err(DriverError::Download(format!(
                    "{url} answered HTTP {}",
                    resp.status().as_u16()
                )));
            }
            let total = resp.content_length();
            let mut file = tokio::fs::File::create(dest).await?;
            let mut done = 0u64;
            let mut last = 0u64;
            while let Some(chunk) = resp.chunk().await.map_err(dl)? {
                file.write_all(&chunk).await?;
                done += chunk.len() as u64;
                // About every 256 KB, so the bus is not flooded.
                if done - last >= 256 * 1024 {
                    progress(done, total);
                    last = done;
                }
            }
            file.flush().await?;
            progress(done, total);
            Ok(())
        })
    }
}

/// Outcome of an install, as the UI shows it.
pub(crate) fn failure_event(id: &str, e: &DriverError) -> Event {
    Event::ComponentFailed {
        id: id.to_owned(),
        message: e.to_string(),
        command: match e {
            DriverError::NeedsTerminal(cmd) => Some(cmd.clone()),
            _ => None,
        },
    }
}

/// Install `id` with the strategy the manifest gives for this machine.
pub(crate) async fn install(
    registry: &Registry,
    events: &EventSender,
    id: &str,
    accept_license: bool,
    from_file: Option<PathBuf>,
    runner: &dyn CommandRunner,
) -> Result<(), DriverError> {
    let c = registry.component(id)?;
    if let Some(l) = &c.license
        && l.accept_required
        && !accept_license
        && from_file.is_none()
    {
        return Err(DriverError::LicenseRequired(c.name));
    }
    let progress: ProgressFn = {
        let (events, id) = (events.clone(), id.to_owned());
        Arc::new(move |p| {
            events.emit(Event::ComponentProgress {
                id: id.clone(),
                progress: p,
            })
        })
    };
    let dir = registry.drivers_dir();
    match (&c.plan, from_file) {
        (InstallPlan::Archive { .. }, Some(file)) => {
            install::install_from_file(id, &c.plan, &file, &dir, &progress).await?;
        }
        (_, Some(_)) => {
            return Err(DriverError::NoStrategy(format!(
                "{} is not installed from a file",
                c.name
            )));
        }
        (InstallPlan::Archive { .. }, None) => {
            let fetcher = HttpFetcher::new()?;
            install::install_archive(id, &c.plan, &dir, &fetcher, &progress).await?;
        }
        (InstallPlan::Package { .. }, None) => {
            install::install_package(&c.plan, runner, &progress).await?;
        }
        (InstallPlan::Builtin, None) => {}
        (InstallPlan::Manual { .. }, None) => {
            return Err(DriverError::NoStrategy(format!(
                "{} is set up by hand; follow the steps shown",
                c.name
            )));
        }
        (InstallPlan::Unavailable { reason }, None) => {
            return Err(DriverError::NoStrategy(reason.clone()));
        }
    }
    let after = registry.component(id)?;
    if !after.status.is_installed() {
        warn!(%id, "installed but still not detected");
        return Err(DriverError::Io(format!(
            "{} was installed but Switchyard still cannot find it; try \"Use existing path\"",
            after.name
        )));
    }
    events.emit(Event::ComponentInstalled { component: after });
    Ok(())
}

/// The real package-manager runner.
pub(crate) fn system_runner() -> SystemRunner {
    SystemRunner
}
