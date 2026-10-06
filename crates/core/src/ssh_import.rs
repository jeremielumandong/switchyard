//! Turning `~/.ssh/config` entries into Hosts: what an import would add (for the preview)
//! and the Hosts it saves, with ProxyJump wired to Host ids.

use std::collections::{HashMap, HashSet};

use switchyard_remote::ssh_config::SshConfigHost;
use switchyard_store::{Host, ProfileId, SshAuth};

/// One `Host` entry as the import preview shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshImportCandidate {
    /// The alias, which becomes the Host's name.
    pub alias: String,
    /// `user@hostname:port`.
    pub address: String,
    /// How it will sign in: `password`, `key ~/.ssh/id_ed25519`, `agent`, …
    pub auth: String,
    /// ProxyJump chain (aliases), outermost first.
    pub via: Vec<String>,
    /// A Host with this name is already saved (it is skipped).
    pub exists: bool,
}

/// The Host an entry becomes (without jump hosts).
pub(crate) fn host_from_entry(h: &SshConfigHost, default_user: &str) -> Host {
    let mut host = Host::new(
        h.alias.clone(),
        h.hostname.clone(),
        h.user.clone().unwrap_or_else(|| default_user.to_owned()),
    );
    host.port = h.port.unwrap_or(22);
    let agent = h
        .identity_agent
        .clone()
        .filter(|a| !a.eq_ignore_ascii_case("none"));
    match (&agent, &h.identity_file) {
        // An agent (1Password, …): the IdentityFile, a `.pub`, picks its key.
        (Some(a), key) => {
            host.auth = SshAuth::Agent;
            host.identity_agent = Some(a.clone());
            host.agent_key = key.clone().map(|k| {
                if k.ends_with(".pub") {
                    k
                } else {
                    format!("{k}.pub")
                }
            });
        }
        (None, Some(key)) if key.ends_with(".pub") => {
            host.auth = SshAuth::Agent;
            host.agent_key = Some(key.clone());
        }
        (None, Some(key)) => {
            host.auth = SshAuth::PublicKey {
                key_path: key.clone(),
            };
        }
        (None, None) => {}
    }
    host
}

fn auth_label(h: &Host) -> String {
    match &h.auth {
        SshAuth::Password => "password".into(),
        SshAuth::PublicKey { key_path } => format!("key {key_path}"),
        SshAuth::Agent => match (&h.identity_agent, &h.agent_key) {
            (Some(a), _) if a.contains("1password") => "1Password agent".into(),
            (Some(_), _) => "agent (IdentityAgent)".into(),
            (None, Some(k)) => format!("agent, key {k}"),
            (None, None) => "agent".into(),
        },
        SshAuth::KeyboardInteractive => "keyboard-interactive".into(),
    }
}

/// What importing `parsed` would do, given the names of saved Hosts.
pub(crate) fn candidates(
    parsed: &[SshConfigHost],
    saved: &HashSet<String>,
    default_user: &str,
) -> Vec<SshImportCandidate> {
    parsed
        .iter()
        .map(|e| {
            let h = host_from_entry(e, default_user);
            let port = if h.port == 22 {
                String::new()
            } else {
                format!(":{}", h.port)
            };
            SshImportCandidate {
                alias: e.alias.clone(),
                address: format!("{}@{}{port}", h.user, h.address),
                auth: auth_label(&h),
                via: e.proxy_jump.clone(),
                exists: saved.contains(&e.alias),
            }
        })
        .collect()
}

/// The Hosts to save: new entries (only those in `only`, when given), each once, with
/// ProxyJump pointing at saved or newly imported Hosts by id.
pub(crate) fn plan(
    parsed: &[SshConfigHost],
    saved: &HashMap<String, ProfileId>,
    only: Option<&HashSet<String>>,
    default_user: &str,
) -> Vec<Host> {
    let mut by_alias = saved.clone();
    let mut created: Vec<(Host, Vec<String>)> = Vec::new();
    for e in parsed {
        if by_alias.contains_key(&e.alias) || only.is_some_and(|o| !o.contains(&e.alias)) {
            continue;
        }
        let host = host_from_entry(e, default_user);
        by_alias.insert(e.alias.clone(), host.id.clone());
        created.push((host, e.proxy_jump.clone()));
    }
    created
        .into_iter()
        .map(|(mut host, jumps)| {
            host.jump_hosts = jumps
                .iter()
                .filter_map(|j| by_alias.get(j).cloned())
                .collect();
            host
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    const CONFIG: &str = "
Host bastion
  HostName bastion.corp.example
  User ops
Host db
  HostName 10.0.0.5
  Port 2222
  ProxyJump bastion
  IdentityFile ~/.ssh/id_ed25519
Host vault
  HostName vault.corp.example
  IdentityAgent ~/.1password/agent.sock
Host old
  HostName old.example
";

    fn parsed() -> Vec<SshConfigHost> {
        switchyard_remote::parse_ssh_config(CONFIG)
    }

    #[test]
    fn preview_lists_every_entry_and_marks_saved_ones() {
        let saved: HashSet<String> = ["old".to_owned()].into();
        let c = candidates(&parsed(), &saved, "me");
        let rows: Vec<(&str, &str, &str, bool)> = c
            .iter()
            .map(|c| {
                (
                    c.alias.as_str(),
                    c.address.as_str(),
                    c.auth.as_str(),
                    c.exists,
                )
            })
            .collect();
        assert_eq!(
            rows,
            [
                ("bastion", "ops@bastion.corp.example", "password", false),
                ("db", "me@10.0.0.5:2222", "key ~/.ssh/id_ed25519", false),
                ("vault", "me@vault.corp.example", "1Password agent", false),
                ("old", "me@old.example", "password", true),
            ]
        );
        assert_eq!(c[1].via, ["bastion"]);
    }

    #[test]
    fn import_only_the_chosen_and_wire_jumps() {
        let old = ProfileId::new();
        let saved: HashMap<String, ProfileId> = [("old".to_owned(), old)].into();
        // Everything new.
        let all = plan(&parsed(), &saved, None, "me");
        let names: Vec<&str> = all.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, ["bastion", "db", "vault"]);
        assert_eq!(
            all[1].jump_hosts,
            [all[0].id.clone()],
            "db jumps via the new bastion"
        );
        // Only db: its jump host isn't imported, so no jump is wired.
        let only: HashSet<String> = ["db".to_owned()].into();
        let one = plan(&parsed(), &saved, Some(&only), "me");
        assert_eq!(one.len(), 1);
        assert!(one[0].jump_hosts.is_empty());
        // A saved bastion is used.
        let bastion = ProfileId::new();
        let saved2: HashMap<String, ProfileId> = [("bastion".to_owned(), bastion.clone())].into();
        let one = plan(&parsed(), &saved2, Some(&only), "me");
        assert_eq!(one[0].jump_hosts, [bastion]);
    }
}
