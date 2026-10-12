//! Ansible inventory import: INI and YAML inventories into Hosts, with groups as tags.
//!
//! Read: host names, `ansible_host`, `ansible_port`, `ansible_user` and
//! `ansible_ssh_private_key_file` (and their old `ansible_ssh_*` spellings), set on a
//! host or on any of its groups (`[group:vars]`, YAML `vars:`), host values first, then
//! the nearest group, `all` last. Groups (direct and parents through `:children`) become
//! tags. Passwords (`ansible_password`, `ansible_become_pass`) are never read. Numeric
//! and letter ranges (`web[01:03]`) expand like Ansible does.

use std::collections::{BTreeMap, HashMap, HashSet};

use switchyard_store::{Host, ProfileId, SshAuth};

use crate::ssh_import::SshImportCandidate;

/// Why an inventory could not be read.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InventoryError {
    /// YAML that does not parse, or is not an inventory.
    #[error("YAML inventory: {0}")]
    Yaml(String),
    /// An INI line that is not a host, a variable or a group header.
    #[error("line {line}: {message}")]
    Ini {
        /// 1-based line.
        line: usize,
        /// What is wrong.
        message: String,
    },
}

/// One inventory host with what Switchyard uses of it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InventoryHost {
    /// Inventory name (becomes the Host's name).
    pub name: String,
    /// `ansible_host` (address); the name when unset.
    pub address: Option<String>,
    /// `ansible_port`.
    pub port: Option<u16>,
    /// `ansible_user`.
    pub user: Option<String>,
    /// `ansible_ssh_private_key_file`.
    pub key_file: Option<String>,
    /// Groups it belongs to, directly or through `children` (not `all` / `ungrouped`).
    pub groups: Vec<String>,
}

/// Variables, by name.
type Vars = HashMap<String, String>;

/// An inventory as read: groups with hosts, children and vars; host vars.
#[derive(Default)]
struct Inventory {
    /// Host names in file order.
    hosts: Vec<String>,
    host_vars: HashMap<String, Vars>,
    /// Group → hosts.
    members: BTreeMap<String, Vec<String>>,
    /// Group → child groups.
    children: BTreeMap<String, Vec<String>>,
    group_vars: HashMap<String, Vars>,
}

impl Inventory {
    fn add_host(&mut self, name: &str, group: Option<&str>, vars: Vars) {
        if !self.hosts.iter().any(|h| h == name) {
            self.hosts.push(name.to_owned());
        }
        self.host_vars
            .entry(name.to_owned())
            .or_default()
            .extend(vars);
        if let Some(g) = group {
            let list = self.members.entry(g.to_owned()).or_default();
            if !list.iter().any(|h| h == name) {
                list.push(name.to_owned());
            }
        }
    }

    /// Groups holding `host`, nearest first (direct, then parents), each once.
    fn groups_of(&self, host: &str) -> Vec<String> {
        let mut out: Vec<String> = self
            .members
            .iter()
            .filter(|(_, hs)| hs.iter().any(|h| h == host))
            .map(|(g, _)| g.clone())
            .collect();
        let mut i = 0;
        while i < out.len() {
            let g = out[i].clone();
            for (parent, kids) in &self.children {
                if kids.contains(&g) && !out.contains(parent) {
                    out.push(parent.clone());
                }
            }
            i += 1;
        }
        out
    }

    fn resolve(&self) -> Vec<InventoryHost> {
        self.hosts
            .iter()
            .map(|name| {
                let groups = self.groups_of(name);
                let var = |keys: &[&str]| -> Option<String> {
                    let scopes = std::iter::once(self.host_vars.get(name))
                        .chain(groups.iter().map(|g| self.group_vars.get(g)))
                        .chain(std::iter::once(self.group_vars.get("all")));
                    for vars in scopes.flatten() {
                        if let Some(v) = keys.iter().find_map(|k| vars.get(*k)) {
                            return Some(v.clone()).filter(|v| !v.is_empty());
                        }
                    }
                    None
                };
                InventoryHost {
                    name: name.clone(),
                    address: var(&["ansible_host", "ansible_ssh_host"]),
                    port: var(&["ansible_port", "ansible_ssh_port"]).and_then(|p| p.parse().ok()),
                    user: var(&["ansible_user", "ansible_ssh_user"]),
                    key_file: var(&["ansible_ssh_private_key_file", "ansible_private_key_file"]),
                    groups: groups
                        .into_iter()
                        .filter(|g| g != "all" && g != "ungrouped")
                        .collect(),
                }
            })
            .collect()
    }
}

