//! Importing database connections from DBeaver: its `data-sources*.json` files (found in
//! every DBeaver workspace on this machine, or one the user picks), with the user names and
//! passwords DBeaver saved in `credentials-config.json`. SSH tunnels become Hosts.

mod map;
mod source;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use secrecy::SecretString;
use serde_json::Value;
use switchyard_db::Engine;
use switchyard_store::{DbConnection, EnvironmentLabel, Host, Profile, ProfileId, SecretRef};

use map::Mapped;

/// One DBeaver connection as the import preview shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DbeaverCandidate {
    /// Identifies it in [`crate::Command::ImportDbeaver`].
    pub key: String,
    /// Its name in DBeaver.
    pub name: String,
    /// The engine it becomes; `None` when Switchyard has no driver for it.
    pub engine: Option<Engine>,
    /// DBeaver's provider and driver.
    pub driver: String,
    /// `user@host:port/database`, or the file of a SQLite database.
    pub address: String,
    /// Sidebar folder.
    pub folder: Option<String>,
    /// Environment, from DBeaver's connection type.
    pub environment: EnvironmentLabel,
    /// DBeaver saved a password, which comes along.
    pub password: bool,
    /// The SSH tunnel's `user@host:port`; it becomes a Host (or uses a saved one).
    pub via: Option<String>,
    /// A connection with this name is already saved (it is skipped).
    pub exists: bool,
    /// Why it cannot be imported.
    pub problem: Option<String>,
}

/// One `connections` entry read from a file.
pub(crate) struct Entry {
    key: String,
    name: String,
    driver: String,
    mapped: Result<Mapped, String>,
}

/// Every `data-sources*.json` of the DBeaver workspaces on this machine.
pub fn default_files() -> Vec<PathBuf> {
    source::workspace_roots()
        .iter()
        .flat_map(|w| source::data_source_files(w))
        .collect()
}

/// Read the connections in `files`, decrypting the credentials saved beside each.
pub(crate) fn read(files: &[PathBuf]) -> Result<Vec<Entry>, String> {
    let mut out = Vec::new();
    for (i, file) in files.iter().enumerate() {
        let text = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
        let json: Value = serde_json::from_str(&text)
            .map_err(|e| format!("{} is not a DBeaver data-sources file: {e}", file.display()))?;
        out.extend(entries(i, file, &json, &source::read_credentials(file)));
    }
    Ok(out)
}

fn entries(i: usize, file: &Path, json: &Value, creds: &source::Credentials) -> Vec<Entry> {
    let types = json.get("connection-types").unwrap_or(&Value::Null);
    // Connections of other projects keep their project as the top folder.
    let project = source::project_of(file);
    let prefix = (!project.is_empty() && project != "General").then_some(project.as_str());
    json.get("connections")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(id, conn)| Entry {
            key: format!("{i}:{id}"),
            name: source::str_of(conn, "name").unwrap_or_else(|| id.clone()),
            driver: map::driver_label(conn),
            mapped: map::map(id, conn, types, prefix, creds),
        })
        .collect()
}

/// The tunnel Host a connection uses: a saved one with the same address, port and user.
fn same_host(hosts: &[Host], t: &map::Tunnel) -> Option<ProfileId> {
    hosts
        .iter()
        .find(|h| {
            h.address.eq_ignore_ascii_case(&t.address) && h.port == t.port && h.user == t.user
        })
        .map(|h| h.id.clone())
}

fn address_of(db: &DbConnection) -> String {
    if db.engine.is_local_file() {
        return db.database.clone();
    }
    let user = if db.user.is_empty() {
        String::new()
    } else {
        format!("{}@", db.user)
    };
    let port = if db.port == db.engine.default_port() || db.server.is_empty() {
        String::new()
    } else {
        format!(":{}", db.port)
    };
    let db_name = if db.database.is_empty() {
        String::new()
    } else if db.server.is_empty() {
        db.database.clone()
    } else {
        format!("/{}", db.database)
    };
    format!("{user}{}{port}{db_name}", db.server)
}

