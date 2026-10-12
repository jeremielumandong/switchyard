//! Sharing a workspace as a `.switchyard-workspace.json` file (for a team's Git repo).
//!
//! The file holds the workspace (id, name, scope), its profiles (Hosts, connections,
//! remote desktops and the credentials they reference) and its tags and folders. Profiles
//! keep their ids, so importing an updated file updates the items instead of duplicating
//! them. Secret values are never written by default: every stored secret is listed as
//! "needs a value". With a passphrase, only the secret values are encrypted
//! (argon2id + ChaCha20-Poly1305, the vault's primitives); everything else stays readable
//! so the file diffs well. Named secrets (`{{vault.name}}`) are references and stay in
//! the profiles unresolved.

use std::collections::{BTreeSet, HashMap, HashSet};

use rusqlite::params;
use secrecy::{ExposeSecret as _, SecretString};
use serde::{Deserialize, Serialize};

use crate::error::{Result, StoreError};
use crate::model::{Profile, ProfileId, SecretRef};
use crate::random::{hex, random_bytes, unhex};
use crate::secrets::{Sealed, derive_key, open, seal};
use crate::store::{Store, now_ms};
use crate::workspaces::{ALL_ID, WorkspaceScope};

/// `format` of the file.
pub const FORMAT: &str = "switchyard-workspace";
/// Current version.
pub const VERSION: u32 = 1;
/// File name suffix (`prod.switchyard-workspace.json`).
pub const FILE_SUFFIX: &str = ".switchyard-workspace.json";
/// Plaintext sealed with the key to recognize a wrong passphrase.
const CHECK: &[u8] = b"switchyard-workspace-secrets-v1";

/// The workspace part of the file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SharedWorkspace {
    /// Stable id.
    pub id: String,
    /// Name.
    pub name: String,
    /// What it shows.
    pub scope: WorkspaceScope,
}

/// Whether a secret's value travels with the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SecretStatus {
    /// Not in the file; the importer types it.
    NeedsValue,
    /// In the file, encrypted with the passphrase.
    Encrypted,
}

/// One stored secret of a profile in the file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretEntry {
    /// The profile.
    pub profile: ProfileId,
    /// Its name (for people reading the file).
    pub name: String,
    /// `password` or `passphrase`.
    pub purpose: String,
    /// Whether the value is in the file.
    pub status: SecretStatus,
    /// The encrypted value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Sealed>,
}

/// Key derivation of the encrypted values.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Kdf {
    /// `argon2id`.
    pub algorithm: String,
    /// Salt, hex.
    pub salt: String,
    /// Memory cost, KiB (argon2's defaults; recorded for readers).
    pub memory_kib: u32,
    /// Iterations.
    pub iterations: u32,
    /// Lanes.
    pub parallelism: u32,
}

/// The secrets section.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedSecrets {
    /// Values are included and encrypted.
    pub encrypted: bool,
    /// `chacha20poly1305` when encrypted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cipher: Option<String>,
    /// Key derivation when encrypted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kdf: Option<Kdf>,
    /// A known value sealed with the key (wrong passphrase check).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<Sealed>,
    /// One entry per stored secret.
    #[serde(default)]
    pub entries: Vec<SecretEntry>,
}

/// A `.switchyard-workspace.json` file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceFile {
    /// [`FORMAT`].
    pub format: String,
    /// [`VERSION`].
    pub version: u32,
    /// The workspace.
    pub workspace: SharedWorkspace,
    /// Its profiles, without stored-secret references.
    pub profiles: Vec<Profile>,
    /// Tags its profiles carry.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Folders its profiles live in.
    #[serde(default)]
    pub folders: Vec<String>,
    /// Secrets.
    #[serde(default)]
    pub secrets: SharedSecrets,
}

/// A secret value of a profile (by its id in the file) to put into the file.
pub type SecretValue = (ProfileId, SecretString);

/// The purpose part of a profile's stored secret key (`<id>:passphrase`).
fn purpose_of(r: &SecretRef) -> String {
    r.0.rsplit_once(':')
        .map_or("password", |(_, p)| p)
        .to_owned()
}