/// Parse an inventory file, YAML or INI (YAML when it parses as a YAML mapping).
pub fn parse_inventory(text: &str) -> Result<Vec<InventoryHost>, InventoryError> {
    let first = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with(';'))
        .unwrap_or("");
    if first == "---" || (first.ends_with(':') && !first.starts_with('[')) {
        parse_yaml(text)
    } else {
        parse_ini(text)
    }
}

/// `web[01:03].example.com` → `web01…web03`; `db-[a:c]` → `db-a…db-c`. Other names
/// pass through.
pub fn expand_range(name: &str) -> Vec<String> {
    let (Some(open), Some(close)) = (name.find('['), name.find(']')) else {
        return vec![name.to_owned()];
    };
    if close < open {
        return vec![name.to_owned()];
    }
    let (head, inner, tail) = (&name[..open], &name[open + 1..close], &name[close + 1..]);
    let mut parts = inner.split(':');
    let (Some(a), Some(b)) = (parts.next(), parts.next()) else {
        return vec![name.to_owned()];
    };
    let step: usize = parts
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1)
        .max(1);
    let mut out = Vec::new();
    if let (Ok(x), Ok(y)) = (a.parse::<u64>(), b.parse::<u64>()) {
        let width = if a.starts_with('0') { a.len() } else { 0 };
        for n in (x..=y.min(x + 10_000)).step_by(step) {
            for rest in expand_range(tail) {
                out.push(format!("{head}{n:0width$}{rest}"));
            }
        }
    } else if let (Some(x), Some(y)) = (single_char(a), single_char(b)) {
        for c in (x..=y).step_by(step) {
            for rest in expand_range(tail) {
                out.push(format!("{head}{c}{rest}"));
            }
        }
    } else {
        return vec![name.to_owned()];
    }
    out
}

fn single_char(s: &str) -> Option<char> {
    let mut it = s.chars();
    let c = it.next()?;
    (it.next().is_none() && c.is_ascii_alphabetic()).then_some(c)
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    for q in ['"', '\''] {
        if v.len() >= 2 && v.starts_with(q) && v.ends_with(q) {
            return v[1..v.len() - 1].to_owned();
        }
    }
    v.to_owned()
}

/// `key=value key2="v w"` pairs after a host name.
fn ini_vars(text: &str) -> Vars {
    let mut out = Vars::new();
    let mut rest = text.trim();
    while !rest.is_empty() {
        let Some(eq) = rest.find('=') else { break };
        let key = rest[..eq].trim().to_owned();
        let after = &rest[eq + 1..];
        let (value, next) = match after.chars().next() {
            Some(q @ ('"' | '\'')) => match after[1..].find(q) {
                Some(end) => (after[1..end + 1].to_owned(), &after[end + 2..]),
                None => (after[1..].to_owned(), ""),
            },
            _ => match after.find(char::is_whitespace) {
                Some(end) => (after[..end].to_owned(), &after[end..]),
                None => (after.to_owned(), ""),
            },
        };
        if !key.is_empty() && !key.contains(char::is_whitespace) {
            out.insert(key, value);
        }
        rest = next.trim_start();
    }
    out
}

