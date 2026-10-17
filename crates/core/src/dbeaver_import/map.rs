//! One DBeaver connection (`data-sources.json` entry) as a Switchyard database connection,
//! plus the SSH tunnel it goes through.

use secrecy::SecretString;
use serde_json::Value;
use switchyard_db::{DbAuthMethod, Engine, SslMode};
use switchyard_store::{DbConnection, EnvironmentLabel, SshAuth};

use super::source::{Credentials, flag, str_of};

/// A DBeaver SSH tunnel.
pub(crate) struct Tunnel {
    pub address: String,
    pub port: u16,
    pub user: String,
    pub auth: SshAuth,
    /// Password, or the key's passphrase.
    pub secret: Option<SecretString>,
}

/// A DBeaver connection mapped onto Switchyard's model.
pub(crate) struct Mapped {
    /// `via_host` is not set; the plan wires the tunnel.
    pub db: DbConnection,
    pub password: Option<SecretString>,
    pub tunnel: Option<Tunnel>,
}

/// DBeaver's provider and driver ids → engine. `None`: no Switchyard driver for it.
pub(crate) fn engine_of(provider: &str, driver: &str) -> Option<Engine> {
    let (p, d) = (provider.to_ascii_lowercase(), driver.to_ascii_lowercase());
    let either = |s: &str| p.contains(s) || d.contains(s);
    Some(match p.as_str() {
        "postgresql" | "postgres" => Engine::Postgres,
        "sqlserver" | "mssql" => Engine::SqlServer,
        "mysql" | "mariadb" => Engine::MySql,
        "oracle" => Engine::Oracle,
        "snowflake" => Engine::Snowflake,
        "sqlite" => Engine::Sqlite,
        "mongodb" | "mongo" => Engine::MongoDb,
        "redis" => Engine::Redis,
        // Drivers DBeaver runs through its generic JDBC provider.
        _ if either("sqlite") => Engine::Sqlite,
        _ if either("snowflake") => Engine::Snowflake,
        _ => return None,
    })
}

/// Map one `connections` entry. `Err` says why it cannot be imported.
pub(crate) fn map(
    id: &str,
    conn: &Value,
    types: &Value,
    folder_prefix: Option<&str>,
    creds: &Credentials,
) -> Result<Mapped, String> {
    let provider = str_of(conn, "provider").unwrap_or_default();
    let driver = str_of(conn, "driver").unwrap_or_default();
    let Some(engine) = engine_of(&provider, &driver) else {
        return Err(format!("no Switchyard driver for {}", driver_label(conn)));
    };
    let cfg = conn.get("configuration").unwrap_or(&Value::Null);
    let name = str_of(conn, "name").unwrap_or_else(|| id.to_owned());
    let mut db = DbConnection::new(name, engine);

    let url = str_of(cfg, "url").unwrap_or_default();
    let from_url = parse_jdbc_url(&url);
    let host = str_of(cfg, "host").or_else(|| from_url.host.clone());
    let port = str_of(cfg, "port")
        .and_then(|p| p.parse().ok())
        .or(from_url.port);
    let database = str_of(cfg, "database").or_else(|| from_url.database.clone());
    db.server = host.unwrap_or_default();
    if let Some(port) = port {
        db.port = port;
    }
    db.database = database.unwrap_or_default();

    let props = cfg.get("provider-properties").unwrap_or(&Value::Null);
    match engine {
        Engine::Sqlite => {
            // The file path is the "database" (or the URL after `jdbc:sqlite:`).
            if db.database.is_empty() {
                db.database = db.server.clone();
            }
            db.server.clear();
            db.port = 0;
        }
        Engine::Oracle => oracle_target(&mut db, props),
        Engine::Redis if db.database.is_empty() => db.database = "0".into(),
        _ => {}
    }

    let saved = creds.db.get(id);
    db.user = saved
        .and_then(|s| s.user.clone())
        .or_else(|| str_of(cfg, "user"))
        .unwrap_or_default();
    let save_password = !conn.get("save-password").is_some_and(|v| v == false);
    let password = saved
        .and_then(|s| s.password.clone())
        .or_else(|| str_of(cfg, "password").map(SecretString::from))
        .filter(|_| save_password);
    db.auth = auth_of(engine, str_of(cfg, "auth-model").as_deref());

    db.environment = environment_of(str_of(cfg, "type").as_deref(), types);
    db.read_only = flag(conn, "read-only") || flag(cfg, "read-only");
    db.folder = match (folder_prefix, str_of(conn, "folder")) {
        (Some(p), Some(f)) => Some(format!("{p}/{f}")),
        (Some(p), None) => Some(p.to_owned()),
        (None, f) => f,
    };

    let handlers = cfg.get("handlers").and_then(Value::as_object);
    let mut tunnel = None;
    for (key, h) in handlers.into_iter().flatten() {
        if !flag(h, "enabled") {
            continue;
        }
        let hp = h.get("properties").unwrap_or(&Value::Null);
        if key == "ssh_tunnel" {
            tunnel = Some(tunnel_of(id, h, hp, creds)?);
        } else if key.contains("ssl") {
            db.ssl_mode = ssl_of(hp);
        }
    }
    Ok(Mapped {
        db,
        password,
        tunnel,
    })
}