/// What importing `entries` would do, given the saved connection names and Hosts.
pub(crate) fn candidates(
    entries: &[Entry],
    saved: &HashSet<String>,
    hosts: &[Host],
) -> Vec<DbeaverCandidate> {
    entries
        .iter()
        .map(|e| match &e.mapped {
            Ok(m) => {
                let mut db = m.db.clone();
                let host = m.tunnel.as_ref().map(|t| tunnel_host(t, hosts));
                db.via_host = host.as_ref().map(|h| h.id.clone());
                let problem = Profile::Db(db)
                    .validate()
                    .err()
                    .or_else(|| host.and_then(|h| Profile::Host(h).validate().err()))
                    .map(|e| e.message);
                DbeaverCandidate {
                    key: e.key.clone(),
                    name: m.db.name.clone(),
                    engine: Some(m.db.engine),
                    driver: e.driver.clone(),
                    address: address_of(&m.db),
                    folder: m.db.folder.clone(),
                    environment: m.db.environment,
                    password: m.password.is_some(),
                    via: m
                        .tunnel
                        .as_ref()
                        .map(|t| format!("{}@{}:{}", t.user, t.address, t.port)),
                    exists: saved.contains(&m.db.name),
                    problem,
                }
            }
            Err(why) => DbeaverCandidate {
                key: e.key.clone(),
                name: e.name.clone(),
                engine: None,
                driver: e.driver.clone(),
                address: String::new(),
                folder: None,
                environment: EnvironmentLabel::Local,
                password: false,
                via: None,
                exists: false,
                problem: Some(why.clone()),
            },
        })
        .collect()
}

/// A saved Host for the tunnel, or a new one.
fn tunnel_host(t: &map::Tunnel, hosts: &[Host]) -> Host {
    if let Some(id) = same_host(hosts, t)
        && let Some(h) = hosts.iter().find(|h| h.id == id)
    {
        return h.clone();
    }
    let name = if hosts.iter().any(|h| h.name == t.address) {
        format!("{}@{}", t.user, t.address)
    } else {
        t.address.clone()
    };
    let mut h = Host::new(name, t.address.clone(), t.user.clone());
    h.port = t.port;
    h.auth = t.auth.clone();
    h
}

/// What an import saves: new tunnel Hosts and the connections, each with the secret to
/// store for it.
#[derive(Default)]
pub(crate) struct ImportPlan {
    pub hosts: Vec<(Host, Option<SecretString>)>,
    pub connections: Vec<(DbConnection, Option<SecretString>)>,
}

/// The connections named in `only` that can be imported and are not saved yet, with the
/// Hosts their tunnels need (one per address, port and user).
pub(crate) fn plan(
    entries: Vec<Entry>,
    only: &HashSet<String>,
    saved: &HashSet<String>,
    hosts: &[Host],
) -> ImportPlan {
    let mut out = ImportPlan::default();
    let mut known: Vec<Host> = hosts.to_vec();
    let mut names = saved.clone();
    for e in entries {
        let Ok(m) = e.mapped else { continue };
        if !only.contains(&e.key) || names.contains(&m.db.name) {
            continue;
        }
        let mut db = m.db;
        if let Some(t) = &m.tunnel {
            let host = tunnel_host(t, &known);
            if !known.iter().any(|h| h.id == host.id) {
                if Profile::Host(host.clone()).validate().is_err() {
                    continue;
                }
                known.push(host.clone());
                out.hosts.push((host.clone(), t.secret.clone()));
            }
            db.via_host = Some(host.id);
        }
        if Profile::Db(db.clone()).validate().is_err() {
            continue;
        }
        names.insert(db.name.clone());
        out.connections.push((db, m.password));
    }
    out
}

/// Where an imported profile's secret is stored.
pub(crate) fn secret_ref(profile: &Profile) -> SecretRef {
    let purpose = match profile {
        Profile::Host(_) => "passphrase",
        _ => "password",
    };
    SecretRef::for_profile(profile.id(), purpose)
}

