//! Rows of the Explorer (the sidebar's connections tree, design v3): every saved source
//! grouped by the place it lives (pinned, servers, cloud accounts, direct) or by type
//! (databases, terminals, files and storage, config and secrets). A filter always lists
//! by type, so a match shows with the place it belongs to.
//!
//! The layout is pure (profiles, collapsed keys and live connections in, rows out) so it
//! is unit tested; [`crate::sidebar`] draws the rows.

use std::collections::HashSet;

use gpui_kit::SharedString;
use switchyard_core::db::Engine;
use switchyard_core::store::{
    CloudProvider, CloudService, DbConnection, EnvironmentLabel, FileProtocol, Host, Profile,
    ProfileId,
};

use crate::actions::fuzzy_score;
use crate::app_state::{Profiles, badge_of};

/// How the Explorer groups its sources.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExplorerGroup {
    /// By the place a source lives: servers, cloud accounts, direct connections.
    #[default]
    Place,
    /// By what it is: databases, terminals, files and storage, config and secrets.
    Type,
}

impl ExplorerGroup {
    /// The value saved in settings.
    pub fn key(self) -> &'static str {
        match self {
            ExplorerGroup::Place => "place",
            ExplorerGroup::Type => "type",
        }
    }

    /// Parse a saved value; anything unknown is [`ExplorerGroup::Place`].
    pub fn from_key(s: &str) -> Self {
        if s == "type" {
            ExplorerGroup::Type
        } else {
            ExplorerGroup::Place
        }
    }
}

/// What a row is drawn as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    /// A section heading (`SERVERS · SSH`): small caps, toggles its section.
    Head,
    /// A server, folder, cloud account or service group: toggles its children.
    Group,
    /// Something that opens.
    Leaf,
}

/// What a click does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnAction {
    /// Expand or collapse the row's children.
    Toggle,
    /// Open a database connection.
    Open,
    /// Open a terminal on a Host (`None`: the local shell).
    Terminal(Option<ProfileId>),
    /// Open a file connection (`None`: local files).
    Files,
    /// Open any other saved profile (cloud services).
    Profile,
}

/// One Explorer row.
#[derive(Clone, Debug)]
pub struct ConnRow {
    /// How it is drawn.
    pub kind: RowKind,
    /// Collapse key (and a stable identity).
    pub key: String,
    /// Nesting below its section (0: top level).
    pub depth: u8,
    /// Monogram for leaves (`PG`, `S3`).
    pub badge: &'static str,
    /// Label.
    pub label: SharedString,
    /// Muted text on the right (address, region, count, place).
    pub sub: SharedString,
    /// Environment dot.
    pub env: Option<EnvironmentLabel>,
    /// A session is open on it (or on one of its children).
    pub live: bool,
    /// The saved profile behind it.
    pub profile: Option<ProfileId>,
    /// Click.
    pub action: ConnAction,
    /// Siblings it can be dragged among (`hosts`, `h:<host id>`, `g:direct`, …).
    pub drag_group: Option<String>,
    /// A session folder's name (its group row; right click edits its Hosts).
    pub folder: Option<String>,
}

impl ConnRow {
    fn head(key: &str, label: &str, sub: String, collapsed: bool) -> Self {
        ConnRow {
            kind: RowKind::Head,
            key: key.to_owned(),
            depth: 0,
            badge: "",
            label: label.to_owned().into(),
            sub: if collapsed { "show".into() } else { sub.into() },
            env: None,
            live: false,
            profile: None,
            action: ConnAction::Toggle,
            drag_group: None,
            folder: None,
        }
    }

    fn group(key: String, depth: u8, label: String, sub: String) -> Self {
        ConnRow {
            kind: RowKind::Group,
            key,
            depth,
            badge: "",
            label: label.into(),
            sub: sub.into(),
            env: None,
            live: false,
            profile: None,
            action: ConnAction::Toggle,
            drag_group: None,
            folder: None,
        }
    }
}

