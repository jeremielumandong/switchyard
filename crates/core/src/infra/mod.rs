//! Infra engineer tools, shared by the app's tabs: the Terraform / OpenTofu state viewer,
//! the cloud resource inventory, network checks and the log viewer.
//!
//! The UI sends [`InfraOp`]s ([`crate::Command::Infra`]) and receives [`InfraUpdate`]s
//! ([`crate::Event::Infra`]) tagged with its request id. Long-running work (log tails, a
//! multi-account inventory) streams several updates and stops on
//! [`crate::Command::InfraCancel`]. Everything here is read-only: no state is written,
//! no cloud resource changed, and Host commands are fixed read-only tools (`tail`,
//! `journalctl`, `getent`, `openssl s_client`, `curl`, `nc`).

pub mod logline;
pub mod netcheck;
pub mod x509;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use secrecy::SecretString;
use switchyard_cloud::inventory::CloudResource;
use switchyard_cloud::inventory::azure::LaWorkspace;
use switchyard_cloud::logs::LogGroup;
use switchyard_cloud::tfstate::TfState;
use switchyard_store::ProfileId;

use crate::bus::RequestId;
use logline::LogLine;
use netcheck::{CheckResult, NetCheck};

/// Where a Terraform state comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TfSource {
    /// A local `.tfstate` file.
    File(PathBuf),
    /// An S3 backend object, read through a saved S3 connection's credentials.
    S3 {
        /// Saved S3 cloud connection.
        conn: ProfileId,
        /// Bucket.
        bucket: String,
        /// Key (`env/prod/terraform.tfstate`).
        key: String,
        /// Object version (S3 versioning), else the current one.
        version: Option<String>,
    },
    /// An Azure Blob backend, read through a saved Blob Storage connection.
    Blob {
        /// Saved Azure Blob connection.
        conn: ProfileId,
        /// Container.
        container: String,
        /// Blob name (`prod.terraform.tfstate`).
        blob: String,
        /// Blob version id, else the current one.
        version: Option<String>,
    },
    /// An HCP Terraform / Terraform Enterprise workspace (API token in the keychain).
    Cloud {
        /// Host name (empty: `app.terraform.io`).
        host: String,
        /// Organization.
        organization: String,
        /// Workspace name.
        workspace: String,
        /// State version id (`sv-…`), else the current state.
        version: Option<String>,
    },
}

impl TfSource {
    /// Short description for headers and errors (never a secret).
    pub fn label(&self) -> String {
        let v = |v: &Option<String>| {
            v.as_ref()
                .map(|v| format!(" @ {}", v.chars().take(12).collect::<String>()))
                .unwrap_or_default()
        };
        match self {
            TfSource::File(p) => p.display().to_string(),
            TfSource::S3 {
                bucket,
                key,
                version,
                ..
            } => format!("s3://{bucket}/{key}{}", v(version)),
            TfSource::Blob {
                container,
                blob,
                version,
                ..
            } => format!("{container}/{blob}{}", v(version)),
            TfSource::Cloud {
                organization,
                workspace,
                version,
                ..
            } => format!("{organization}/{workspace}{}", v(version)),
        }
    }

    /// The same source at another version.
    pub fn at_version(&self, id: Option<String>) -> TfSource {
        let mut s = self.clone();
        match &mut s {
            TfSource::File(_) => {}
            TfSource::S3 { version, .. }
            | TfSource::Blob { version, .. }
            | TfSource::Cloud { version, .. } => *version = id,
        }
        s
    }
}

/// A stored version of a state (S3 / Blob versions, HCP Terraform state versions).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TfVersion {
    /// Version id to load it with.
    pub id: String,
    /// When written, ms since the epoch.
    pub modified_ms: Option<i64>,
    /// Size in bytes, when known.
    pub size: Option<u64>,
    /// Serial, when known before loading (HCP Terraform).
    pub serial: Option<u64>,
    /// The current version.
    pub latest: bool,
}

/// One account to inventory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InventoryAccount {
    /// A profile of the AWS CLI's config files.
    AwsProfile {
        /// Profile name.
        profile: String,
        /// Regions (empty: the profile's; `all`: every enabled region).
        regions: Vec<String>,
    },
    /// A saved cloud connection's sign-in: AWS (keys or profile), Azure (Microsoft
    /// sign-in or Azure CLI) or Cloudflare (API token).
    Connection {
        /// Saved cloud connection.
        conn: ProfileId,
        /// AWS regions (as for [`InventoryAccount::AwsProfile`]).
        regions: Vec<String>,
    },
    /// The Azure CLI's signed-in account (`az login`), every subscription it sees.
    AzureCli,
    /// A Google Cloud project with the Google Cloud CLI's sign-in.
    Gcp {
        /// Project id (empty: gcloud's default).
        project: String,
    },
}

/// What a log tail follows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogSource {
    /// A file on an SSH Host (`tail -F`).
    HostFile {
        /// Host.
        host: ProfileId,
        /// Absolute path.
        path: String,
        /// Lines of history first.
        lines: u32,
    },
    /// The systemd journal on an SSH Host (`journalctl -f -o json`).
    Journal {
        /// Host.
        host: ProfileId,
        /// One unit (`nginx.service`), else everything.
        unit: Option<String>,
        /// Entries of history first.
        lines: u32,
    },
    /// An AWS CloudWatch Logs group, polled with `FilterLogEvents`.
    CloudWatch {
        /// Saved AWS cloud connection.
        conn: ProfileId,
        /// Region (empty: the connection's).
        region: String,
        /// Log group.
        group: String,
        /// Filter pattern (CloudWatch syntax).
        pattern: String,
        /// Minutes of history first.
        since_mins: u32,
        /// Keep polling for new events.
        follow: bool,
    },
    /// An Azure Monitor Log Analytics workspace, queried with KQL.
    LogAnalytics {
        /// Saved Azure cloud connection (Microsoft sign-in or Azure CLI); `None`: the
        /// Azure CLI.
        conn: Option<ProfileId>,
        /// Workspace (customer) id.
        workspace: String,
        /// KQL query.
        query: String,
        /// Minutes of history first.
        since_mins: u32,
        /// Re-run for new rows.
        follow: bool,
    },
}