impl WorkspaceFile {
    /// Parse a file, checking its format and version.
    pub fn parse(json: &str) -> Result<Self> {
        let f: WorkspaceFile = serde_json::from_str(json)?;
        if f.format != FORMAT {
            return Err(StoreError::Invalid(
                "This is not a Switchyard workspace file".into(),
            ));
        }
        if f.version > VERSION {
            return Err(StoreError::Invalid(format!(
                "This workspace file is version {}; update Switchyard to import it",
                f.version
            )));
        }
        Ok(f)
    }

    /// Pretty JSON.
    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Encrypt `values` into the file with `passphrase` (the opt-in "Include secrets").
    /// Entries without a value stay "needs a value".
    pub fn seal_secrets(
        &mut self,
        passphrase: &SecretString,
        values: &[SecretValue],
    ) -> Result<()> {
        if passphrase.expose_secret().is_empty() {
            return Err(StoreError::Invalid("Enter a passphrase".into()));
        }
        let salt: [u8; 16] = random_bytes();
        let cipher = derive_key(passphrase, &salt)?;
        let params = argon2::Params::default();
        self.secrets.encrypted = true;
        self.secrets.cipher = Some("chacha20poly1305".into());
        self.secrets.kdf = Some(Kdf {
            algorithm: "argon2id".into(),
            salt: hex(&salt),
            memory_kib: params.m_cost(),
            iterations: params.t_cost(),
            parallelism: params.p_cost(),
        });
        self.secrets.check = Some(seal(&cipher, CHECK)?);
        let by_id: HashMap<&ProfileId, &SecretString> =
            values.iter().map(|(i, v)| (i, v)).collect();
        for e in &mut self.secrets.entries {
            if let Some(v) = by_id.get(&e.profile) {
                e.value = Some(seal(&cipher, v.expose_secret().as_bytes())?);
                e.status = SecretStatus::Encrypted;
            }
        }
        Ok(())
    }

    /// Decrypt the included values with `passphrase`: (profile id in the file, value).
    /// A wrong passphrase is [`StoreError::WrongPassphrase`].
    pub fn open_secrets(&self, passphrase: &SecretString) -> Result<Vec<SecretValue>> {
        if !self.secrets.encrypted {
            return Ok(Vec::new());
        }
        let salt = self
            .secrets
            .kdf
            .as_ref()
            .and_then(|k| unhex(&k.salt))
            .ok_or(StoreError::WrongPassphrase)?;
        let cipher = derive_key(passphrase, &salt).map_err(|_| StoreError::WrongPassphrase)?;
        let check = self
            .secrets
            .check
            .as_ref()
            .ok_or(StoreError::WrongPassphrase)?;
        if open(&cipher, check).map_err(|_| StoreError::WrongPassphrase)? != CHECK {
            return Err(StoreError::WrongPassphrase);
        }
        let mut out = Vec::new();
        for e in &self.secrets.entries {
            if let Some(v) = &e.value {
                let bytes = open(&cipher, v).map_err(|_| StoreError::WrongPassphrase)?;
                let s = String::from_utf8(bytes).map_err(|_| StoreError::WrongPassphrase)?;
                out.push((e.profile.clone(), SecretString::from(s)));
            }
        }
        Ok(out)
    }
}

/// What to do with an item that exists already.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ImportChoice {
    /// Leave the local one.
    Skip,
    /// Update the local one (its stored secret stays).
    #[default]
    Replace,
    /// Import a copy under a new id, named "<name> (imported)".
    KeepBoth,
}

/// One profile of a file as the import preview shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportItem {
    /// Id in the file.
    pub id: ProfileId,
    /// Name in the file.
    pub name: String,
    /// Profile kind (`host`, `db`, `desktop`, `credential`, …).
    pub kind: String,
    /// The local profile with the same id, by name.
    pub existing: Option<String>,
    /// The local profile has exactly the same settings.
    pub identical: bool,
    /// Its secret travels with the file (encrypted).
    pub secret_included: bool,
    /// It has a secret the file does not carry.
    pub needs_secret: bool,
}