/// Where a source lives, for its sub text when listed by type.
fn place_of(p: &Profile, profiles: &Profiles) -> String {
    let host = |id: &ProfileId| profiles.host(id).map(|h| h.name.clone());
    match p {
        Profile::Db(d) => match (&d.via_host, cloud_db_service(d)) {
            (Some(h), _) => host(h).unwrap_or_else(|| "server".into()),
            (None, Some(_)) => account_of_db(d).1,
            (None, None) => format!("direct · {}", d.engine.display_name()),
        },
        Profile::File(f) => match &f.protocol {
            FileProtocol::Sftp { host_id } => host(host_id).unwrap_or_else(|| "server".into()),
            FileProtocol::Ftp { .. } => "direct · FTP".into(),
        },
        Profile::Terminal(t) => match &t.host_id {
            Some(h) => host(h).unwrap_or_else(|| "server".into()),
            None => "direct".into(),
        },
        Profile::Host(h) => h.address.clone(),
        Profile::Cloud(c) => account_of_cloud(c.service, c.folder.as_deref(), &c.endpoint).1,
    }
}

/// The Cloudflare database services that live under a Cloudflare account.
fn cloud_db_service(d: &DbConnection) -> Option<&'static str> {
    match d.engine {
        Engine::D1 => Some("D1 databases"),
        Engine::DurableObject => Some("Durable Objects"),
        _ => None,
    }
}

/// The cloud account a service belongs to: (grouping key, label). There is no saved
/// account entity: services of one provider share an account unless their folder names
/// another one, and S3 against a custom endpoint is its own "S3-compatible" account.
fn account_of_cloud(
    service: CloudService,
    folder: Option<&str>,
    endpoint: &str,
) -> (String, String) {
    let provider = service.provider();
    let custom_s3 = service == CloudService::S3 && !endpoint.trim().is_empty();
    let base = if custom_s3 {
        "S3-compatible"
    } else {
        provider.display_name()
    };
    match folder.map(str::trim).filter(|f| !f.is_empty()) {
        Some(f) => (format!("a:{base}:{f}"), format!("{base} · {f}")),
        None => (format!("a:{base}"), base.to_owned()),
    }
}

fn account_of_db(d: &DbConnection) -> (String, String) {
    let base = CloudProvider::Cloudflare.display_name();
    match d.folder.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
        Some(f) => (format!("a:{base}:{f}"), format!("{base} · {f}")),
        None => (format!("a:{base}"), base.to_owned()),
    }
}

/// The service group a cloud service lists under inside its account.
fn service_group(s: CloudService) -> &'static str {
    match s {
        CloudService::S3 => "S3 buckets",
        CloudService::R2 => "R2 buckets",
        CloudService::AzureBlob => "Storage accounts",
        CloudService::AppConfig => "App Configuration",
        CloudService::KeyVault => "Key Vaults",
        CloudService::SecretsManager => "Secrets Manager",
        CloudService::ParameterStore => "Parameter Store",
        CloudService::WorkersKv => "Workers KV",
    }
}

/// Which type section a profile lists under.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TypeSec {
    Databases,
    Terminals,
    Files,
    Config,
}

const TYPE_SECTIONS: [(TypeSec, &str, &str); 4] = [
    (TypeSec::Databases, "t:db", "DATABASES"),
    (TypeSec::Terminals, "t:shell", "TERMINALS"),
    (TypeSec::Files, "t:files", "FILES & STORAGE"),
    (TypeSec::Config, "t:config", "CONFIG & SECRETS"),
];

fn type_of(p: &Profile) -> TypeSec {
    match p {
        Profile::Db(_) => TypeSec::Databases,
        Profile::Host(_) | Profile::Terminal(_) => TypeSec::Terminals,
        Profile::File(_) => TypeSec::Files,
        Profile::Cloud(c) => match c.service {
            CloudService::S3 | CloudService::R2 | CloudService::AzureBlob => TypeSec::Files,
            _ => TypeSec::Config,
        },
    }
}

/// The Explorer's rows.
pub fn explorer_rows(
    profiles: &Profiles,
    collapsed: &HashSet<String>,
    live: &HashSet<ProfileId>,
    group: ExplorerGroup,
    filter: &str,
) -> Vec<ConnRow> {
    let filter = filter.trim();
    if filter.is_empty() && group == ExplorerGroup::Place {
        place_rows(profiles, collapsed, live)
    } else {
        type_rows(profiles, collapsed, live, filter)
    }
}