/// An INI inventory.
pub fn parse_ini(text: &str) -> Result<Vec<InventoryHost>, InventoryError> {
    enum Section {
        Hosts(String),
        Vars(String),
        Children(String),
    }
    let mut inv = Inventory::default();
    let mut section = Section::Hosts("ungrouped".into());
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(head) = line.strip_prefix('[') {
            let Some(head) = head.strip_suffix(']') else {
                return Err(InventoryError::Ini {
                    line: i + 1,
                    message: "a group header needs a closing `]`".into(),
                });
            };
            section = match head.split_once(':') {
                Some((g, "vars")) => Section::Vars(g.trim().to_owned()),
                Some((g, "children")) => Section::Children(g.trim().to_owned()),
                Some((_, other)) => {
                    return Err(InventoryError::Ini {
                        line: i + 1,
                        message: format!("unknown section kind `:{other}`"),
                    });
                }
                None => {
                    let g = head.trim().to_owned();
                    inv.members.entry(g.clone()).or_default();
                    Section::Hosts(g)
                }
            };
            continue;
        }
        match &section {
            Section::Hosts(group) => {
                let (name, vars) = match line.split_once(char::is_whitespace) {
                    Some((n, rest)) => (n, ini_vars(rest)),
                    None => (line, Vars::new()),
                };
                for host in expand_range(name) {
                    inv.add_host(&host, Some(group), vars.clone());
                }
            }
            Section::Vars(group) => {
                let Some((k, v)) = line.split_once('=') else {
                    return Err(InventoryError::Ini {
                        line: i + 1,
                        message: "expected `name=value`".into(),
                    });
                };
                inv.group_vars
                    .entry(group.clone())
                    .or_default()
                    .insert(k.trim().to_owned(), unquote(v));
            }
            Section::Children(group) => {
                let child = line.split_whitespace().next().unwrap_or(line).to_owned();
                let kids = inv.children.entry(group.clone()).or_default();
                if !kids.contains(&child) {
                    kids.push(child);
                }
            }
        }
    }
    Ok(inv.resolve())
}

