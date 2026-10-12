//! Shared credentials: one username (optionally with a domain) and a password, or an SSH
//! key with its passphrase, referenced by many Hosts, remote desktops and database
//! connections. The secret lives in the keychain or vault under the credential's id; a
//! profile that references a credential stores no secret of its own, so rotating the
//! credential changes what every one of them signs in with.

use serde::{Deserialize, Serialize};

use crate::model::{DbConnection, EnvironmentLabel, Host, ProfileId, SecretRef, SshAuth};

/// What a credential holds besides the user name.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum CredentialKind {
    /// A password (in the keychain or vault).
    #[default]
    Password,
    /// An SSH private key read in place; its passphrase (if any) in the keychain or vault.
    SshKey {
        /// Path to the private key.
        key_path: String,
    },
}

impl CredentialKind {
    /// Label for selects.
    pub fn label(&self) -> &'static str {
        match self {
            CredentialKind::Password => "Password",
            CredentialKind::SshKey { .. } => "SSH key",
        }
    }
}

/// A shared credential profile.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Credential {
    /// Id.
    pub id: ProfileId,
    /// Display name (`ops-admin`, `CORP domain admin`).
    pub name: String,
    /// User name.
    pub username: String,
    /// Windows domain (`CORP`), for RDP and SQL Server Windows sign-in.
    #[serde(default)]
    pub domain: Option<String>,
    /// Password or SSH key.
    #[serde(default)]
    pub auth: CredentialKind,
    /// The password or key passphrase.
    #[serde(default)]
    pub secret: Option<SecretRef>,
    /// Free-form note (who owns it, when it rotates). Never a secret.
    #[serde(default)]
    pub note: Option<String>,
}

impl Credential {
    /// A new password credential.
    pub fn new(name: impl Into<String>, username: impl Into<String>) -> Self {
        Self {
            id: ProfileId::new(),
            name: name.into(),
            username: username.into(),
            domain: None,
            auth: CredentialKind::Password,
            secret: None,
            note: None,
        }
    }

    /// The domain, trimmed; `None` when blank.
    pub fn domain(&self) -> Option<&str> {
        self.domain
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
    }

    /// `DOMAIN\user`, or the user alone.
    pub fn qualified_user(&self) -> String {
        match self.domain() {
            Some(d) => format!("{d}\\{}", self.username.trim()),
            None => self.username.trim().to_owned(),
        }
    }

    /// The environment of a credential (it has none; used by generic code).
    pub fn environment(&self) -> EnvironmentLabel {
        EnvironmentLabel::Local
    }
}

impl Host {
    /// The Host as it signs in when it uses `cred`: the credential's user, its key (an
    /// SSH-key credential) or its password, and its stored secret. Agent and
    /// keyboard-interactive Hosts keep their method and only take the user name.
    pub fn with_credential(&self, cred: &Credential) -> Host {
        let mut h = self.clone();
        h.user = cred.username.trim().to_owned();
        match &cred.auth {
            CredentialKind::SshKey { key_path } => {
                h.auth = SshAuth::PublicKey {
                    key_path: key_path.clone(),
                };
                h.secret = cred.secret.clone();
            }
            CredentialKind::Password => {
                if matches!(h.auth, SshAuth::Password | SshAuth::PublicKey { .. }) {
                    h.auth = SshAuth::Password;
                }
                h.secret = cred.secret.clone();
            }
        }
        h
    }
}

impl DbConnection {
    /// The connection as it signs in when it uses `cred`: the credential's user
    /// (`DOMAIN\user` for SQL Server Windows sign-in) and its stored password.
    pub fn with_credential(&self, cred: &Credential) -> DbConnection {
        let mut d = self.clone();
        d.user = if d.auth == switchyard_db::DbAuthMethod::WindowsPassword {
            cred.qualified_user()
        } else {
            cred.username.trim().to_owned()
        };
        d.secret = cred.secret.clone();
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use switchyard_db::{DbAuthMethod, Engine};

    #[test]
    fn hosts_and_connections_take_the_credentials_login() {
        let mut cred = Credential::new("ops", "ops");
        cred.domain = Some(" CORP ".into());
        cred.secret = Some(SecretRef("cred1:password".into()));
        let mut h = Host::new("web", "10.0.0.1", "root");
        h.credential = Some(cred.id.clone());
        let eff = h.with_credential(&cred);
        assert_eq!(eff.user, "ops");
        assert_eq!(eff.auth, SshAuth::Password);
        assert_eq!(eff.secret, cred.secret);

        cred.auth = CredentialKind::SshKey {
            key_path: "~/.ssh/ops".into(),
        };
        let eff = h.with_credential(&cred);
        assert_eq!(
            eff.auth,
            SshAuth::PublicKey {
                key_path: "~/.ssh/ops".into()
            }
        );

        let mut agent = h.clone();
        agent.auth = SshAuth::Agent;
        cred.auth = CredentialKind::Password;
        assert_eq!(agent.with_credential(&cred).auth, SshAuth::Agent);

        let mut d = DbConnection::new("db", Engine::SqlServer);
        d.auth = DbAuthMethod::WindowsPassword;
        assert_eq!(d.with_credential(&cred).user, "CORP\\ops");
        d.auth = DbAuthMethod::Password;
        let eff = d.with_credential(&cred);
        assert_eq!(eff.user, "ops");
        assert_eq!(eff.secret, cred.secret);
    }
}