/// A leaf for a saved profile.
fn leaf(p: &Profile, depth: u8, drag_group: Option<&str>, live: &HashSet<ProfileId>) -> ConnRow {
    let (sub, action) = match p {
        Profile::Db(d) => (
            if d.via_host.is_some() || cloud_db_service(d).is_some() {
                String::new()
            } else if d.engine.is_local_file() {
                std::path::Path::new(d.database.trim())
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_default()
            } else {
                format!(":{}", d.port)
            },
            ConnAction::Open,
        ),
        Profile::File(f) => (
            match &f.protocol {
                FileProtocol::Sftp { .. } => String::new(),
                FileProtocol::Ftp { .. } => "FTP".into(),
            },
            ConnAction::Files,
        ),
        Profile::Cloud(c) => (c.region.trim().to_owned(), ConnAction::Profile),
        Profile::Terminal(t) => (String::new(), ConnAction::Terminal(t.host_id.clone())),
        Profile::Host(h) => (h.address.clone(), ConnAction::Terminal(Some(h.id.clone()))),
    };
    ConnRow {
        kind: RowKind::Leaf,
        key: p.id().0.clone(),
        depth,
        badge: badge_of(p),
        label: p.name().to_owned().into(),
        sub: sub.into(),
        env: None,
        live: live.contains(p.id()),
        profile: Some(p.id().clone()),
        action,
        drag_group: drag_group.map(str::to_owned),
        folder: None,
    }
}

fn local_shell(depth: u8) -> ConnRow {
    ConnRow {
        kind: RowKind::Leaf,
        key: "local-shell".into(),
        depth,
        badge: "SH",
        label: "Local shell".into(),
        sub: "".into(),
        env: None,
        live: false,
        profile: None,
        action: ConnAction::Terminal(None),
        drag_group: None,
        folder: None,
    }
}

fn local_files(depth: u8) -> ConnRow {
    ConnRow {
        kind: RowKind::Leaf,
        key: "local-files".into(),
        depth,
        badge: "FS",
        label: "Local files".into(),
        sub: "".into(),
        env: None,
        live: false,
        profile: None,
        action: ConnAction::Files,
        drag_group: None,
        folder: None,
    }
}

/// A Host's terminal leaf.
fn host_terminal(h: &Host, depth: u8) -> ConnRow {
    ConnRow {
        kind: RowKind::Leaf,
        key: format!("h:{}:term", h.id),
        depth,
        badge: "SSH",
        label: "Terminal".into(),
        sub: "".into(),
        env: None,
        live: false,
        profile: Some(h.id.clone()),
        action: ConnAction::Terminal(Some(h.id.clone())),
        drag_group: None,
        folder: None,
    }
}

