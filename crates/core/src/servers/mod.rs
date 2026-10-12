//! Servers and workspaces of the Default side: named workspaces (scope and layout),
//! sharing them as files, the Servers view's reachability checks, shared credentials,
//! and remote desktops (RDP and VNC) opened with the system's client (MX-12) or, for
//! RDP, in an app tab ([`embedded`]).
//!
//! None of this is reachable from the MCP server or `swy`: agents never see workspaces,
//! credentials or remote desktops, and never launch a client.

pub mod embedded;
pub mod launch;
pub mod rdp;
pub mod reach;

use std::path::PathBuf;

use secrecy::SecretString;
use switchyard_store::workspace_file::{ImportChoice, ImportItem};
use switchyard_store::{ProfileId, ServerFacts, Workspace, WorkspaceScope};

use crate::bus::RequestId;
pub use reach::ReachStatus;

/// Setting: reachability checks on (`true`, the default) or off.
pub const REACH_SETTING: &str = "servers.reachability";
/// Seconds between two reachability rounds.
pub const REACH_INTERVAL_SECS: u64 = 30;
/// Connection attempts at once.
pub const REACH_CONCURRENCY: usize = 16;
/// Timeout of one attempt.
pub const REACH_TIMEOUT_MS: u64 = 3_000;

/// A request from the UI ([`crate::Command::Servers`]).
#[derive(Clone, Debug)]
pub enum ServersCommand {
    /// Emit [`ServersEvent::Workspaces`] and [`ServersEvent::Facts`].
    Load,
    /// Make `id` current. With `load`, answer with [`crate::Event::Workspace`] (its
    /// layout and buffers) so the UI restores it.
    Switch {
        /// Workspace.
        id: String,
        /// Send the layout back.
        load: bool,
    },
    /// Create a workspace ([`ServersEvent::Created`]).
    Create {
        /// Request.
        request: RequestId,
        /// Name.
        name: String,
        /// Scope.
        scope: WorkspaceScope,
    },
    /// Rename a workspace.
    Rename {
        /// Workspace.
        id: String,
        /// New name.
        name: String,
    },
    /// Copy a workspace ([`ServersEvent::Created`]).
    Duplicate {
        /// Request.
        request: RequestId,
        /// Workspace.
        id: String,
        /// Name of the copy.
        name: String,
    },
    /// Replace a workspace's scope (the settings sheet).
    SetScope {
        /// Workspace.
        id: String,
        /// Scope.
        scope: WorkspaceScope,
    },
    /// What deleting would also remove ([`ServersEvent::DeletePreview`]).
    PreviewDelete {
        /// Workspace.
        id: String,
    },
    /// Delete a workspace; with `delete_profiles`, also the items in no other workspace
    /// and their stored secrets.
    Delete {
        /// Workspace.
        id: String,
        /// Also delete its exclusive items.
        delete_profiles: bool,
    },
    /// Add items to a workspace.
    Add {
        /// Workspace.
        id: String,
        /// Items.
        profiles: Vec<ProfileId>,
    },
    /// Remove items from a workspace (they stay in "All").
    Remove {
        /// Workspace.
        id: String,
        /// Items.
        profiles: Vec<ProfileId>,
    },
    /// Write workspace `id` to `path`; with a passphrase, include its secrets encrypted.
    Export {
        /// Workspace.
        id: String,
        /// File.
        path: PathBuf,
        /// "Include secrets".
        passphrase: Option<SecretString>,
    },
    /// Read a workspace file and answer [`ServersEvent::ImportPreview`].
    PreviewImport {
        /// File.
        path: PathBuf,
    },
    /// Import a workspace file ([`ServersEvent::Imported`]).
    Import {
        /// File.
        path: PathBuf,
        /// Passphrase of its encrypted secrets (none: they are left to fill in).
        passphrase: Option<SecretString>,
        /// Per-item choice for items that exist already.
        choices: Vec<(ProfileId, ImportChoice)>,
    },
    /// Check these profiles' reachability every [`REACH_INTERVAL_SECS`] until stopped
    /// or replaced ([`ServersEvent::Reach`]).
    StartReachability {
        /// Hosts and remote desktops.
        ids: Vec<ProfileId>,
    },
    /// Stop the reachability checks.
    StopReachability,
    /// Open a remote desktop with the system client ([`ServersEvent::Launch`]).
    Launch {
        /// Request.
        request: RequestId,
        /// The remote desktop.
        id: ProfileId,
    },
    /// Stop a launched client (and its tunnel).
    CancelLaunch {
        /// Request.
        request: RequestId,
    },
    /// Which profiles use a credential ([`ServersEvent::CredentialUsers`]).
    CredentialUsers {
        /// Credential.
        id: ProfileId,
    },
    /// Delete a credential. When profiles use it, `detach` must be set: they stop using
    /// it (and need their own sign-in) before it goes.
    DeleteCredential {
        /// Credential.
        id: ProfileId,
        /// Detach its users first.
        detach: bool,
    },
}