/// The import preview.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportPreview {
    /// Workspace name in the file.
    pub workspace: String,
    /// A workspace with the same id exists here (it is updated).
    pub workspace_exists: bool,
    /// Items.
    pub items: Vec<ImportItem>,
    /// The file's secrets are encrypted (a passphrase is needed to use them).
    pub encrypted: bool,
}

/// What an import did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportOutcome {
    /// The workspace's id here.
    pub workspace_id: String,
    /// File id → id here, for every imported profile.
    pub id_map: HashMap<ProfileId, ProfileId>,
    /// Profiles skipped.
    pub skipped: usize,
    /// Profiles updated in place.
    pub updated: usize,
    /// Profiles added.
    pub added: usize,
}

impl Store {
    /// The workspace `id` as a shareable file, without secret values (each stored secret
    /// is listed as "needs a value").
    pub fn export_workspace(&self, id: &str) -> Result<WorkspaceFile> {
        let w = self
            .workspace_by_id(id)?
            .ok_or_else(|| StoreError::NotFound(format!("workspace {id}")))?;
        let all = self.profiles()?;
        let visible = w.scope.visible(&all);
        let mut profiles = Vec::new();
        let mut entries = Vec::new();
        let mut tags = BTreeSet::new();
        let mut folders = BTreeSet::new();
        for p in all.iter().filter(|p| visible.contains(p.id())) {
            if matches!(p, Profile::Terminal(_)) && !w.scope.matches(p) {
                continue;
            }
            tags.extend(p.tags().iter().cloned());
            folders.extend(p.folder().map(str::to_owned));
            let mut copy = p.clone();
            match p.secret() {
                // A named secret is a reference: it stays, unresolved.
                Some(r) if r.named_secret().is_some() => {}
                Some(r) => {
                    entries.push(SecretEntry {
                        profile: p.id().clone(),
                        name: p.name().to_owned(),
                        purpose: purpose_of(r),
                        status: SecretStatus::NeedsValue,
                        value: None,
                    });
                    copy = copy.without_secret();
                }
                None => {}
            }
            profiles.push(copy);
        }
        let mut scope = w.scope.clone();
        if id == ALL_ID {
            scope = WorkspaceScope {
                members: profiles
                    .iter()
                    .filter(|p| p.is_connection())
                    .map(|p| p.id().clone())
                    .collect(),
                ..WorkspaceScope::default()
            };
        }
        Ok(WorkspaceFile {
            format: FORMAT.into(),
            version: VERSION,
            workspace: SharedWorkspace {
                id: w.id.clone(),
                name: w.name.clone(),
                scope,
            },
            profiles,
            tags: tags.into_iter().collect(),
            folders: folders.into_iter().collect(),
            secrets: SharedSecrets {
                entries,
                ..SharedSecrets::default()
            },
        })
    }

    /// What importing `file` would do, item by item.
    pub fn preview_workspace_import(&self, file: &WorkspaceFile) -> Result<ImportPreview> {
        let mut items = Vec::new();
        for p in &file.profiles {
            let local = self.profile(p.id())?;
            let entry = file.secrets.entries.iter().find(|e| &e.profile == p.id());
            items.push(ImportItem {
                id: p.id().clone(),
                name: p.name().to_owned(),
                kind: p.kind().to_owned(),
                existing: local.as_ref().map(|l| l.name().to_owned()),
                identical: local
                    .as_ref()
                    .is_some_and(|l| l.without_secret() == p.without_secret()),
                secret_included: entry.is_some_and(|e| e.status == SecretStatus::Encrypted),
                needs_secret: entry.is_some_and(|e| e.status == SecretStatus::NeedsValue),
            });
        }
        let workspace_exists = self.workspace_by_id(&file.workspace.id)?.is_some();
        Ok(ImportPreview {
            workspace: file.workspace.name.clone(),
            workspace_exists,
            items,
            encrypted: file.secrets.encrypted,
        })
    }