fn place_rows(
    profiles: &Profiles,
    collapsed: &HashSet<String>,
    live: &HashSet<ProfileId>,
) -> Vec<ConnRow> {
    let mut rows = Vec::new();
    let open = |key: &str| !collapsed.contains(key);

    // PINNED: favorite Hosts (MX-6), one click opens a terminal.
    let pinned: Vec<&Host> = profiles.hosts().filter(|h| h.favorite).collect();
    if !pinned.is_empty() {
        rows.push(ConnRow::head(
            "g:fav",
            "PINNED",
            String::new(),
            !open("g:fav"),
        ));
        if open("g:fav") {
            rows.extend(pinned.into_iter().map(|h| ConnRow {
                key: format!("fav:{}", h.id),
                env: Some(h.environment),
                ..leaf(&Profile::Host(h.clone()), 0, None, live)
            }));
        }
    }

    // SERVERS · SSH: Hosts (outside folders first), each with its terminal, files and
    // databases.
    let hosts: Vec<&Host> = profiles.hosts().collect();
    if !hosts.is_empty() {
        rows.push(ConnRow::head(
            "g:servers",
            "SERVERS · SSH",
            String::new(),
            !open("g:servers"),
        ));
        if open("g:servers") {
            let (loose, folders) = crate::sidebar::folder_groups(hosts.into_iter());
            let mut groups: Vec<(Option<String>, Vec<&Host>)> = vec![(None, loose)];
            groups.extend(folders.into_iter().map(|(f, hs)| (Some(f), hs)));
            for (folder, hosts) in groups {
                let depth = if let Some(name) = &folder {
                    let key = format!("fd:{name}");
                    rows.push(ConnRow {
                        folder: Some(name.clone()),
                        ..ConnRow::group(
                            key.clone(),
                            0,
                            format!("▣ {name}"),
                            hosts.len().to_string(),
                        )
                    });
                    if !open(&key) {
                        continue;
                    }
                    1
                } else {
                    0
                };
                for h in hosts {
                    let key = format!("h:{}", h.id);
                    let kids = profiles.host_children(&h.id);
                    rows.push(ConnRow {
                        env: Some(h.environment),
                        live: kids.iter().any(|k| live.contains(k.id())),
                        profile: Some(h.id.clone()),
                        drag_group: Some("hosts".into()),
                        ..ConnRow::group(key.clone(), depth, h.name.clone(), h.address.clone())
                    });
                    if open(&key) {
                        rows.push(host_terminal(h, depth + 1));
                        rows.extend(
                            kids.into_iter()
                                .map(|k| leaf(k, depth + 1, Some(&key), live)),
                        );
                    }
                }
            }
        }
    }

    // CLOUD ACCOUNTS: services by account, then by service.
    struct Account<'a> {
        key: String,
        label: String,
        env: Option<EnvironmentLabel>,
        groups: Vec<(&'static str, Vec<&'a Profile>)>,
    }
    let mut accounts: Vec<Account> = Vec::new();
    for p in &profiles.all {
        let (akey, alabel, gname) = match p {
            Profile::Cloud(c) => {
                let (k, l) = account_of_cloud(c.service, c.folder.as_deref(), &c.endpoint);
                (k, l, service_group(c.service))
            }
            Profile::Db(d) if d.via_host.is_none() => match cloud_db_service(d) {
                Some(g) => {
                    let (k, l) = account_of_db(d);
                    (k, l, g)
                }
                None => continue,
            },
            _ => continue,
        };
        let i = match accounts.iter().position(|a| a.key == akey) {
            Some(i) => i,
            None => {
                accounts.push(Account {
                    key: akey,
                    label: alabel,
                    env: None,
                    groups: Vec::new(),
                });
                accounts.len() - 1
            }
        };
        let a = &mut accounts[i];
        // The account's dot is its most exposed environment.
        let env = p.environment();
        if a.env.is_none_or(|e| env_rank(env) < env_rank(e)) {
            a.env = Some(env);
        }
        match a.groups.iter_mut().find(|(g, _)| *g == gname) {
            Some((_, list)) => list.push(p),
            None => a.groups.push((gname, vec![p])),
        }
    }
    if !accounts.is_empty() {
        rows.push(ConnRow::head(
            "g:cloud",
            "CLOUD ACCOUNTS",
            String::new(),
            !open("g:cloud"),
        ));
        if open("g:cloud") {
            for a in accounts {
                let n: usize = a.groups.iter().map(|(_, l)| l.len()).sum();
                rows.push(ConnRow {
                    env: a.env,
                    live: a
                        .groups
                        .iter()
                        .flat_map(|(_, l)| l)
                        .any(|p| live.contains(p.id())),
                    ..ConnRow::group(a.key.clone(), 0, a.label.clone(), n.to_string())
                });
                if !open(&a.key) {
                    continue;
                }
                for (g, list) in a.groups {
                    let gkey = format!("{}:{g}", a.key);
                    rows.push(ConnRow::group(
                        gkey.clone(),
                        1,
                        g.to_owned(),
                        list.len().to_string(),
                    ));
                    if open(&gkey) {
                        rows.extend(list.into_iter().map(|p| leaf(p, 2, Some(&gkey), live)));
                    }
                }
            }
        }
    }

    // DIRECT CONNECTIONS: everything not on a server or in a cloud account, plus the
    // local shell and local files.
    let direct: Vec<&Profile> = profiles
        .direct()
        .into_iter()
        .filter(|p| !matches!(p, Profile::Db(d) if cloud_db_service(d).is_some()))
        .collect();
    rows.push(ConnRow::head(
        "g:direct",
        "DIRECT CONNECTIONS",
        String::new(),
        !open("g:direct"),
    ));
    if open("g:direct") {
        rows.push(ConnRow {
            env: Some(EnvironmentLabel::Local),
            ..local_shell(0)
        });
        rows.extend(direct.into_iter().map(|p| ConnRow {
            env: Some(p.environment()),
            ..leaf(p, 0, Some("g:direct"), live)
        }));
        rows.push(ConnRow {
            env: Some(EnvironmentLabel::Local),
            ..local_files(0)
        });
    }
    rows
}

/// Production first, Local last.
fn env_rank(e: EnvironmentLabel) -> u8 {
    match e {
        EnvironmentLabel::Production => 0,
        EnvironmentLabel::Staging => 1,
        EnvironmentLabel::Development => 2,
        EnvironmentLabel::Local => 3,
    }
}