/// `provider / driver` for messages.
pub(crate) fn driver_label(conn: &Value) -> String {
    match (str_of(conn, "provider"), str_of(conn, "driver")) {
        (Some(p), Some(d)) if p != d => format!("{p} ({d})"),
        (Some(p), _) => p,
        (None, Some(d)) => d,
        (None, None) => "an unknown driver".into(),
    }
}

/// Oracle: DBeaver connects by service name, SID or TNS alias.
fn oracle_target(db: &mut DbConnection, props: &Value) {
    let kind = str_of(props, "@dbeaver-connection-type@").unwrap_or_default();
    if kind.eq_ignore_ascii_case("TNS") {
        if let Some(alias) = str_of(props, "@dbeaver-tns-name@") {
            db.database = alias;
        }
        db.server.clear();
        return;
    }
    let sid = str_of(props, "@dbeaver-sid-service@").is_some_and(|s| s.eq_ignore_ascii_case("SID"));
    if sid && !db.server.is_empty() {
        // Easy Connect only names services; a SID needs a descriptor.
        db.database = format!(
            "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCP)(HOST={})(PORT={}))(CONNECT_DATA=(SID={})))",
            db.server, db.port, db.database
        );
        db.server.clear();
    }
}

/// DBeaver's `auth-model` → sign-in method.
fn auth_of(engine: Engine, model: Option<&str>) -> DbAuthMethod {
    let m = model.unwrap_or_default().to_ascii_lowercase();
    if engine != Engine::SqlServer {
        return DbAuthMethod::Password;
    }
    if m.contains("ad_password") || m.contains("active_directory_password") {
        DbAuthMethod::EntraPassword
    } else if m.contains("ad_interactive") || m.contains("interactive") {
        DbAuthMethod::EntraInteractive
    } else if m.contains("ntlm") {
        DbAuthMethod::WindowsPassword
    } else if m.contains("windows") || m.contains("kerberos") || m.contains("integrated") {
        DbAuthMethod::Integrated
    } else {
        DbAuthMethod::Password
    }
}

/// DBeaver's connection type (`dev`, `test`, `prod` or a custom one) → environment.
fn environment_of(kind: Option<&str>, types: &Value) -> EnvironmentLabel {
    let Some(kind) = kind else {
        return EnvironmentLabel::Local;
    };
    let name = types
        .get(kind)
        .and_then(|t| str_of(t, "name"))
        .unwrap_or_default();
    let s = format!("{kind} {name}").to_ascii_lowercase();
    if s.contains("prod") {
        EnvironmentLabel::Production
    } else if s.contains("stag") || s.contains("test") || s.contains("qa") {
        EnvironmentLabel::Staging
    } else if s.contains("dev") {
        EnvironmentLabel::Development
    } else {
        EnvironmentLabel::Local
    }
}

/// An enabled SSL handler's properties → TLS policy.
fn ssl_of(props: &Value) -> SslMode {
    let mode = ["sslMode", "ssl.mode", "sslmode"]
        .iter()
        .find_map(|k| str_of(props, k))
        .unwrap_or_default()
        .to_ascii_lowercase();
    match mode.as_str() {
        "disable" => SslMode::Disable,
        "allow" | "prefer" => SslMode::Prefer,
        "verify-full" | "verify_identity" => SslMode::VerifyFull,
        "require" | "verify-ca" | "verify_ca" => SslMode::Require,
        _ if flag(props, "ssl.verify.server") => SslMode::VerifyFull,
        _ => SslMode::Require,
    }
}

fn tunnel_of(id: &str, h: &Value, props: &Value, creds: &Credentials) -> Result<Tunnel, String> {
    let address = str_of(props, "host").ok_or("the SSH tunnel has no host")?;
    let saved = creds.ssh.get(id);
    let user = saved
        .and_then(|s| s.user.clone())
        .or_else(|| str_of(props, "user"))
        .or_else(|| str_of(h, "user"))
        .ok_or("the SSH tunnel has no saved user")?;
    let auth = match str_of(props, "authType").as_deref() {
        Some("PUBLIC_KEY") => match str_of(props, "keyPath") {
            Some(key_path) => SshAuth::PublicKey { key_path },
            None => SshAuth::Agent,
        },
        Some("AGENT") => SshAuth::Agent,
        _ => SshAuth::Password,
    };
    let save_password = !h.get("save-password").is_some_and(|v| v == false);
    Ok(Tunnel {
        address,
        port: str_of(props, "port")
            .and_then(|p| p.parse().ok())
            .unwrap_or(22),
        user,
        auth,
        secret: saved
            .and_then(|s| s.password.clone())
            .filter(|_| save_password),
    })
}

/// What a JDBC URL names (used when DBeaver saved only the URL).
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct UrlParts {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub database: Option<String>,
}