/// Saved connection names and Hosts, for [`candidates`] and [`plan`].
pub(crate) fn saved(profiles: &[Profile]) -> (HashSet<String>, Vec<Host>) {
    let mut names = HashSet::new();
    let mut hosts = Vec::new();
    for p in profiles {
        match p {
            Profile::Db(d) => {
                names.insert(d.name.clone());
            }
            Profile::Host(h) => hosts.push(h.clone()),
            _ => {}
        }
    }
    (names, hosts)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use secrecy::ExposeSecret;
    use switchyard_db::{DbAuthMethod, SslMode};
    use switchyard_store::SshAuth;

    const DATA_SOURCES: &str = r#"{
  "folders": {"Prod": {}},
  "connections": {
    "postgres-jdbc-1": {
      "provider": "postgresql", "driver": "postgres-jdbc", "name": "orders",
      "save-password": true, "folder": "Prod",
      "configuration": {
        "host": "10.0.0.5", "port": "5433", "database": "orders",
        "url": "jdbc:postgresql://10.0.0.5:5433/orders",
        "type": "prod", "auth-model": "native",
        "handlers": {
          "ssh_tunnel": {"type": "TUNNEL", "enabled": true, "save-password": true,
            "properties": {"host": "bastion.corp", "port": 22, "authType": "PUBLIC_KEY",
              "keyPath": "~/.ssh/id_ed25519"}},
          "postgre_ssl": {"type": "CONFIG", "enabled": true,
            "properties": {"sslMode": "verify-full"}}
        }
      }
    },
    "mssql_jdbc_ms_new-2": {
      "provider": "sqlserver", "driver": "mssql_jdbc_ms_new", "name": "Sales",
      "configuration": {"url": "jdbc:sqlserver://sql01:1444;databaseName=Sales",
        "type": "dev", "auth-model": "sqlserver_windows"}
    },
    "postgres-jdbc-3": {
      "provider": "postgresql", "driver": "postgres-jdbc", "name": "reports",
      "configuration": {"host": "10.0.0.6", "database": "reports",
        "handlers": {"ssh_tunnel": {"enabled": true,
          "properties": {"host": "bastion.corp", "port": "22", "authType": "PUBLIC_KEY",
            "keyPath": "~/.ssh/id_ed25519"}}}}
    },
    "clickhouse-4": {"provider": "clickhouse", "driver": "com_clickhouse", "name": "events",
      "configuration": {"host": "ch"}},
    "sqlite_jdbc-5": {"provider": "generic", "driver": "sqlite_jdbc", "name": "local",
      "configuration": {"database": "/home/me/app.db"}},
    "oracle_thin-6": {"provider": "oracle", "driver": "oracle_thin", "name": "erp",
      "configuration": {"host": "ora", "port": "1521", "database": "XE",
        "provider-properties": {"@dbeaver-sid-service@": "SID"}}},
    "postgres-jdbc-7": {"provider": "postgresql", "driver": "postgres-jdbc", "name": "old",
      "configuration": {"host": "old"}}
  },
  "connection-types": {"prod": {"name": "Production"}}
}"#;

    const CREDS: &str = r##"{
      "postgres-jdbc-1": {"#connection": {"user": "app", "password": "s3cret"},
        "network/ssh_tunnel": {"user": "ops", "password": "keypass"}},
      "postgres-jdbc-3": {"#connection": {"user": "rep"},
        "network/ssh_tunnel": {"user": "ops"}},
      "mssql_jdbc_ms_new-2": {"#connection": {"user": "CORP\\me"}},
      "oracle_thin-6": {"#connection": {"user": "scott", "password": "tiger"}}
    }"##;

    fn workspace() -> (tempfile::TempDir, PathBuf) {
        let ws = tempfile::tempdir().unwrap();
        let dir = ws.path().join("General").join(".dbeaver");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("data-sources.json"), DATA_SOURCES).unwrap();
        std::fs::write(
            dir.join("credentials-config.json"),
            source::tests::encrypt(CREDS.as_bytes()),
        )
        .unwrap();
        let file = dir.join("data-sources.json");
        (ws, file)
    }

    #[test]
    fn preview_maps_supported_drivers_and_explains_the_rest() {
        let (_ws, file) = workspace();
        let entries = read(&[file]).unwrap();
        let saved: HashSet<String> = ["old".to_owned()].into();
        let c = candidates(&entries, &saved, &[]);
        let rows: Vec<(&str, Option<Engine>, &str, bool, bool)> = c
            .iter()
            .map(|c| {
                (
                    c.name.as_str(),
                    c.engine,
                    c.address.as_str(),
                    c.password,
                    c.problem.is_none() && !c.exists,
                )
            })
            .collect();
        assert_eq!(
            rows,
            [
                (
                    "orders",
                    Some(Engine::Postgres),
                    "app@10.0.0.5:5433/orders",
                    true,
                    true
                ),
                (
                    "Sales",
                    Some(Engine::SqlServer),
                    "CORP\\me@sql01:1444/Sales",
                    false,
                    true
                ),
                (
                    "reports",
                    Some(Engine::Postgres),
                    "rep@10.0.0.6/reports",
                    false,
                    true
                ),
                ("events", None, "", false, false),
                (
                    "local",
                    Some(Engine::Sqlite),
                    "/home/me/app.db",
                    false,
                    true
                ),
                (
                    "erp",
                    Some(Engine::Oracle),
                    "scott@(DESCRIPTION=(ADDRESS=(PROTOCOL=TCP)(HOST=ora)(PORT=1521))(CONNECT_DATA=(SID=XE)))",
                    true,
                    true
                ),
                ("old", Some(Engine::Postgres), "old", false, false),
            ]
        );
        assert_eq!(
            c[3].problem.as_deref(),
            Some("no Switchyard driver for clickhouse (com_clickhouse)")
        );
        assert_eq!(c[0].via.as_deref(), Some("ops@bastion.corp:22"));
        assert_eq!(c[0].environment, EnvironmentLabel::Production);
        assert_eq!(c[0].folder.as_deref(), Some("Prod"));
        assert!(c[6].exists);
    }

    #[test]
    fn import_shares_one_tunnel_host_and_keeps_secrets() {
        let (_ws, file) = workspace();
        let entries = read(&[file]).unwrap();
        let only: HashSet<String> = [
            "0:postgres-jdbc-1",
            "0:postgres-jdbc-3",
            "0:mssql_jdbc_ms_new-2",
        ]
        .map(String::from)
        .into();
        let plan = plan(entries, &only, &HashSet::new(), &[]);
        assert_eq!(plan.hosts.len(), 1, "both tunnels go through bastion");
        let (host, pass) = &plan.hosts[0];
        assert_eq!(
            (host.address.as_str(), host.user.as_str()),
            ("bastion.corp", "ops")
        );
        assert_eq!(
            host.auth,
            SshAuth::PublicKey {
                key_path: "~/.ssh/id_ed25519".into()
            }
        );
        assert_eq!(pass.as_ref().unwrap().expose_secret(), "keypass");

        let names: Vec<&str> = plan
            .connections
            .iter()
            .map(|(d, _)| d.name.as_str())
            .collect();
        assert_eq!(names, ["orders", "Sales", "reports"]);
        let (sales, _) = &plan.connections[1];
        assert_eq!(sales.auth, DbAuthMethod::Integrated);
        assert_eq!(sales.environment, EnvironmentLabel::Development);
        let (orders, pw) = &plan.connections[0];
        assert_eq!(orders.via_host.as_ref(), Some(&host.id));
        assert_eq!(orders.ssl_mode, SslMode::VerifyFull);
        assert_eq!(orders.port, 5433);
        assert_eq!(pw.as_ref().unwrap().expose_secret(), "s3cret");
        assert_eq!(plan.connections[2].0.via_host.as_ref(), Some(&host.id));
    }

    #[test]
    fn reuses_a_saved_host_and_skips_saved_names() {
        let (_ws, file) = workspace();
        let mut bastion = Host::new("bastion", "BASTION.corp", "ops");
        bastion.port = 22;
        let only: HashSet<String> = ["0:postgres-jdbc-1", "0:postgres-jdbc-7"]
            .map(String::from)
            .into();
        let saved: HashSet<String> = ["old".to_owned()].into();
        let plan = plan(
            read(&[file]).unwrap(),
            &only,
            &saved,
            std::slice::from_ref(&bastion),
        );
        assert!(plan.hosts.is_empty());
        assert_eq!(plan.connections.len(), 1);
        assert_eq!(plan.connections[0].0.via_host.as_ref(), Some(&bastion.id));
    }

    #[test]
    fn a_broken_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("data-sources.json");
        std::fs::write(&f, "not json").unwrap();
        let Err(e) = read(&[f]) else {
            panic!("read a broken file")
        };
        assert!(e.contains("not a DBeaver data-sources file"));
    }
}