fn type_rows(
    profiles: &Profiles,
    collapsed: &HashSet<String>,
    live: &HashSet<ProfileId>,
    filter: &str,
) -> Vec<ConnRow> {
    // Every openable thing with the place it lives.
    let mut leaves: Vec<(TypeSec, ConnRow, String)> = Vec::new();
    for p in &profiles.all {
        let place = place_of(p, profiles);
        let mut row = leaf(p, 0, None, live);
        row.env = Some(p.environment());
        if let Profile::Host(h) = p {
            // A Host lists as its terminal.
            row = ConnRow {
                key: format!("t:{}", h.id),
                label: "Terminal".into(),
                env: Some(h.environment),
                ..host_terminal(h, 0)
            };
            leaves.push((TypeSec::Terminals, row, h.name.clone()));
            continue;
        }
        leaves.push((type_of(p), row, place));
    }
    leaves.push((
        TypeSec::Terminals,
        ConnRow {
            env: Some(EnvironmentLabel::Local),
            ..local_shell(0)
        },
        "direct".into(),
    ));
    leaves.push((
        TypeSec::Files,
        ConnRow {
            env: Some(EnvironmentLabel::Local),
            ..local_files(0)
        },
        "this machine".into(),
    ));
    let mut rows = Vec::new();
    for (sec, key, title) in TYPE_SECTIONS {
        let mut list: Vec<(usize, ConnRow)> = leaves
            .iter()
            .filter(|(s, _, _)| *s == sec)
            .filter_map(|(_, r, place)| {
                let hay = format!("{} {} {}", r.badge, r.label, place);
                fuzzy_score(filter, &hay).map(|score| {
                    let mut r = r.clone();
                    r.sub = place.clone().into();
                    (score, r)
                })
            })
            .collect();
        if list.is_empty() {
            continue;
        }
        if !filter.is_empty() {
            list.sort_by_key(|(s, _)| *s);
        }
        let open = !collapsed.contains(key);
        rows.push(ConnRow::head(key, title, list.len().to_string(), !open));
        if open {
            rows.extend(list.into_iter().map(|(_, r)| r));
        }
    }
    rows
}