/// `jdbc:<sub>://host[:port][/db][;k=v…][?…]`, SQL Server's `;databaseName=`, Oracle's
/// `@//host:port/service` and `@host:port:SID`, and `jdbc:sqlite:<path>`.
pub(crate) fn parse_jdbc_url(url: &str) -> UrlParts {
    let Some(rest) = url.strip_prefix("jdbc:") else {
        return UrlParts::default();
    };
    if let Some(path) = rest.strip_prefix("sqlite:") {
        return UrlParts {
            database: Some(path.to_owned()).filter(|p| !p.is_empty()),
            ..Default::default()
        };
    }
    let oracle = rest.strip_prefix("oracle:").and_then(|r| r.split_once('@'));
    let (target, oracle_sid) = match oracle {
        // Oracle thin: `oracle:thin:@//h:p/svc` or `oracle:thin:@h:p:SID`.
        Some((_, t)) => match t.strip_prefix("//") {
            Some(t) => (t, false),
            None => (t, true),
        },
        None => match rest.split_once("://") {
            Some((_, t)) => (t, false),
            None => return UrlParts::default(),
        },
    };
    let (target, params) = target.split_once(';').unwrap_or((target, ""));
    let target = target.split('?').next().unwrap_or_default();
    let (authority, mut database) = match target.split_once('/') {
        Some((a, d)) => (a, Some(d.to_owned()).filter(|d| !d.is_empty())),
        None => (target, None),
    };
    let mut parts: Vec<&str> = authority.splitn(3, ':').collect();
    if oracle_sid && parts.len() == 3 {
        database = parts.pop().map(str::to_owned);
    }
    for kv in params.split(';') {
        if let Some((k, v)) = kv.split_once('=')
            && k.eq_ignore_ascii_case("databaseName")
        {
            database = Some(v.to_owned());
        }
    }
    // SQL Server's `host\instance` keeps its instance.
    UrlParts {
        host: parts
            .first()
            .map(|h| (*h).to_owned())
            .filter(|h| !h.is_empty()),
        port: parts.get(1).and_then(|p| p.parse().ok()),
        database,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn reads_jdbc_urls() {
        let p = |h: &str, port: Option<u16>, d: Option<&str>| UrlParts {
            host: Some(h.into()),
            port,
            database: d.map(Into::into),
        };
        assert_eq!(
            parse_jdbc_url("jdbc:postgresql://db.example:5433/app?ssl=true"),
            p("db.example", Some(5433), Some("app"))
        );
        assert_eq!(
            parse_jdbc_url("jdbc:sqlserver://sql01:1433;databaseName=Sales;encrypt=true"),
            p("sql01", Some(1433), Some("Sales"))
        );
        assert_eq!(
            parse_jdbc_url("jdbc:mysql://localhost/shop"),
            p("localhost", None, Some("shop"))
        );
        assert_eq!(
            parse_jdbc_url("jdbc:oracle:thin:@//ora:1521/ORCLPDB1"),
            p("ora", Some(1521), Some("ORCLPDB1"))
        );
        assert_eq!(
            parse_jdbc_url("jdbc:oracle:thin:@ora:1521:XE"),
            p("ora", Some(1521), Some("XE"))
        );
        assert_eq!(
            parse_jdbc_url("jdbc:sqlite:/home/me/app.db")
                .database
                .as_deref(),
            Some("/home/me/app.db")
        );
        assert_eq!(parse_jdbc_url("not a url"), UrlParts::default());
    }

    #[test]
    fn maps_drivers() {
        assert_eq!(
            engine_of("postgresql", "postgres-jdbc"),
            Some(Engine::Postgres)
        );
        assert_eq!(
            engine_of("sqlserver", "mssql_jdbc_ms_new"),
            Some(Engine::SqlServer)
        );
        assert_eq!(engine_of("mysql", "mariaDB"), Some(Engine::MySql));
        assert_eq!(engine_of("generic", "sqlite_jdbc"), Some(Engine::Sqlite));
        assert_eq!(engine_of("generic", "h2_embedded"), None);
        assert_eq!(engine_of("clickhouse", "clickhouse"), None);
    }

    #[test]
    fn sql_server_sign_in_and_environments() {
        assert_eq!(
            auth_of(Engine::SqlServer, Some("sqlserver_windows")),
            DbAuthMethod::Integrated
        );
        assert_eq!(
            auth_of(Engine::SqlServer, Some("sqlserver_ntlm")),
            DbAuthMethod::WindowsPassword
        );
        assert_eq!(auth_of(Engine::SqlServer, None), DbAuthMethod::Password);
        assert_eq!(
            auth_of(Engine::Postgres, Some("postgres_pgpass")),
            DbAuthMethod::Password
        );
        let types = serde_json::json!({"cust": {"name": "Customer PROD"}});
        assert_eq!(
            environment_of(Some("prod"), &types),
            EnvironmentLabel::Production
        );
        assert_eq!(
            environment_of(Some("cust"), &types),
            EnvironmentLabel::Production
        );
        assert_eq!(
            environment_of(Some("test"), &types),
            EnvironmentLabel::Staging
        );
        assert_eq!(
            environment_of(Some("dev"), &types),
            EnvironmentLabel::Development
        );
        assert_eq!(environment_of(None, &types), EnvironmentLabel::Local);
    }
}