/// A request to the infra tools.
#[derive(Clone, Debug)]
pub enum InfraOp {
    /// Load a state ([`InfraUpdate::TfState`]).
    TfLoad(TfSource),
    /// List a state's stored versions ([`InfraUpdate::TfVersions`]).
    TfVersions(TfSource),
    /// Save the API token for an HCP Terraform host in the keychain ([`InfraUpdate::Done`]).
    TfSaveToken {
        /// Host (empty: `app.terraform.io`).
        host: String,
        /// Token.
        token: SecretString,
    },
    /// Whether a token is saved for a host ([`InfraUpdate::TfToken`]).
    TfHasToken {
        /// Host.
        host: String,
    },
    /// The AWS CLI profiles on this computer ([`InfraUpdate::AwsProfiles`]).
    AwsProfiles,
    /// Inventory these accounts: one [`InfraUpdate::Inventory`] per account, then
    /// [`InfraUpdate::Done`].
    Inventory(Vec<InventoryAccount>),
    /// Run one network check ([`InfraUpdate::Check`]).
    Check(NetCheck),
    /// CloudWatch log groups of a connection ([`InfraUpdate::LogGroups`]).
    LogGroups {
        /// Saved AWS cloud connection.
        conn: ProfileId,
        /// Region (empty: the connection's).
        region: String,
        /// Name prefix.
        prefix: String,
    },
    /// Log Analytics workspaces visible to an Azure sign-in ([`InfraUpdate::Workspaces`]).
    Workspaces {
        /// Saved Azure connection; `None`: the Azure CLI.
        conn: Option<ProfileId>,
    },
    /// Follow a log ([`InfraUpdate::LogLines`] until cancelled, then
    /// [`InfraUpdate::LogEnded`]).
    Tail(LogSource),
}

/// A result or progress of an [`InfraOp`].
#[derive(Clone, Debug)]
pub enum InfraUpdate {
    /// A loaded state.
    TfState {
        /// Where from.
        label: String,
        /// The state, or why it could not be read.
        result: Result<Arc<TfState>, String>,
    },
    /// Stored versions, newest first.
    TfVersions(Result<Vec<TfVersion>, String>),
    /// Whether a token is saved.
    TfToken(bool),
    /// AWS CLI profile names.
    AwsProfiles(Vec<String>),
    /// One account's resources (and services that failed in it).
    Inventory {
        /// Account label.
        account: String,
        /// Resources found.
        items: Vec<CloudResource>,
        /// Per-service failures (`eu-west-1: EKS: AccessDenied…`).
        errors: Vec<String>,
    },
    /// A check's result.
    Check(CheckResult),
    /// Log groups.
    LogGroups(Result<Vec<LogGroup>, String>),
    /// Log Analytics workspaces.
    Workspaces(Result<Vec<LaWorkspace>, String>),
    /// New log lines, oldest first.
    LogLines(Vec<LogLine>),
    /// A status line for the log viewer (`Connected to web-1`, `Polling every 5 s`).
    LogStatus(String),
    /// The tail stopped: `None` when it ended normally or was stopped.
    LogEnded(Option<String>),
    /// The request finished (`Err` when it failed as a whole).
    Done(Result<(), String>),
}

/// Running infra requests, so [`crate::Command::InfraCancel`] can stop them.
#[derive(Default)]
pub(crate) struct InfraTasks {
    running: Mutex<HashMap<RequestId, futures::future::AbortHandle>>,
}

impl InfraTasks {
    pub(crate) fn insert(&self, request: RequestId, handle: futures::future::AbortHandle) {
        if let Ok(mut m) = self.running.lock() {
            m.insert(request, handle);
        }
    }

    pub(crate) fn remove(&self, request: RequestId) {
        if let Ok(mut m) = self.running.lock() {
            m.remove(&request);
        }
    }

    /// Stop `request`; true when it was running.
    pub(crate) fn cancel(&self, request: RequestId) -> bool {
        let h = self
            .running
            .lock()
            .ok()
            .and_then(|mut m| m.remove(&request));
        if let Some(h) = &h {
            h.abort();
        }
        h.is_some()
    }
}

/// Keychain key of an HCP Terraform host's API token.
pub fn tf_token_key(host: &str) -> switchyard_store::SecretRef {
    let host = host.trim().trim_end_matches('/');
    let host = if host.is_empty() {
        switchyard_cloud::tfcloud::DEFAULT_HOST
    } else {
        host
    };
    switchyard_store::SecretRef(format!("infra:tfc-token:{}", host.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_labels_and_versions() {
        let s = TfSource::S3 {
            conn: ProfileId("c".into()),
            bucket: "state".into(),
            key: "prod.tfstate".into(),
            version: None,
        };
        assert_eq!(s.label(), "s3://state/prod.tfstate");
        let v = s.at_version(Some("3HL4kqtJlcpXroDTDmJ+rmSpXd3dIbrHY".into()));
        assert_eq!(v.label(), "s3://state/prod.tfstate @ 3HL4kqtJlcpX");
        assert_eq!(tf_token_key("").0, "infra:tfc-token:app.terraform.io");
        assert_eq!(tf_token_key("TFE.corp/").0, "infra:tfc-token:tfe.corp");
    }
}