/// How many openable rows a list holds (the filter's count).
pub fn leaf_count(rows: &[ConnRow]) -> usize {
    rows.iter().filter(|r| r.kind == RowKind::Leaf).count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use switchyard_core::store::{CloudConnection, FileConnection};

    fn host(id: &str, name: &str, favorite: bool) -> Profile {
        let mut h: Host = serde_json::from_value(serde_json::json!({
            "id": id, "name": name, "address": format!("{name}.example"), "port": 22,
            "user": "deploy", "auth": { "method": "agent" }, "environment": "production",
        }))
        .unwrap_or_else(|e| panic!("host: {e}"));
        h.favorite = favorite;
        Profile::Host(h)
    }

    fn db(id: &str, name: &str, engine: &str, via: Option<&str>) -> Profile {
        Profile::Db(
            serde_json::from_value(serde_json::json!({
                "id": id, "name": name, "engine": engine, "server": "db.example", "port": 5432,
                "database": "shop", "user": "app", "via_host": via, "environment": "staging",
            }))
            .unwrap_or_else(|e| panic!("db: {e}")),
        )
    }

    fn cloud(id: &str, name: &str, service: &str) -> Profile {
        let c: CloudConnection = serde_json::from_value(serde_json::json!({
            "id": id, "name": name, "service": service, "environment": "production",
        }))
        .unwrap_or_else(|e| panic!("cloud: {e}"));
        Profile::Cloud(c)
    }

    fn ftp(id: &str, name: &str) -> Profile {
        let f: FileConnection = serde_json::from_value(serde_json::json!({
            "id": id, "name": name, "environment": "local",
            "protocol": { "protocol": "ftp", "server": "ftp.example", "port": 21, "tls": "none", "mode": "passive", "user": "u" },
        }))
        .unwrap_or_else(|e| panic!("ftp: {e}"));
        Profile::File(f)
    }

    fn sample() -> Profiles {
        Profiles {
            all: vec![
                host("h1", "prod-db-01", true),
                db("d1", "shop_prod", "postgres", Some("h1")),
                db("d2", "Reporting", "sqlserver", None),
                db("d3", "shop_edge", "d1", None),
                cloud("c1", "acme-assets", "s3"),
                cloud("c2", "appcs-prod", "app-config"),
                cloud("c3", "media", "r2"),
                ftp("f1", "assets.acme.dev"),
            ],
        }
    }

    fn labels(rows: &[ConnRow]) -> Vec<String> {
        rows.iter()
            .map(|r| format!("{}{}", "  ".repeat(r.depth as usize), r.label))
            .collect()
    }

    #[test]
    fn place_groups_servers_cloud_accounts_and_direct() {
        let rows = explorer_rows(
            &sample(),
            &HashSet::new(),
            &HashSet::new(),
            ExplorerGroup::Place,
            "",
        );
        assert_eq!(
            labels(&rows),
            [
                "PINNED",
                "prod-db-01",
                "SERVERS · SSH",
                "prod-db-01",
                "  Terminal",
                "  shop_prod",
                "CLOUD ACCOUNTS",
                "Cloudflare",
                "  D1 databases",
                "    shop_edge",
                "  R2 buckets",
                "    media",
                "AWS",
                "  S3 buckets",
                "    acme-assets",
                "Azure",
                "  App Configuration",
                "    appcs-prod",
                "DIRECT CONNECTIONS",
                "Local shell",
                "Reporting",
                "assets.acme.dev",
                "Local files",
            ]
        );
    }

    #[test]
    fn collapsed_sections_hide_their_rows() {
        let collapsed: HashSet<String> = ["g:cloud".to_owned(), "h:h1".to_owned()].into();
        let rows = explorer_rows(
            &sample(),
            &collapsed,
            &HashSet::new(),
            ExplorerGroup::Place,
            "",
        );
        let l = labels(&rows);
        assert!(!l.contains(&"AWS".to_owned()));
        assert!(!l.contains(&"  shop_prod".to_owned()));
        let cloud = rows
            .iter()
            .find(|r| r.key == "g:cloud")
            .map(|r| r.sub.to_string());
        assert_eq!(cloud.as_deref(), Some("show"));
    }

    #[test]
    fn type_groups_list_each_source_with_its_place() {
        let rows = explorer_rows(
            &sample(),
            &HashSet::new(),
            &HashSet::new(),
            ExplorerGroup::Type,
            "",
        );
        let heads: Vec<_> = rows
            .iter()
            .filter(|r| r.kind == RowKind::Head)
            .map(|r| (r.label.to_string(), r.sub.to_string()))
            .collect();
        assert_eq!(
            heads,
            [
                ("DATABASES".to_owned(), "3".to_owned()),
                ("TERMINALS".to_owned(), "2".to_owned()),
                ("FILES & STORAGE".to_owned(), "4".to_owned()),
                ("CONFIG & SECRETS".to_owned(), "1".to_owned()),
            ]
        );
        let shop = rows.iter().find(|r| r.label.as_ref() == "shop_prod");
        assert_eq!(
            shop.map(|r| r.sub.to_string()).as_deref(),
            Some("prod-db-01")
        );
        let edge = rows.iter().find(|r| r.label.as_ref() == "shop_edge");
        assert_eq!(
            edge.map(|r| r.sub.to_string()).as_deref(),
            Some("Cloudflare")
        );
    }

    #[test]
    fn filter_lists_matches_by_type_with_a_count() {
        let rows = explorer_rows(
            &sample(),
            &HashSet::new(),
            &HashSet::new(),
            ExplorerGroup::Place,
            "shop",
        );
        assert_eq!(labels(&rows), ["DATABASES", "shop_prod", "shop_edge"]);
        assert_eq!(leaf_count(&rows), 2);
        // The place is searched too.
        let rows = explorer_rows(
            &sample(),
            &HashSet::new(),
            &HashSet::new(),
            ExplorerGroup::Place,
            "prod-db",
        );
        assert!(labels(&rows).contains(&"shop_prod".to_owned()));
    }

    #[test]
    fn folders_split_cloud_accounts() {
        let mut p = sample();
        if let Some(Profile::Cloud(c)) = p.all.iter_mut().find(|p| p.id().0 == "c1") {
            c.folder = Some("sandbox".into());
        }
        let rows = explorer_rows(
            &p,
            &HashSet::new(),
            &HashSet::new(),
            ExplorerGroup::Place,
            "",
        );
        assert!(labels(&rows).contains(&"AWS · sandbox".to_owned()));
    }

    #[test]
    fn group_setting_round_trips() {
        for g in [ExplorerGroup::Place, ExplorerGroup::Type] {
            assert_eq!(ExplorerGroup::from_key(g.key()), g);
        }
        assert_eq!(ExplorerGroup::from_key("?"), ExplorerGroup::Place);
    }
}