fn yaml_string(v: &serde_yaml::Value) -> Option<String> {
    match v {
        serde_yaml::Value::String(s) => Some(s.clone()),
        serde_yaml::Value::Number(n) => Some(n.to_string()),
        serde_yaml::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn yaml_vars(v: Option<&serde_yaml::Value>) -> Vars {
    let mut out = Vars::new();
    if let Some(serde_yaml::Value::Mapping(m)) = v {
        for (k, v) in m {
            if let (Some(k), Some(v)) = (yaml_string(k), yaml_string(v)) {
                out.insert(k, v);
            }
        }
    }
    out
}

fn yaml_group(inv: &mut Inventory, name: &str, body: &serde_yaml::Value, depth: usize) {
    if depth > 32 {
        return;
    }
    inv.members.entry(name.to_owned()).or_default();
    let serde_yaml::Value::Mapping(m) = body else {
        return;
    };
    let vars = yaml_vars(m.get("vars"));
    if !vars.is_empty() {
        inv.group_vars
            .entry(name.to_owned())
            .or_default()
            .extend(vars);
    }
    if let Some(serde_yaml::Value::Mapping(hosts)) = m.get("hosts") {
        for (h, hv) in hosts {
            let Some(h) = yaml_string(h) else { continue };
            let vars = yaml_vars(Some(hv));
            for host in expand_range(&h) {
                inv.add_host(&host, Some(name), vars.clone());
            }
        }
    }
    if let Some(serde_yaml::Value::Mapping(kids)) = m.get("children") {
        for (c, cv) in kids {
            let Some(c) = yaml_string(c) else { continue };
            let list = inv.children.entry(name.to_owned()).or_default();
            if !list.contains(&c) {
                list.push(c.clone());
            }
            yaml_group(inv, &c, cv, depth + 1);
        }
    }
}

/// A YAML inventory (`all:` with `hosts`, `vars` and `children`; other top-level groups
/// too).
pub fn parse_yaml(text: &str) -> Result<Vec<InventoryHost>, InventoryError> {
    let doc: serde_yaml::Value =
        serde_yaml::from_str(text).map_err(|e| InventoryError::Yaml(e.to_string()))?;
    let serde_yaml::Value::Mapping(top) = doc else {
        return Err(InventoryError::Yaml(
            "expected groups at the top level (`all:`)".into(),
        ));
    };
    let mut inv = Inventory::default();
    for (g, body) in &top {
        if let Some(g) = yaml_string(g) {
            yaml_group(&mut inv, &g, body, 0);
        }
    }
    Ok(inv.resolve())
}

/// The Host an inventory entry becomes. Without a key file it signs in with the SSH
/// agent (as Ansible would through OpenSSH).
pub fn host_from_inventory(h: &InventoryHost, default_user: &str) -> Host {
    let mut host = Host::new(
        h.name.clone(),
        h.address.clone().unwrap_or_else(|| h.name.clone()),
        h.user.clone().unwrap_or_else(|| default_user.to_owned()),
    );
    host.port = h.port.unwrap_or(22);
    host.auth = match &h.key_file {
        Some(k) => SshAuth::PublicKey {
            key_path: k.clone(),
        },
        None => SshAuth::Agent,
    };
    host.tags = switchyard_store::tags::normalize_tags(&h.groups);
    host
}

/// The import preview rows, with saved names marked.
pub fn candidates(
    hosts: &[InventoryHost],
    saved: &HashSet<String>,
    default_user: &str,
) -> Vec<SshImportCandidate> {
    hosts
        .iter()
        .map(|e| {
            let h = host_from_inventory(e, default_user);
            let port = if h.port == 22 {
                String::new()
            } else {
                format!(":{}", h.port)
            };
            SshImportCandidate {
                alias: e.name.clone(),
                address: format!("{}@{}{port}", h.user, h.address),
                auth: match &e.key_file {
                    Some(k) => format!("key {k}"),
                    None => "agent".into(),
                },
                via: Vec::new(),
                exists: saved.contains(&e.name),
                tags: h.tags,
            }
        })
        .collect()
}

/// The Hosts to save: entries not saved yet (only those in `only`, when given).
pub fn plan(
    hosts: &[InventoryHost],
    saved: &HashMap<String, ProfileId>,
    only: Option<&HashSet<String>>,
    default_user: &str,
) -> Vec<Host> {
    let mut seen = HashSet::new();
    hosts
        .iter()
        .filter(|e| !saved.contains_key(&e.name) && only.is_none_or(|o| o.contains(&e.name)))
        .filter(|e| seen.insert(e.name.clone()))
        .map(|e| host_from_inventory(e, default_user))
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const INI: &str = "\
# production inventory
bastion.example.com ansible_user=admin

[web]
web[01:03].example.com
web-canary ansible_host=10.0.0.9 ansible_port=2222 ansible_user=\"deploy user\"

[db]
db1 ansible_host=10.0.1.1 ansible_ssh_private_key_file=~/.ssh/db_key

[web:vars]
ansible_user=www

[prod:children]
web
db

[prod:vars]
ansible_port=2200

[all:vars]
ansible_user=ops
ansible_password=never-read
";

    fn by_name<'a>(hosts: &'a [InventoryHost], name: &str) -> &'a InventoryHost {
        hosts.iter().find(|h| h.name == name).unwrap()
    }

    #[test]
    fn ini_hosts_groups_and_vars() {
        let hosts = parse_ini(INI).unwrap();
        let names: Vec<&str> = hosts.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "bastion.example.com",
                "web01.example.com",
                "web02.example.com",
                "web03.example.com",
                "web-canary",
                "db1"
            ]
        );
        let b = by_name(&hosts, "bastion.example.com");
        assert_eq!(b.user.as_deref(), Some("admin"));
        assert!(b.groups.is_empty());
        assert_eq!(b.port, None);
        let w = by_name(&hosts, "web02.example.com");
        // Nearest group wins: web's user, prod's port.
        assert_eq!(w.user.as_deref(), Some("www"));
        assert_eq!(w.port, Some(2200));
        assert_eq!(w.groups, ["web", "prod"]);
        let c = by_name(&hosts, "web-canary");
        assert_eq!(c.address.as_deref(), Some("10.0.0.9"));
        assert_eq!(c.port, Some(2222));
        assert_eq!(c.user.as_deref(), Some("deploy user"));
        let d = by_name(&hosts, "db1");
        assert_eq!(d.key_file.as_deref(), Some("~/.ssh/db_key"));
        assert_eq!(d.user.as_deref(), Some("ops"));
        assert_eq!(d.groups, ["db", "prod"]);
        assert!(parse_inventory(INI).unwrap() == hosts);
    }

    #[test]
    fn ini_errors_name_the_line() {
        let e = parse_ini("[web\nhost").unwrap_err();
        assert_eq!(
            e,
            InventoryError::Ini {
                line: 1,
                message: "a group header needs a closing `]`".into()
            }
        );
        assert!(parse_ini("[web:vars]\njust words").is_err());
    }

    const YAML: &str = "\
all:
  vars:
    ansible_user: ops
  hosts:
    bastion:
      ansible_host: 203.0.113.10
  children:
    web:
      hosts:
        web-[a:b].example.com:
        web-canary:
          ansible_port: 2222
      vars:
        ansible_user: www
    prod:
      children:
        web:
        db:
          hosts:
            db1:
              ansible_host: 10.0.1.1
              ansible_ssh_private_key_file: ~/.ssh/db_key