/// An imported item whose secret still needs a value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissingSecret {
    /// The item here.
    pub id: ProfileId,
    /// Its name.
    pub name: String,
    /// `password` or `passphrase`.
    pub purpose: String,
}

/// What an import file holds, for the preview dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportPreviewInfo {
    /// The workspace's name.
    pub workspace: String,
    /// It exists here (it is updated).
    pub workspace_exists: bool,
    /// Items.
    pub items: Vec<ImportItem>,
    /// Secret values are included, encrypted.
    pub encrypted: bool,
}

/// Where a launch is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaunchState {
    /// The client started (its name).
    Started(String),
    /// The client exited or was stopped.
    Ended,
    /// It could not start; with the component id to install when the client is missing.
    Failed {
        /// Why.
        message: String,
        /// Driver Manager component to install (`rdp-client`, `vnc-client`).
        install: Option<String>,
    },
}

/// Answers for the Servers features ([`crate::Event::Servers`]).
#[derive(Clone, Debug)]
pub enum ServersEvent {
    /// Every workspace and the current one.
    Workspaces {
        /// Workspaces ("All" first), without buffers.
        list: Vec<Workspace>,
        /// The current one's id.
        current: String,
    },
    /// A workspace was created or duplicated.
    Created {
        /// Request.
        request: RequestId,
        /// The new workspace.
        workspace: Box<Workspace>,
    },
    /// What deleting a workspace would also remove.
    DeletePreview {
        /// Workspace.
        id: String,
        /// Items in no other workspace: (id, name, kind badge).
        exclusive: Vec<(ProfileId, String, String)>,
    },
    /// A workspace was deleted.
    Deleted {
        /// Workspace.
        id: String,
        /// Items deleted with it.
        deleted: usize,
        /// Items kept because something else uses them.
        kept: Vec<String>,
    },
    /// An import file was read.
    ImportPreview {
        /// File.
        path: PathBuf,
        /// What it holds, or why it could not be read.
        result: Result<ImportPreviewInfo, String>,
    },
    /// A workspace was imported.
    Imported {
        /// Its id here.
        workspace: String,
        /// Imported items whose secret still needs a value.
        missing: Vec<MissingSecret>,
        /// Summary for a toast.
        summary: String,
    },
    /// Import failed (wrong passphrase, damaged file).
    ImportFailed {
        /// File.
        path: PathBuf,
        /// Why.
        message: String,
        /// The passphrase was wrong (ask again).
        wrong_passphrase: bool,
    },
    /// What the Servers view knows per profile.
    Facts(Vec<ServerFacts>),
    /// One reachability result.
    Reach {
        /// Profile.
        id: ProfileId,
        /// Status.
        status: ReachStatus,
    },
    /// A remote desktop launch changed state.
    Launch {
        /// Request.
        request: RequestId,
        /// The remote desktop.
        id: ProfileId,
        /// State.
        state: LaunchState,
    },
    /// The profiles that use a credential: (id, name).
    CredentialUsers {
        /// Credential.
        id: ProfileId,
        /// Users.
        users: Vec<(ProfileId, String)>,
    },
}