    /// Import `file`. Items that exist here follow `choices` (default: replace, keeping
    /// the local stored secret); new items are added with their ids. The workspace with
    /// the file's id is created or its name and scope updated. All or nothing.
    pub fn apply_workspace_import(
        &mut self,
        file: &WorkspaceFile,
        choices: &HashMap<ProfileId, ImportChoice>,
    ) -> Result<ImportOutcome> {
        let mut out = ImportOutcome::default();
        let mut keep_both: HashMap<ProfileId, ProfileId> = HashMap::new();
        let mut locals: HashMap<ProfileId, Profile> = HashMap::new();
        for p in &file.profiles {
            if let Some(l) = self.profile(p.id())? {
                if choices.get(p.id()).copied().unwrap_or_default() == ImportChoice::KeepBoth {
                    keep_both.insert(p.id().clone(), ProfileId::new());
                }
                locals.insert(p.id().clone(), l);
            }
        }
        let mut prepared: Vec<Profile> = Vec::new();
        for p in &file.profiles {
            let choice = choices.get(p.id()).copied().unwrap_or_default();
            let local = locals.get(p.id());
            if local.is_some() && choice == ImportChoice::Skip {
                out.skipped += 1;
                continue;
            }
            let mut q = p.clone();
            // A stored-secret reference never comes from a file.
            if q.secret().is_some_and(|r| r.named_secret().is_none()) {
                q = q.without_secret();
            }
            q.remap_references(&keep_both);
            match (local, keep_both.get(p.id())) {
                (Some(_), Some(new_id)) => {
                    q.reidentify(new_id.clone(), format!("{} (imported)", p.name()));
                    out.added += 1;
                }
                (Some(l), None) => {
                    if q.secret().is_none()
                        && let Some(r) = l.secret()
                    {
                        q.set_secret(r.clone());
                    }
                    out.updated += 1;
                }
                (None, _) => out.added += 1,
            }
            if let Profile::Host(h) = &mut q {
                h.tags = crate::tags::normalize_tags(&h.tags);
            }
            if let Profile::Desktop(d) = &mut q {
                d.tags = crate::tags::normalize_tags(&d.tags);
            }
            q.validate()?;
            out.id_map.insert(p.id().clone(), q.id().clone());
            prepared.push(q);
        }
        let mut scope = file.workspace.scope.clone();
        scope.all = false;
        for m in scope.members.iter_mut().chain(scope.excluded.iter_mut()) {
            if let Some(n) = keep_both.get(m) {
                *m = n.clone();
            }
        }
        let tx = self.conn.transaction()?;
        for q in &prepared {
            let data = serde_json::to_string(q)?;
            tx.execute(
                "INSERT INTO profiles (id, kind, name, sort_order, data, updated_at)
                 VALUES (?1, ?2, ?3, (SELECT COALESCE(MAX(sort_order), -1) + 1 FROM profiles), ?4, ?5)
                 ON CONFLICT(id) DO UPDATE SET kind = ?2, name = ?3, data = ?4, updated_at = ?5",
                params![q.id().0, q.kind(), q.name(), data, now_ms()],
            )?;
            if let Profile::Host(h) = q {
                tx.execute("DELETE FROM host_tags WHERE host_id = ?1", [&h.id.0])?;
                for tag in &h.tags {
                    tx.execute(
                        "INSERT OR IGNORE INTO host_tags (host_id, tag) VALUES (?1, ?2)",
                        params![h.id.0, tag],
                    )?;
                }
            }
        }
        // Every reference must resolve (to an imported item or one already here).
        let ids: HashSet<String> = {
            let mut stmt = tx.prepare("SELECT id FROM profiles")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        for q in &prepared {
            for r in q.references() {
                if !ids.contains(&r.0) {
                    return Err(StoreError::NotFound(format!(
                        "{} refers to a profile missing from the file",
                        q.name()
                    )));
                }
            }
        }
        let ws_id = if file.workspace.id.is_empty() || file.workspace.id == ALL_ID {
            crate::random::random_hex(8)
        } else {
            file.workspace.id.clone()
        };
        tx.commit()?;
        let mut w = self.workspace_by_id(&ws_id)?.unwrap_or_default();
        w.id = ws_id.clone();
        w.name = unique_name(self, &file.workspace.name, &ws_id)?;
        w.scope = scope;
        w.buffers.clear();
        self.save_workspace(&w)?;
        out.workspace_id = ws_id;
        Ok(out)
    }
}

/// `name`, or `name (2)`, … when another workspace has it.
fn unique_name(store: &Store, name: &str, id: &str) -> Result<String> {
    let names: Vec<String> = store
        .workspaces()?
        .into_iter()
        .filter(|w| w.id != id)
        .map(|w| w.name.to_lowercase())
        .collect();
    let base = name.trim();
    let base = if base.is_empty() { "Imported" } else { base };
    let mut n = 1;
    let mut candidate = base.to_owned();
    while names.contains(&candidate.to_lowercase()) {
        n += 1;
        candidate = format!("{base} ({n})");
    }
    Ok(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Credential, DesktopProtocol, DesktopServer, Host};

    fn sample(s: &mut Store) -> (String, Host, DesktopServer, Credential) {
        let mut cred = Credential::new("ops", "ops");
        cred.secret = Some(SecretRef::for_profile(&cred.id, "password"));
        s.save_profile(&Profile::Credential(cred.clone())).unwrap();
        let mut h = Host::new("web-1", "10.0.0.5", "root");
        h.tags = vec!["role:web".into()];
        h.folder = Some("Prod".into());
        h.secret = Some(SecretRef::for_profile(&h.id, "passphrase"));
        s.save_profile(&Profile::Host(h.clone())).unwrap();
        let mut d = DesktopServer::new("dc-1", DesktopProtocol::Rdp);
        d.address = "10.0.0.9".into();
        d.credential = Some(cred.id.clone());
        s.save_profile(&Profile::Desktop(d.clone())).unwrap();
        let w = s
            .create_workspace(
                "Prod",
                WorkspaceScope {
                    folders: vec!["Prod".into()],
                    members: vec![d.id.clone()],
                    ..Default::default()
                },
            )
            .unwrap();
        (w.id, h, d, cred)
    }

    #[test]
    fn export_has_no_secrets_and_lists_what_needs_a_value() {
        let mut s = Store::open_in_memory().unwrap();
        let (id, h, d, cred) = sample(&mut s);
        let f = s.export_workspace(&id).unwrap();
        let json = f.to_json().unwrap();
        assert!(!json.contains(":passphrase") && !json.contains(":password"));
        assert!(!f.secrets.encrypted);
        let ids: Vec<&ProfileId> = f.profiles.iter().map(Profile::id).collect();
        assert!(ids.contains(&&h.id) && ids.contains(&&d.id));
        assert!(ids.contains(&&cred.id), "the referenced credential travels");
        assert!(
            f.secrets
                .entries
                .iter()
                .all(|e| e.status == SecretStatus::NeedsValue)
        );
        assert_eq!(f.secrets.entries.len(), 2);
        assert_eq!(f.tags, ["role:web"]);
        assert_eq!(f.folders, ["Prod"]);
        assert_eq!(WorkspaceFile::parse(&json).unwrap(), f);
        assert!(WorkspaceFile::parse(r#"{"format":"x","version":1,"workspace":{"id":"a","name":"a","scope":{}},"profiles":[]}"#).is_err());
    }

    #[test]
    fn secrets_round_trip_with_a_passphrase() {
        let mut s = Store::open_in_memory().unwrap();
        let (id, h, _, cred) = sample(&mut s);
        let mut f = s.export_workspace(&id).unwrap();
        let pass = SecretString::from("correct horse");
        f.seal_secrets(
            &pass,
            &[
                (h.id.clone(), SecretString::from("key-pass")),
                (cred.id.clone(), SecretString::from("hunter2")),
            ],
        )
        .unwrap();
        let json = f.to_json().unwrap();
        assert!(!json.contains("hunter2") && !json.contains("key-pass"));
        assert!(json.contains("\"encrypted\": true"));
        assert!(json.contains("web-1"), "only the values are encrypted");
        let back = WorkspaceFile::parse(&json).unwrap();
        let mut vals: Vec<(String, String)> = back
            .open_secrets(&pass)
            .unwrap()
            .into_iter()
            .map(|(i, v)| (i.0, v.expose_secret().to_owned()))
            .collect();
        vals.sort();
        let mut want = vec![
            (h.id.0.clone(), "key-pass".to_owned()),
            (cred.id.0.clone(), "hunter2".to_owned()),
        ];
        want.sort();
        assert_eq!(vals, want);
        assert!(matches!(
            back.open_secrets(&SecretString::from("wrong")),
            Err(StoreError::WrongPassphrase)
        ));
    }

    #[test]
    fn reimport_updates_instead_of_duplicating() {
        let mut src = Store::open_in_memory().unwrap();
        let (id, h, _, _) = sample(&mut src);
        let f = src.export_workspace(&id).unwrap();

        let mut dst = Store::open_in_memory().unwrap();
        let preview = dst.preview_workspace_import(&f).unwrap();
        assert!(!preview.workspace_exists);
        assert!(preview.items.iter().all(|i| i.existing.is_none()));
        let out = dst.apply_workspace_import(&f, &HashMap::new()).unwrap();
        assert_eq!(out.added, 3);
        let n = dst.profiles().unwrap().len();

        // The team edits the file; a local secret was typed in meanwhile.
        let mut local = match dst.profile(&h.id).unwrap() {
            Some(Profile::Host(h)) => h,
            _ => panic!(),
        };
        local.secret = Some(SecretRef::for_profile(&h.id, "passphrase"));
        dst.save_profile(&Profile::Host(local)).unwrap();
        let mut f2 = f.clone();
        if let Some(Profile::Host(x)) = f2.profiles.iter_mut().find(|p| p.id() == &h.id) {
            x.address = "10.0.0.50".into();
        }
        f2.workspace.name = "Production".into();
        let preview = dst.preview_workspace_import(&f2).unwrap();
        assert!(preview.workspace_exists);
        let item = preview.items.iter().find(|i| i.id == h.id).unwrap();
        assert_eq!(item.existing.as_deref(), Some("web-1"));
        assert!(!item.identical);
        let out = dst.apply_workspace_import(&f2, &HashMap::new()).unwrap();
        assert_eq!((out.updated, out.added), (3, 0));
        assert_eq!(dst.profiles().unwrap().len(), n, "no duplicates");
        match dst.profile(&h.id).unwrap() {
            Some(Profile::Host(x)) => {
                assert_eq!(x.address, "10.0.0.50");
                assert!(x.secret.is_some(), "the local secret stays");
            }
            _ => panic!(),
        }
        assert_eq!(dst.workspaces().unwrap().len(), 2);
        assert_eq!(
            dst.workspace_by_id(&id).unwrap().unwrap().name,
            "Production"
        );

        // Skip and keep both.
        let mut choices = HashMap::new();
        choices.insert(h.id.clone(), ImportChoice::KeepBoth);
        let out = dst.apply_workspace_import(&f2, &choices).unwrap();
        assert_eq!(out.added, 1);
        assert_eq!(dst.profiles().unwrap().len(), n + 1);
        let copy = &out.id_map[&h.id];
        assert_ne!(copy, &h.id);
        let ws = dst.workspace_by_id(&id).unwrap().unwrap();
        assert!(ws.scope.visible(&dst.profiles().unwrap()).contains(copy));
        choices.insert(h.id.clone(), ImportChoice::Skip);
        let out = dst.apply_workspace_import(&f2, &choices).unwrap();
        assert_eq!(out.skipped, 1);
    }
}