";

    #[test]
    fn yaml_hosts_groups_and_vars() {
        let hosts = parse_inventory(YAML).unwrap();
        let names: Vec<&str> = hosts.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "bastion",
                "web-a.example.com",
                "web-b.example.com",
                "web-canary",
                "db1"
            ]
        );
        let b = by_name(&hosts, "bastion");
        assert_eq!(b.address.as_deref(), Some("203.0.113.10"));
        assert_eq!(b.user.as_deref(), Some("ops"));
        assert!(b.groups.is_empty());
        let c = by_name(&hosts, "web-canary");
        assert_eq!(c.port, Some(2222));
        assert_eq!(c.user.as_deref(), Some("www"));
        assert_eq!(c.groups, ["web", "prod"]);
        let d = by_name(&hosts, "db1");
        assert_eq!(d.groups, ["db", "prod"]);
        assert_eq!(d.key_file.as_deref(), Some("~/.ssh/db_key"));
        assert!(parse_yaml("- a\n- b\n").is_err());
        assert!(parse_yaml("all: [unclosed").is_err());
    }

    #[test]
    fn ranges_expand() {
        assert_eq!(expand_range("web[1:3]"), ["web1", "web2", "web3"]);
        assert_eq!(expand_range("n[08:10].x"), ["n08.x", "n09.x", "n10.x"]);
        assert_eq!(expand_range("h[0:4:2]"), ["h0", "h2", "h4"]);
        assert_eq!(expand_range("db-[a:c]"), ["db-a", "db-b", "db-c"]);
        assert_eq!(expand_range("plain"), ["plain"]);
        assert_eq!(expand_range("odd[x]"), ["odd[x]"]);
    }

    #[test]
    fn hosts_and_preview_from_inventory() {
        let hosts = parse_ini(INI).unwrap();
        let saved: HashSet<String> = ["db1".to_owned()].into();
        let rows = candidates(&hosts, &saved, "me");
        let canary = rows.iter().find(|r| r.alias == "web-canary").unwrap();
        assert_eq!(canary.address, "deploy user@10.0.0.9:2222");
        assert_eq!(canary.tags, ["web", "prod"]);
        assert_eq!(canary.auth, "agent");
        assert!(rows.iter().find(|r| r.alias == "db1").unwrap().exists);
        let saved: HashMap<String, ProfileId> = [("db1".to_owned(), ProfileId::new())].into();
        let only: HashSet<String> = ["web-canary".to_owned(), "db1".to_owned()].into();
        let new = plan(&hosts, &saved, Some(&only), "me");
        assert_eq!(new.len(), 1);
        assert_eq!(new[0].name, "web-canary");
        assert_eq!(new[0].port, 2222);
        assert_eq!(new[0].tags, ["web", "prod"]);
        let all = plan(&hosts, &HashMap::new(), None, "me");
        assert_eq!(all.len(), 6);
        let db = all.iter().find(|h| h.name == "db1").unwrap();
        assert_eq!(
            db.auth,
            SshAuth::PublicKey {
                key_path: "~/.ssh/db_key".into()
            }
        );
    }
}
