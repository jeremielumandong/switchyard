//! Connection editor dialog: type picker, a form per type, environment label, Driver
//! Manager card, Test connection and Save.

use std::collections::HashMap;

use gpui_kit::component::input::{Input, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, EventEmitter, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, deferred, div, px,
};
use secrecy::SecretString;
use switchyard_core::db::{DbAuthMethod, Engine, SslMode};
use switchyard_core::drivers::{Component, ComponentStatus};
use switchyard_core::store::{
    DbConnection, EnvironmentLabel, FileConnection, FileProtocol, FtpMode, FtpTls, Host, Profile,
    ProfileId, SshAuth,
};
use switchyard_core::{Command, RequestId, RuntimeHandle};

mod driver_card;

use crate::app_state::{Profiles, next_id};
use crate::theme::{MONO, Palette, palette};
use crate::ui::{self, Kind};

/// Connection types offered by the editor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnKind {
    /// PostgreSQL.
    Postgres,
    /// SQL Server.
    SqlServer,
    /// Cloudflare D1.
    D1,
    /// Snowflake.
    Snowflake,
    /// Oracle Database.
    Oracle,
    /// Redis.
    Redis,
    /// SSH Host.
    Ssh,
    /// SFTP over a Host.
    Sftp,
    /// FTP / FTPS.
    Ftp,
}

impl ConnKind {
    const ALL: [ConnKind; 9] = [
        ConnKind::Postgres,
        ConnKind::SqlServer,
        ConnKind::Oracle,
        ConnKind::Snowflake,
        ConnKind::D1,
        ConnKind::Redis,
        ConnKind::Ssh,
        ConnKind::Sftp,
        ConnKind::Ftp,
    ];

    fn badge(self) -> &'static str {
        match self {
            ConnKind::Postgres => "PG",
            ConnKind::SqlServer => "MS",
            ConnKind::D1 => "D1",
            ConnKind::Snowflake => "SF",
            ConnKind::Oracle => "OR",
            ConnKind::Redis => "RD",
            ConnKind::Ssh => "SSH",
            ConnKind::Sftp => "SFTP",
            ConnKind::Ftp => "FTP",
        }
    }

    fn label(self) -> &'static str {
        match self {
            ConnKind::Postgres => "PostgreSQL",
            ConnKind::SqlServer => "SQL Server",
            ConnKind::D1 => "Cloudflare D1",
            ConnKind::Snowflake => "Snowflake",
            ConnKind::Oracle => "Oracle",
            ConnKind::Redis => "Redis",
            ConnKind::Ssh => "SSH Host",
            ConnKind::Sftp => "SFTP",
            ConnKind::Ftp => "FTP / FTPS",
        }
    }

    fn is_db(self) -> bool {
        matches!(
            self,
            ConnKind::Postgres
                | ConnKind::SqlServer
                | ConnKind::Oracle
                | ConnKind::D1
                | ConnKind::Snowflake
                | ConnKind::Redis
        )
    }

    /// A SQL database (query history and coding agents apply).
    fn is_sql(self) -> bool {
        self.is_db() && self != ConnKind::Redis
    }

    fn sub(self) -> &'static str {
        match self {
            ConnKind::Postgres | ConnKind::SqlServer | ConnKind::Oracle => "Database",
            ConnKind::D1 => "SQLite over HTTPS",
            ConnKind::Snowflake => "Cloud warehouse",
            ConnKind::Redis => "Key-value store",
            ConnKind::Ssh => "Terminal + tunnels",
            ConnKind::Sftp => "Files over a Host",
            ConnKind::Ftp => "Files, own login",
        }
    }
}

/// Events for the workspace.
pub enum ConnEditorEvent {
    /// Close the dialog.
    Close,
    /// Show a toast.
    Toast(String),
}

#[derive(Clone, Debug, PartialEq)]
enum TestState {
    Idle,
    Testing(RequestId),
    Passed(String),
    Failed(String),
    Missing,
}

/// A select field: options (label, value) and the chosen index.
struct Select {
    options: Vec<(String, String)>,
    chosen: usize,
}

/// The editor.
pub struct ConnEditor {
    core: RuntimeHandle,
    profiles: Profiles,
    kind: ConnKind,
    existing_id: Option<ProfileId>,
    inputs: HashMap<&'static str, Entity<InputState>>,
    selects: HashMap<&'static str, Select>,
    open_select: Option<&'static str>,
    env: EnvironmentLabel,
    read_only: bool,
    history: bool,
    /// Coding agents (`swy mcp`) may use this connection.
    agents: bool,
    test: TestState,
    card: Option<driver_card::DriverCard>,
    driver_path: Entity<InputState>,
    components: Vec<Component>,
    error: Option<(Option<&'static str>, String)>,
    request: Option<RequestId>,
    connect_after: bool,
    /// A Host's port forwards.
    forwards: Vec<crate::forwards_editor::ForwardRow>,
    /// A Host's agent forwarding (`ssh -A`).
    forward_agent: bool,
    /// A Host's X11 forwarding (`ssh -X`).
    forward_x11: bool,
}

impl EventEmitter<ConnEditorEvent> for ConnEditor {}

fn text_input(
    window: &mut Window,
    cx: &mut Context<ConnEditor>,
    value: &str,
    placeholder: &str,
    masked: bool,
) -> Entity<InputState> {
    let v = value.to_owned();
    let ph = placeholder.to_owned();
    cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(ph)
            .masked(masked)
            .default_value(v)
    })
}

impl ConnEditor {
    /// A new editor; `existing` edits a saved profile.
    pub fn new(
        core: RuntimeHandle,
        profiles: Profiles,
        kind: ConnKind,
        existing: Option<Profile>,
        components: Vec<Component>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let kind = match &existing {
            Some(Profile::Db(d)) if d.engine == Engine::SqlServer => ConnKind::SqlServer,
            Some(Profile::Db(d)) if d.engine == Engine::D1 => ConnKind::D1,
            Some(Profile::Db(d)) if d.engine == Engine::Snowflake => ConnKind::Snowflake,
            Some(Profile::Db(d)) if d.engine == Engine::Oracle => ConnKind::Oracle,
            Some(Profile::Db(d)) if d.engine == Engine::Redis => ConnKind::Redis,
            Some(Profile::Db(_)) => ConnKind::Postgres,
            Some(Profile::Host(_)) => ConnKind::Ssh,
            Some(Profile::File(f)) => match f.protocol {
                FileProtocol::Sftp { .. } => ConnKind::Sftp,
                FileProtocol::Ftp { .. } => ConnKind::Ftp,
            },
            _ => kind,
        };
        let mut this = Self {
            core,
            profiles,
            kind,
            existing_id: existing.as_ref().map(|p| p.id().clone()),
            inputs: HashMap::new(),
            selects: HashMap::new(),
            open_select: None,
            env: existing
                .as_ref()
                .map(|p| p.environment())
                .unwrap_or(EnvironmentLabel::Development),
            read_only: matches!(&existing, Some(Profile::Db(d)) if d.read_only),
            history: !matches!(&existing, Some(Profile::Db(d)) if !d.history_enabled),
            agents: matches!(&existing, Some(Profile::Db(d)) if d.agent_access),
            test: TestState::Idle,
            card: None,
            driver_path: driver_card::path_input(window, cx),
            components,
            error: None,
            request: None,
            connect_after: true,
            forwards: Vec::new(),
            forward_agent: matches!(&existing, Some(Profile::Host(h)) if h.forward_agent),
            forward_x11: matches!(&existing, Some(Profile::Host(h)) if h.forward_x11),
        };
        this.build_fields(existing.as_ref(), window, cx);
        this
    }

    /// The pending save request.
    pub fn request(&self) -> Option<RequestId> {
        self.request
    }

    /// Whether to open the connection after saving.
    pub fn connect_after_save(&self) -> bool {
        self.connect_after && self.kind.is_db()
    }

    fn host_options(&self, none_label: &str) -> Vec<(String, String)> {
        let mut v = vec![(none_label.to_owned(), String::new())];
        v.extend(
            self.profiles
                .hosts()
                .map(|h| (format!("{} · SSH tunnel", h.name), h.id.0.clone())),
        );
        v
    }

    fn build_fields(
        &mut self,
        existing: Option<&Profile>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.inputs.clear();
        self.selects.clear();
        self.open_select = None;
        self.error = None;
        self.test = TestState::Idle;
        let mut add = |this: &mut Self, key: &'static str, value: &str, ph: &str, masked: bool| {
            let i = text_input(window, cx, value, ph, masked);
            this.inputs.insert(key, i);
        };
        let sel = |options: Vec<(String, String)>, value: &str| {
            let chosen = options.iter().position(|(_, v)| v == value).unwrap_or(0);
            Select { options, chosen }
        };
        match self.kind {
            ConnKind::Postgres | ConnKind::SqlServer | ConnKind::Oracle | ConnKind::Redis => {
                let engine = match self.kind {
                    ConnKind::Postgres => Engine::Postgres,
                    ConnKind::Oracle => Engine::Oracle,
                    ConnKind::Redis => Engine::Redis,
                    _ => Engine::SqlServer,
                };
                let d = match existing {
                    Some(Profile::Db(d)) => d.clone(),
                    _ => {
                        let mut d = DbConnection::new("", engine);
                        d.database = match engine {
                            Engine::Postgres => "postgres".into(),
                            Engine::Oracle => String::new(),
                            Engine::Redis => "0".into(),
                            _ => "master".into(),
                        };
                        if engine == Engine::Redis {
                            // Redis has no STARTTLS; a server either speaks TLS or not.
                            d.ssl_mode = SslMode::Disable;
                        }
                        d
                    }
                };
                add(
                    self,
                    "name",
                    &d.name,
                    match engine {
                        Engine::Postgres => "shop_prod",
                        Engine::Oracle => "erp",
                        Engine::Redis => "cache",
                        _ => "Reporting",
                    },
                    false,
                );
                add(self, "host", &d.server, "localhost", false);
                add(self, "port", &d.port.to_string(), "", false);
                add(
                    self,
                    "database",
                    &d.database,
                    if engine == Engine::Oracle {
                        "FREEPDB1"
                    } else {
                        ""
                    },
                    false,
                );
                add(
                    self,
                    "user",
                    &d.user,
                    if engine == Engine::Redis {
                        "default"
                    } else {
                        "app_ro"
                    },
                    false,
                );
                add(
                    self,
                    "password",
                    "",
                    if d.secret.is_some() {
                        "•••••••• (stored)"
                    } else {
                        ""
                    },
                    true,
                );
                let ssl = sel(
                    SslMode::ALL
                        .iter()
                        .map(|m| (m.label().to_owned(), m.label().to_owned()))
                        .collect(),
                    d.ssl_mode.label(),
                );
                self.selects.insert("ssl", ssl);
                self.selects.insert(
                    "via",
                    sel(
                        self.host_options("None — direct"),
                        d.via_host.as_ref().map_or("", |h| h.0.as_str()),
                    ),
                );
                if engine == Engine::SqlServer {
                    self.selects.insert(
                        "auth",
                        sel(
                            MSSQL_AUTH
                                .iter()
                                .map(|(key, m)| (m.label().into(), (*key).into()))
                                .collect(),
                            auth_key(d.auth),
                        ),
                    );
                    add(
                        self,
                        "tenant",
                        d.tenant.as_deref().unwrap_or_default(),
                        "contoso.onmicrosoft.com",
                        false,
                    );
                    add(
                        self,
                        "client_id",
                        d.entra_client_id.as_deref().unwrap_or_default(),
                        "Switchyard's own",
                        false,
                    );
                }
            }
            ConnKind::D1 => {
                let d = match existing {
                    Some(Profile::Db(d)) => d.clone(),
                    _ => {
                        let mut d = DbConnection::new("", Engine::D1);
                        d.server.clear();
                        d
                    }
                };
                add(self, "name", &d.name, "edge_prod", false);
                add(
                    self,
                    "server",
                    &d.server,
                    "0123456789abcdef0123456789abcdef",
                    false,
                );
                add(
                    self,
                    "database",
                    &d.database,
                    "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx",
                    false,
                );
                add(
                    self,
                    "password",
                    "",
                    if d.secret.is_some() {
                        "•••••••• (stored)"
                    } else {
                        "API token with D1 Read or Edit"
                    },
                    true,
                );
            }
            ConnKind::Snowflake => {
                let d = match existing {
                    Some(Profile::Db(d)) => d.clone(),
                    _ => {
                        let mut d = DbConnection::new("", Engine::Snowflake);
                        d.server.clear();
                        d.auth = DbAuthMethod::KeyPair;
                        d
                    }
                };
                let option = |k: &str| d.option(k).unwrap_or_default().to_owned();
                add(self, "name", &d.name, "analytics", false);
                add(self, "server", &d.server, "myorg-myaccount", false);
                add(self, "user", &d.user, "REPORTING_SVC", false);
                add(self, "database", &d.database, "ANALYTICS", false);
                add(self, "schema", &option("schema"), "PUBLIC", false);
                add(self, "warehouse", &option("warehouse"), "COMPUTE_WH", false);
                add(self, "role", &option("role"), "The user's default", false);
                add(
                    self,
                    "private_key_path",
                    &option("private_key_path"),
                    "~/.snowflake/rsa_key.p8",
                    false,
                );
                add(
                    self,
                    "password",
                    "",
                    if d.secret.is_some() {
                        "•••••••• (stored)"
                    } else {
                        ""
                    },
                    true,
                );
                self.selects.insert(
                    "auth",
                    sel(
                        SNOWFLAKE_AUTH
                            .iter()
                            .map(|(key, m)| (m.label().into(), (*key).into()))
                            .collect(),
                        snowflake_auth_key(d.auth),
                    ),
                );
            }
            ConnKind::Ssh => {
                let h = match existing {
                    Some(Profile::Host(h)) => h.clone(),
                    _ => Host::new("", "", std::env::var("USER").unwrap_or_default()),
                };
                add(self, "name", &h.name, "prod-db-01", false);
                add(self, "address", &h.address, "10.0.4.12", false);
                add(self, "port", &h.port.to_string(), "", false);
                add(self, "user", &h.user, "deploy", false);
                let key = match &h.auth {
                    SshAuth::PublicKey { key_path } => key_path.clone(),
                    _ => "~/.ssh/id_ed25519".into(),
                };
                add(self, "key", &key, "~/.ssh/id_ed25519", false);
                // Agents: which socket (1Password's or another) and which key.
                let agent_hint = match self
                    .components
                    .iter()
                    .find(|c| c.id == "ssh-agent")
                    .map(|c| &c.status)
                {
                    Some(ComponentStatus::Installed { location, .. })
                        if location.to_lowercase().contains("1password") =>
                    {
                        "Automatic · 1Password found".to_owned()
                    }
                    _ if cfg!(windows) => {
                        "Automatic · OpenSSH agent service, then Pageant".to_owned()
                    }
                    _ => "Automatic · SSH_AUTH_SOCK, then 1Password".to_owned(),
                };
                add(
                    self,
                    "agent_socket",
                    h.identity_agent.as_deref().unwrap_or_default(),
                    &agent_hint,
                    false,
                );
                add(
                    self,
                    "agent_key",
                    h.agent_key.as_deref().unwrap_or_default(),
                    "Any key the agent holds",
                    false,
                );
                add(
                    self,
                    "keepalive",
                    &h.keepalive_secs.to_string(),
                    "30",
                    false,
                );
                add(
                    self,
                    "x11_display",
                    h.x11_display.as_deref().unwrap_or_default(),
                    if cfg!(windows) {
                        "localhost:0"
                    } else {
                        "$DISPLAY"
                    },
                    false,
                );
                add(
                    self,
                    "password",
                    "",
                    if h.secret.is_some() {
                        "•••••••• (stored)"
                    } else {
                        ""
                    },
                    true,
                );
                let auth = match h.auth {
                    SshAuth::Password => "password",
                    SshAuth::PublicKey { .. } => "key",
                    SshAuth::KeyboardInteractive => "kbd",
                    SshAuth::Agent => "agent",
                };
                self.selects.insert(
                    "auth",
                    sel(
                        vec![
                            ("Public key".into(), "key".into()),
                            ("Password".into(), "password".into()),
                            ("Keyboard-interactive".into(), "kbd".into()),
                            ("SSH agent (1Password, OpenSSH)".into(), "agent".into()),
                        ],
                        if existing.is_some() { auth } else { "key" },
                    ),
                );
                let mut jumps = vec![("None".to_owned(), String::new())];
                jumps.extend(
                    self.profiles
                        .hosts()
                        .filter(|o| o.id != h.id)
                        .map(|o| (o.name.clone(), o.id.0.clone())),
                );
                self.selects.insert(
                    "jump",
                    sel(jumps, h.jump_hosts.first().map_or("", |j| j.0.as_str())),
                );
            }
            ConnKind::Sftp => {
                let (name, host, path) = match existing {
                    Some(Profile::File(f)) => (
                        f.name.clone(),
                        match &f.protocol {
                            FileProtocol::Sftp { host_id } => host_id.0.clone(),
                            _ => String::new(),
                        },
                        f.default_path.clone().unwrap_or_default(),
                    ),
                    _ => (String::new(), String::new(), String::new()),
                };
                add(self, "name", &name, "prod-db-01 files", false);
                add(self, "path", &path, "/var/www/shop", false);
                let mut hosts: Vec<(String, String)> = self
                    .profiles
                    .hosts()
                    .map(|h| (format!("{} · reuses SSH session", h.name), h.id.0.clone()))
                    .collect();
                if hosts.is_empty() {
                    hosts.push(("No Hosts saved yet".into(), String::new()));
                }
                self.selects.insert("host", sel(hosts, &host));
            }
            ConnKind::Ftp => {
                let (name, server, port, tls, mode, user) = match existing {
                    Some(Profile::File(FileConnection {
                        name,
                        protocol:
                            FileProtocol::Ftp {
                                server,
                                port,
                                tls,
                                mode,
                                user,
                            },
                        ..
                    })) => (
                        name.clone(),
                        server.clone(),
                        *port,
                        *tls,
                        *mode,
                        user.clone(),
                    ),
                    _ => (
                        String::new(),
                        String::new(),
                        21,
                        FtpTls::Explicit,
                        FtpMode::Passive,
                        String::new(),
                    ),
                };
                add(self, "name", &name, "assets.acme.dev", false);
                add(self, "server", &server, "ftp.assets.acme.dev", false);
                add(self, "port", &port.to_string(), "21", false);
                add(self, "user", &user, "deploy-assets", false);
                add(self, "password", "", "", true);
                self.selects.insert(
                    "tls",
                    sel(
                        vec![
                            ("Explicit (FTPES)".into(), "explicit".into()),
                            ("Implicit (FTPS)".into(), "implicit".into()),
                            ("None (plain FTP)".into(), "none".into()),
                        ],
                        match tls {
                            FtpTls::Explicit => "explicit",
                            FtpTls::Implicit => "implicit",
                            FtpTls::None => "none",
                        },
                    ),
                );
                self.selects.insert(
                    "mode",
                    sel(
                        vec![
                            ("Passive".into(), "passive".into()),
                            ("Active".into(), "active".into()),
                        ],
                        if mode == FtpMode::Active {
                            "active"
                        } else {
                            "passive"
                        },
                    ),
                );
            }
        }
        if self.kind.is_sql() {
            let chosen = match existing {
                Some(Profile::Db(d)) => d.assistant_agent.clone().unwrap_or_default(),
                _ => String::new(),
            };
            let mut options = vec![("Default (Settings → Assistant)".to_owned(), String::new())];
            options.extend(
                crate::assistant_settings::AGENTS
                    .into_iter()
                    .map(|k| (k.display_name().to_owned(), k.id().to_owned())),
            );
            self.selects.insert("assistant", sel(options, &chosen));
        }
        self.forwards = match existing {
            Some(Profile::Host(h)) if self.kind == ConnKind::Ssh => h
                .forwards
                .iter()
                .map(|f| crate::forwards_editor::ForwardRow::new(f, window, cx))
                .collect(),
            _ => Vec::new(),
        };
    }

    /// A port forward row changed.
    fn forward_action(
        &mut self,
        action: crate::forwards_editor::RowAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::forwards_editor::{ForwardRow, RowAction};
        use switchyard_core::store::{ForwardDirection, PortForward};
        match action {
            RowAction::Add => {
                let f = PortForward::new(ForwardDirection::Local);
                self.forwards.push(ForwardRow::new(&f, window, cx));
            }
            RowAction::Direction(i, d) => {
                if let Some(r) = self.forwards.get_mut(i) {
                    r.set_direction(d);
                }
            }
            RowAction::ToggleAuto(i) => {
                if let Some(r) = self.forwards.get_mut(i) {
                    r.toggle_auto();
                }
            }
            RowAction::Remove(i) => {
                if i < self.forwards.len() {
                    self.forwards.remove(i);
                }
            }
        }
        cx.notify();
    }

    fn value(&self, key: &str, cx: &Context<Self>) -> String {
        self.inputs
            .get(key)
            .map(|i| i.read(cx).value().trim().to_owned())
            .unwrap_or_default()
    }

    fn chosen(&self, key: &str) -> String {
        self.selects
            .get(key)
            .and_then(|s| s.options.get(s.chosen))
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    fn secret(&self, cx: &Context<Self>) -> Option<SecretString> {
        let pw = self
            .inputs
            .get("password")
            .map(|i| i.read(cx).value().to_string())
            .unwrap_or_default();
        (!pw.is_empty()).then(|| SecretString::from(pw))
    }

    fn port(&self, cx: &Context<Self>, default: u16) -> Result<u16, String> {
        let v = self.value("port", cx);
        if v.is_empty() {
            return Ok(default);
        }
        v.parse::<u16>()
            .ok()
            .filter(|p| *p > 0)
            .ok_or_else(|| "Port must be 1–65535".to_owned())
    }

    fn build(&self, cx: &Context<Self>) -> Result<Profile, (Option<&'static str>, String)> {
        let id = self.existing_id.clone().unwrap_or_default();
        let existing = self
            .existing_id
            .as_ref()
            .and_then(|i| self.profiles.all.iter().find(|p| p.id() == i));
        Ok(match self.kind {
            ConnKind::Postgres | ConnKind::SqlServer | ConnKind::Oracle | ConnKind::Redis => {
                let engine = match self.kind {
                    ConnKind::Postgres => Engine::Postgres,
                    ConnKind::Oracle => Engine::Oracle,
                    ConnKind::Redis => Engine::Redis,
                    _ => Engine::SqlServer,
                };
                let mut d = match existing {
                    Some(Profile::Db(d)) => d.clone(),
                    _ => DbConnection::new("", engine),
                };
                d.id = id;
                d.engine = engine;
                d.name = self.value("name", cx);
                d.server = self.value("host", cx);
                d.port = self
                    .port(cx, engine.default_port())
                    .map_err(|m| (Some("port"), m))?;
                d.database = self.value("database", cx);
                d.user = self.value("user", cx);
                d.ssl_mode = SslMode::ALL
                    .into_iter()
                    .find(|m| m.label() == self.chosen("ssl"))
                    .unwrap_or_default();
                let via = self.chosen("via");
                d.via_host = (!via.is_empty()).then_some(ProfileId(via));
                d.auth = auth_from_key(&self.chosen("auth"));
                let opt = |v: String| (!v.is_empty()).then_some(v);
                if engine == Engine::SqlServer {
                    d.tenant = opt(self.value("tenant", cx));
                    d.entra_client_id = opt(self.value("client_id", cx));
                }
                d.environment = self.env;
                d.read_only = self.read_only;
                d.history_enabled = self.history;
                d.agent_access = self.agents && engine.is_sql();
                d.assistant_agent = Some(self.chosen("assistant")).filter(|a| !a.is_empty());
                Profile::Db(d)
            }
            ConnKind::D1 => {
                let mut d = match existing {
                    Some(Profile::Db(d)) => d.clone(),
                    _ => DbConnection::new("", Engine::D1),
                };
                d.id = id;
                d.engine = Engine::D1;
                d.name = self.value("name", cx);
                d.server = self.value("server", cx);
                d.port = Engine::D1.default_port();
                d.database = self.value("database", cx);
                d.user.clear();
                d.via_host = None;
                d.environment = self.env;
                d.read_only = self.read_only;
                d.history_enabled = self.history;
                d.agent_access = self.agents;
                d.assistant_agent = Some(self.chosen("assistant")).filter(|a| !a.is_empty());
                Profile::Db(d)
            }
            ConnKind::Snowflake => {
                let mut d = match existing {
                    Some(Profile::Db(d)) => d.clone(),
                    _ => DbConnection::new("", Engine::Snowflake),
                };
                d.id = id;
                d.engine = Engine::Snowflake;
                d.name = self.value("name", cx);
                d.server = self.value("server", cx);
                d.port = Engine::Snowflake.default_port();
                d.database = self.value("database", cx);
                d.user = self.value("user", cx);
                d.auth = snowflake_auth_from_key(&self.chosen("auth"));
                d.via_host = None;
                for key in ["schema", "warehouse", "role", "private_key_path"] {
                    let v = self.value(key, cx);
                    if v.is_empty()
                        || (key == "private_key_path" && d.auth != DbAuthMethod::KeyPair)
                    {
                        d.options.remove(key);
                    } else {
                        d.options.insert(key.to_owned(), v);
                    }
                }
                d.environment = self.env;
                d.read_only = self.read_only;
                d.history_enabled = self.history;
                d.agent_access = self.agents;
                d.assistant_agent = Some(self.chosen("assistant")).filter(|a| !a.is_empty());
                Profile::Db(d)
            }
            ConnKind::Ssh => {
                let mut h = match existing {
                    Some(Profile::Host(h)) => h.clone(),
                    _ => Host::new("", "", ""),
                };
                h.id = id;
                h.name = self.value("name", cx);
                h.address = self.value("address", cx);
                h.port = self.port(cx, 22).map_err(|m| (Some("port"), m))?;
                h.user = self.value("user", cx);
                h.auth = match self.chosen("auth").as_str() {
                    "password" => SshAuth::Password,
                    "kbd" => SshAuth::KeyboardInteractive,
                    "agent" => SshAuth::Agent,
                    _ => SshAuth::PublicKey {
                        key_path: self.value("key", cx),
                    },
                };
                let opt = |v: String| (!v.is_empty()).then_some(v);
                h.identity_agent = opt(self.value("agent_socket", cx));
                h.agent_key = opt(self.value("agent_key", cx));
                h.keepalive_secs = self.value("keepalive", cx).parse().unwrap_or(30);
                h.forward_agent = self.forward_agent;
                h.forward_x11 = self.forward_x11;
                h.x11_display = opt(self.value("x11_display", cx));
                let jump = self.chosen("jump");
                h.jump_hosts = if jump.is_empty() {
                    vec![]
                } else {
                    vec![ProfileId(jump)]
                };
                h.environment = self.env;
                h.forwards = self
                    .forwards
                    .iter()
                    .map(|r| r.read(cx))
                    .collect::<Result<_, _>>()
                    .map_err(|m| (None, m))?;
                Profile::Host(h)
            }
            ConnKind::Sftp => {
                let host = self.chosen("host");
                if host.is_empty() {
                    return Err((
                        Some("host"),
                        "Save a Host first; SFTP reuses its SSH session".into(),
                    ));
                }
                let path = self.value("path", cx);
                Profile::File(FileConnection {
                    id,
                    name: self.value("name", cx),
                    protocol: FileProtocol::Sftp {
                        host_id: ProfileId(host),
                    },
                    default_path: (!path.is_empty()).then_some(path),
                    environment: self.env,
                    folder: None,
                    secret: existing.and_then(|p| p.secret().cloned()),
                })
            }
            ConnKind::Ftp => Profile::File(FileConnection {
                id,
                name: self.value("name", cx),
                protocol: FileProtocol::Ftp {
                    server: self.value("server", cx),
                    port: self.port(cx, 21).map_err(|m| (Some("port"), m))?,
                    tls: match self.chosen("tls").as_str() {
                        "implicit" => FtpTls::Implicit,
                        "none" => FtpTls::None,
                        _ => FtpTls::Explicit,
                    },
                    mode: if self.chosen("mode") == "active" {
                        FtpMode::Active
                    } else {
                        FtpMode::Passive
                    },
                    user: self.value("user", cx),
                },
                default_path: None,
                environment: self.env,
                folder: None,
                secret: existing.and_then(|p| p.secret().cloned()),
            }),
        })
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        match self.build(cx) {
            Ok(profile) => {
                if let Err(v) = profile.validate() {
                    self.error = Some((Some(v.field), v.message));
                    cx.notify();
                    return;
                }
                let request = next_id();
                self.request = Some(request);
                self.error = None;
                self.core.send(Command::SaveProfile {
                    request,
                    profile,
                    secret: self.secret(cx),
                });
            }
            Err((field, msg)) => self.error = Some((field, msg)),
        }
        cx.notify();
    }

    fn test(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        match (self.kind, self.build(cx)) {
            (ConnKind::Oracle, Ok(Profile::Db(_))) if self.oracle_client_missing() => {
                self.test = TestState::Missing;
                if self.card.as_ref().is_none_or(|c| c.id != ORACLE_CLIENT) {
                    self.card = Some(driver_card::DriverCard::new(ORACLE_CLIENT));
                }
            }
            (
                ConnKind::Postgres
                | ConnKind::Oracle
                | ConnKind::D1
                | ConnKind::Snowflake
                | ConnKind::Redis,
                Ok(Profile::Db(d)),
            ) => {
                let request = next_id();
                self.test = TestState::Testing(request);
                self.core.send(Command::TestConnection {
                    request,
                    connection: d,
                    secret: self.secret(cx),
                });
            }
            (ConnKind::SqlServer, Ok(Profile::Db(d))) => {
                let gss_missing = self
                    .components
                    .iter()
                    .any(|c| c.id == "gssapi" && !c.status.is_installed());
                if d.auth == DbAuthMethod::Integrated && gss_missing {
                    self.test = TestState::Missing;
                    if self.card.as_ref().is_none_or(|c| c.id != "gssapi") {
                        self.card = Some(driver_card::DriverCard::new("gssapi"));
                    }
                } else {
                    let request = next_id();
                    self.test = TestState::Testing(request);
                    self.core.send(Command::TestConnection {
                        request,
                        connection: d,
                        secret: self.secret(cx),
                    });
                }
            }
            (ConnKind::Ssh, Ok(Profile::Host(host))) => {
                let request = next_id();
                self.test = TestState::Testing(request);
                self.core.send(Command::TestHost {
                    request,
                    host,
                    secret: self.secret(cx),
                });
            }
            (ConnKind::Sftp, Ok(_)) => {
                self.test = TestState::Failed("SFTP ships in milestone M4 (russh-sftp)".into())
            }
            (ConnKind::Ftp, Ok(_)) => {
                self.test = TestState::Failed("FTP/FTPS ships in milestone M4 (suppaftp)".into())
            }
            (_, Err((f, m))) => self.error = Some((f, m)),
            _ => {}
        }
        cx.notify();
    }

    /// A test result arrived.
    pub fn on_test_result(
        &mut self,
        request: RequestId,
        result: Result<String, String>,
        cx: &mut Context<Self>,
    ) {
        if self.test == TestState::Testing(request) {
            self.test = match result {
                Ok(s) => TestState::Passed(s),
                Err(e) => TestState::Failed(e),
            };
            cx.notify();
        }
    }

    /// A save failed.
    pub fn set_error(
        &mut self,
        field: Option<&'static str>,
        message: String,
        cx: &mut Context<Self>,
    ) {
        self.request = None;
        self.error = Some((field, message));
        cx.notify();
    }

    fn oracle_client_missing(&self) -> bool {
        self.components
            .iter()
            .any(|c| c.id == ORACLE_CLIENT && !c.status.is_installed())
    }

    fn set_kind(&mut self, kind: ConnKind, window: &mut Window, cx: &mut Context<Self>) {
        if self.existing_id.is_some() || kind == self.kind {
            return;
        }
        self.kind = kind;
        self.build_fields(None, window, cx);
        cx.notify();
    }

    #[allow(clippy::too_many_arguments)]
    fn field(
        &self,
        key: &'static str,
        label: &str,
        span: u16,
        mono: bool,
        hint: Option<&str>,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let err = self
            .error
            .as_ref()
            .filter(|(f, _)| *f == Some(key))
            .map(|(_, m)| m.clone());
        let body: AnyElement = if let Some(input) = self.inputs.get(key) {
            div()
                .h(px(28.))
                .flex()
                .items_center()
                .px(px(9.))
                .border_1()
                .border_color(if err.is_some() { p.prod } else { p.bd2 })
                .rounded(px(6.))
                .bg(p.bg)
                .text_size(px(12.5))
                .when(mono, |d| d.font_family(MONO))
                .child(Input::new(input).appearance(false).text_size(px(12.5)))
                .into_any_element()
        } else if let Some(sel) = self.selects.get(key) {
            let label = sel
                .options
                .get(sel.chosen)
                .map(|(l, _)| l.clone())
                .unwrap_or_default();
            let open = self.open_select == Some(key);
            let options: Vec<(usize, String)> = sel
                .options
                .iter()
                .enumerate()
                .map(|(i, (l, _))| (i, l.clone()))
                .collect();
            let chosen = sel.chosen;
            div()
                .relative()
                .child(
                    div()
                        .id(SharedString::from(format!("sel-{key}")))
                        .h(px(28.))
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .px(px(9.))
                        .border_1()
                        .border_color(if open { p.acc } else { p.bd2 })
                        .rounded(px(6.))
                        .bg(p.bg)
                        .text_size(px(12.5))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_select = if this.open_select == Some(key) {
                                None
                            } else {
                                Some(key)
                            };
                            cx.notify();
                        }))
                        .child(div().flex_1().truncate().child(label))
                        .child(div().text_color(p.fg3).text_size(px(10.)).child("▾")),
                )
                .when(open, |d| {
                    d.child(deferred(
                        div()
                            .id(SharedString::from(format!("sel-menu-{key}")))
                            .absolute()
                            .top(px(30.))
                            .left_0()
                            .right_0()
                            .p(px(4.))
                            .bg(p.elev)
                            .rounded(px(7.))
                            .shadow(ui::shadow(p))
                            .occlude()
                            .children(options.into_iter().map(|(i, l)| {
                                div()
                                    .id(SharedString::from(format!("opt-{key}-{i}")))
                                    .h(px(26.))
                                    .flex()
                                    .items_center()
                                    .px(px(8.))
                                    .rounded(px(4.))
                                    .text_size(px(12.5))
                                    .when(i == chosen, |d| d.bg(p.sel))
                                    .hover(|s| s.bg(p.sel))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if let Some(s) = this.selects.get_mut(key) {
                                            s.chosen = i;
                                        }
                                        this.open_select = None;
                                        this.test = TestState::Idle;
                                        cx.notify();
                                    }))
                                    .child(l)
                            })),
                    ))
                })
                .into_any_element()
        } else {
            div().into_any_element()
        };
        div()
            .col_span(span)
            .flex()
            .flex_col()
            .gap(px(5.))
            .min_w_0()
            .child(
                div()
                    .text_size(px(11.5))
                    .text_color(p.fg2)
                    .font_weight(FontWeight::MEDIUM)
                    .child(label.to_owned()),
            )
            .child(body)
            .when_some(err.or(hint.map(str::to_owned)), |d, h| {
                let is_err = self.error.as_ref().is_some_and(|(f, _)| *f == Some(key));
                d.child(
                    div()
                        .text_size(px(11.))
                        .text_color(if is_err { p.prod } else { p.fg3 })
                        .child(h),
                )
            })
            .into_any_element()
    }

    fn fields(&self, p: &Palette, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut v = Vec::new();
        match self.kind {
            ConnKind::Redis => {
                v.push(self.field("name", "Name", 6, false, None, p, cx));
                v.push(self.field("host", "Host", 4, true, None, p, cx));
                v.push(self.field("port", "Port", 2, true, None, p, cx));
                v.push(self.field(
                    "database",
                    "Database",
                    2,
                    false,
                    Some("Number, 0 by default"),
                    p,
                    cx,
                ));
                v.push(self.field(
                    "user",
                    "ACL user",
                    2,
                    false,
                    Some("Empty for the default user"),
                    p,
                    cx,
                ));
                v.push(self.field(
                    "password",
                    "Password",
                    2,
                    false,
                    Some("Stored in the OS keychain"),
                    p,
                    cx,
                ));
                v.push(self.field(
                    "ssl",
                    "TLS",
                    3,
                    false,
                    Some("Require for TLS ports (cloud Redis); Prefer means plain TCP"),
                    p,
                    cx,
                ));
                v.push(self.field(
                    "via",
                    "Connect via Host",
                    3,
                    false,
                    Some("Opens an ephemeral local port automatically"),
                    p,
                    cx,
                ));
            }
            ConnKind::Postgres => {
                v.push(self.field("name", "Name", 6, false, None, p, cx));
                v.push(self.field("host", "Host", 4, true, None, p, cx));
                v.push(self.field("port", "Port", 2, true, None, p, cx));
                v.push(self.field("database", "Database", 3, false, None, p, cx));
                v.push(self.field("user", "User", 3, false, None, p, cx));
                v.push(self.field(
                    "password",
                    "Password",
                    3,
                    false,
                    Some("Stored in the OS keychain"),
                    p,
                    cx,
                ));
                v.push(self.field("ssl", "SSL mode", 3, false, None, p, cx));
                v.push(self.field(
                    "via",
                    "Connect via Host",
                    6,
                    false,
                    Some("Opens an ephemeral local port automatically"),
                    p,
                    cx,
                ));
            }
            ConnKind::SqlServer => {
                let auth = auth_from_key(&self.chosen("auth"));
                let server_hint = (auth == DbAuthMethod::Integrated).then_some(
                    "Full host name (db.corp.example.com): Kerberos looks up MSSQLSvc/<server>:<port>",
                );
                v.push(self.field("name", "Name", 6, false, None, p, cx));
                v.push(self.field("host", "Server", 4, true, server_hint, p, cx));
                v.push(self.field("port", "Port", 2, true, None, p, cx));
                v.push(self.field("database", "Database", 3, false, None, p, cx));
                v.push(self.field(
                    "auth",
                    "Authentication",
                    3,
                    false,
                    (auth == DbAuthMethod::Integrated).then_some(if cfg!(windows) {
                        "Signs in as the current Windows user"
                    } else {
                        "Uses your Kerberos ticket (kinit or your desktop's sign-in)"
                    }),
                    p,
                    cx,
                ));
                match auth {
                    DbAuthMethod::Integrated
                    | DbAuthMethod::KeyPair
                    | DbAuthMethod::AccessToken => {}
                    DbAuthMethod::WindowsPassword => {
                        v.push(self.field(
                            "user",
                            "Windows account",
                            3,
                            false,
                            Some("DOMAIN\\user"),
                            p,
                            cx,
                        ));
                        v.push(self.field(
                            "password",
                            "Password",
                            3,
                            false,
                            Some("Stored in the OS keychain"),
                            p,
                            cx,
                        ));
                    }
                    DbAuthMethod::EntraInteractive | DbAuthMethod::EntraDeviceCode => {
                        v.push(self.field(
                            "user",
                            "Account (optional)",
                            3,
                            false,
                            Some("Pre-fills the Microsoft sign-in, e.g. name@company.com"),
                            p,
                            cx,
                        ));
                        v.push(self.field(
                            "tenant",
                            "Tenant (optional)",
                            3,
                            true,
                            Some("Directory id or domain; blank = any work or school account"),
                            p,
                            cx,
                        ));
                        v.push(self.field(
                            "client_id",
                            "Application (client) id",
                            3,
                            true,
                            Some("Leave empty to sign in as Microsoft's SQL client (like SSMS)"),
                            p,
                            cx,
                        ));
                    }
                    DbAuthMethod::EntraServicePrincipal => {
                        v.push(self.field("user", "Application (client) id", 3, true, None, p, cx));
                        v.push(self.field(
                            "password",
                            "Client secret",
                            3,
                            false,
                            Some("Stored in the OS keychain"),
                            p,
                            cx,
                        ));
                        v.push(self.field("tenant", "Tenant", 3, true, None, p, cx));
                    }
                    DbAuthMethod::Password | DbAuthMethod::EntraPassword => {
                        let user = if auth == DbAuthMethod::EntraPassword {
                            "Microsoft account"
                        } else {
                            "User"
                        };
                        v.push(self.field("user", user, 3, false, None, p, cx));
                        v.push(self.field(
                            "password",
                            "Password",
                            3,
                            false,
                            Some("Stored in the OS keychain"),
                            p,
                            cx,
                        ));
                        if auth == DbAuthMethod::EntraPassword {
                            v.push(self.field(
                                "tenant",
                                "Tenant (optional)",
                                3,
                                true,
                                Some("No MFA with this method; use browser sign-in for MFA"),
                                p,
                                cx,
                            ));
                        }
                    }
                }
                v.push(self.field("ssl", "Encrypt", 3, false, None, p, cx));
                v.push(self.field("via", "Connect via Host", 3, false, None, p, cx));
            }
            ConnKind::D1 => {
                v.push(self.field("name", "Name", 6, false, None, p, cx));
                v.push(self.field(
                    "server",
                    "Account ID",
                    6,
                    true,
                    Some("Cloudflare dashboard → Workers & Pages overview (right column), or the dashboard URL"),
                    p,
                    cx,
                ));
                v.push(self.field(
                    "database",
                    "Database ID",
                    6,
                    true,
                    Some("From `wrangler d1 list` or the D1 database page"),
                    p,
                    cx,
                ));
                v.push(self.field(
                    "password",
                    "API token",
                    6,
                    false,
                    Some("Stored in the OS keychain · needs the D1 Read or D1 Edit permission"),
                    p,
                    cx,
                ));
            }
            ConnKind::Oracle => {
                v.push(self.field("name", "Name", 6, false, None, p, cx));
                v.push(self.field("host", "Host", 4, true, None, p, cx));
                v.push(self.field("port", "Port", 2, true, None, p, cx));
                v.push(self.field(
                    "database",
                    "Service name",
                    6,
                    true,
                    Some("FREEPDB1, ORCLPDB1… or, with Host empty, a TNS alias or descriptor"),
                    p,
                    cx,
                ));
                v.push(self.field("user", "User", 3, false, None, p, cx));
                v.push(self.field(
                    "password",
                    "Password",
                    3,
                    false,
                    Some("Stored in the OS keychain"),
                    p,
                    cx,
                ));
                v.push(self.field(
                    "via",
                    "Connect via Host",
                    6,
                    false,
                    Some("Opens an ephemeral local port automatically"),
                    p,
                    cx,
                ));
            }
            ConnKind::Snowflake => {
                let auth = snowflake_auth_from_key(&self.chosen("auth"));
                v.push(self.field("name", "Name", 6, false, None, p, cx));
                v.push(self.field(
                    "server",
                    "Account identifier",
                    6,
                    true,
                    Some("orgname-accountname, or the host before .snowflakecomputing.com"),
                    p,
                    cx,
                ));
                v.push(self.field("user", "User", 3, false, None, p, cx));
                v.push(self.field("auth", "Authentication", 3, false, None, p, cx));
                if auth == DbAuthMethod::KeyPair {
                    v.push(self.field(
                        "private_key_path",
                        "Private key file",
                        6,
                        true,
                        Some("PKCS#8 .p8 (or PKCS#1) PEM; read in place, never copied"),
                        p,
                        cx,
                    ));
                    v.push(self.field(
                        "password",
                        "Key passphrase (optional)",
                        6,
                        false,
                        Some("Only for an encrypted key · stored in the OS keychain"),
                        p,
                        cx,
                    ));
                } else {
                    v.push(self.field(
                        "password",
                        "Programmatic access token",
                        6,
                        false,
                        Some("Snowsight → your profile → Programmatic access tokens · stored in the OS keychain"),
                        p,
                        cx,
                    ));
                }
                v.push(self.field("warehouse", "Warehouse", 3, true, None, p, cx));
                v.push(self.field("role", "Role", 3, true, None, p, cx));
                v.push(self.field("database", "Database", 3, true, None, p, cx));
                v.push(self.field("schema", "Schema", 3, true, None, p, cx));
            }
            ConnKind::Ssh => {
                v.push(self.field("name", "Name", 6, false, None, p, cx));
                v.push(self.field("address", "Address", 4, true, None, p, cx));
                v.push(self.field("port", "Port", 2, true, None, p, cx));
                v.push(self.field("user", "User", 3, false, None, p, cx));
                v.push(self.field("auth", "Auth", 3, false, None, p, cx));
                match self.chosen("auth").as_str() {
                    "agent" => {
                        v.push(self.field(
                            "agent_socket",
                            "Agent socket",
                            3,
                            true,
                            Some(if cfg!(windows) {
                                "A named pipe (\\\\.\\pipe\\…), or pageant for PuTTY's Pageant"
                            } else {
                                "1Password: ~/.1password/agent.sock (macOS: the Group Containers path)"
                            }),
                            p,
                            cx,
                        ));
                        v.push(self.field(
                            "agent_key",
                            "Public key (optional)",
                            3,
                            true,
                            Some("Offer only this key, e.g. ~/.ssh/prod.pub; 1Password asks you to approve"),
                            p,
                            cx,
                        ));
                    }
                    "key" => {
                        v.push(self.field(
                            "key",
                            "Key file",
                            6,
                            true,
                            Some("Read in place — never copied"),
                            p,
                            cx,
                        ));
                    }
                    "password" => {
                        v.push(self.field(
                            "password",
                            "Password",
                            6,
                            false,
                            Some("Stored in the OS keychain"),
                            p,
                            cx,
                        ));
                    }
                    _ => {}
                }
                v.push(self.field("jump", "Jump host", 4, true, None, p, cx));
                v.push(self.field("keepalive", "Keepalive (s)", 2, false, None, p, cx));
                if self.forward_x11 {
                    v.push(self.field(
                        "x11_display",
                        "X display",
                        3,
                        true,
                        Some("Empty: DISPLAY (Windows: VcXsrv/X410 on localhost:0)"),
                        p,
                        cx,
                    ));
                }
            }
            ConnKind::Sftp => {
                v.push(self.field("name", "Name", 6, false, None, p, cx));
                v.push(self.field("host", "Host", 6, false, Some("No second login"), p, cx));
                v.push(self.field("path", "Default remote path", 6, true, None, p, cx));
            }
            ConnKind::Ftp => {
                v.push(self.field("name", "Name", 6, false, None, p, cx));
                v.push(self.field("server", "Server", 4, true, None, p, cx));
                v.push(self.field("port", "Port", 2, true, None, p, cx));
                v.push(self.field("tls", "TLS", 3, false, None, p, cx));
                v.push(self.field("mode", "Mode", 3, false, None, p, cx));
                v.push(self.field("user", "User", 3, false, None, p, cx));
                v.push(self.field("password", "Password", 3, false, None, p, cx));
            }
        }
        v
    }
}

impl Render for ConnEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let title = if self.existing_id.is_some() {
            format!("Edit {}", self.kind.label())
        } else if self.kind == ConnKind::Ssh {
            "New host".into()
        } else {
            "New connection".into()
        };
        let fields = self.fields(&p, cx);
        let assistant_field = (self.kind.is_sql() && self.agents).then(|| {
            div().w(px(300.)).child(self.field(
                "assistant",
                "Assistant CLI",
                6,
                false,
                None,
                &p,
                cx,
            ))
        });
        let forwards = (self.kind == ConnKind::Ssh).then(|| {
            crate::forwards_editor::render(&self.forwards, &p, cx, |this: &mut Self, a, w, cx| {
                this.forward_action(a, w, cx)
            })
        });
        let (test_label, test_color, testing) = match &self.test {
            TestState::Idle => (String::new(), gpui_kit::transparent_black(), false),
            TestState::Testing(_) => ("Connecting…".into(), p.acc, true),
            TestState::Passed(s) => (s.clone(), p.dev, false),
            TestState::Failed(s) => (s.clone(), p.prod, false),
            TestState::Missing => ("A required component is missing".into(), p.stg, false),
        };
        let general_error = self
            .error
            .as_ref()
            .filter(|(f, _)| {
                f.is_none()
                    || !self.inputs.contains_key(f.unwrap_or(""))
                        && !self.selects.contains_key(f.unwrap_or(""))
            })
            .map(|(_, m)| m.clone());
        let is_db = self.kind.is_db();
        let _ = window;
        div()
            .id("conn-editor")
            .w(px(780.))
            .max_h(px(640.))
            .flex()
            .flex_col()
            .bg(p.elev)
            .rounded(px(10.))
            .shadow(ui::shadow(&p))
            .overflow_hidden()
            .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| {
                cx.stop_propagation()
            })
            .child(
                div()
                    .h(px(46.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .px(px(16.))
                    .border_b_1()
                    .border_color(p.bd)
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_size(px(14.))
                            .child(title),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("ce-close")
                            .px(px(8.))
                            .py(px(2.))
                            .rounded(px(4.))
                            .text_color(p.fg3)
                            .hover(|s| s.bg(p.hover))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(ConnEditorEvent::Close)))
                            .child("×"),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(
                        div()
                            .w(px(190.))
                            .flex_none()
                            .p(px(8.))
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .bg(p.panel)
                            .border_r_1()
                            .border_color(p.bd)
                            .children(ConnKind::ALL.iter().map(|k| {
                                let k = *k;
                                let active = k == self.kind;
                                let disabled = self.existing_id.is_some() && !active;
                                div()
                                    .id(SharedString::from(format!("ctype-{}", k.badge())))
                                    .flex()
                                    .items_center()
                                    .gap(px(9.))
                                    .px(px(8.))
                                    .py(px(7.))
                                    .rounded(px(6.))
                                    .when(active, |d| d.bg(p.sel))
                                    .when(!active && !disabled, |d| d.hover(|s| s.bg(p.hover)))
                                    .when(disabled, |d| d.opacity(0.4))
                                    .on_click(
                                        cx.listener(move |this, _, w, cx| this.set_kind(k, w, cx)),
                                    )
                                    .child(
                                        div()
                                            .w(px(32.))
                                            .flex_none()
                                            .flex()
                                            .justify_center()
                                            .border_1()
                                            .border_color(p.bd2)
                                            .rounded(px(3.))
                                            .text_color(p.fg2)
                                            .font_family(MONO)
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_size(px(8.5))
                                            .line_height(px(16.))
                                            .child(k.badge()),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .min_w_0()
                                            .child(
                                                div()
                                                    .text_size(px(12.5))
                                                    .font_weight(FontWeight::MEDIUM)
                                                    .whitespace_nowrap()
                                                    .child(k.label()),
                                            )
                                            .child(
                                                div()
                                                    .text_size(px(11.))
                                                    .text_color(p.fg3)
                                                    .truncate()
                                                    .child(k.sub()),
                                            ),
                                    )
                            })),
                    )
                    .child(
                        div()
                            .id("ce-form")
                            .flex_1()
                            .min_w_0()
                            .overflow_y_scroll()
                            .px(px(18.))
                            .py(px(16.))
                            .flex()
                            .flex_col()
                            .gap(px(14.))
                            .child(div().grid().grid_cols(6).gap(px(12.)).children(fields))
                            .children(forwards)
                            .when(self.kind == ConnKind::Ssh, |d| {
                                d.child(
                                    ui::checkbox(
                                        "fwd-agent",
                                        self.forward_agent,
                                        "Forward SSH agent (ssh -A): the Host can use your agent's keys while you are connected; only for Hosts you trust",
                                        &p,
                                    )
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.forward_agent = !this.forward_agent;
                                        cx.notify();
                                    })),
                                )
                                .child(
                                    ui::checkbox(
                                        "fwd-x11",
                                        self.forward_x11,
                                        "Forward X11 (ssh -X): graphical programs on the Host open windows here",
                                        &p,
                                    )
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.forward_x11 = !this.forward_x11;
                                        cx.notify();
                                    })),
                                )
                            })
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(6.))
                                    .child(
                                        div()
                                            .text_size(px(11.5))
                                            .text_color(p.fg2)
                                            .font_weight(FontWeight::MEDIUM)
                                            .child("Environment"),
                                    )
                                    .child(div().flex().gap(px(6.)).children(
                                        EnvironmentLabel::ALL.iter().map(|e| {
                                            let e = *e;
                                            let active = e == self.env;
                                            div()
                                                .id(SharedString::from(format!(
                                                    "env-{}",
                                                    e.badge()
                                                )))
                                                .flex_1()
                                                .h(px(30.))
                                                .flex()
                                                .items_center()
                                                .gap(px(8.))
                                                .px(px(10.))
                                                .border_1()
                                                .border_color(if active { p.env(e) } else { p.bd2 })
                                                .rounded(px(6.))
                                                .bg(if active { p.env_bg(e) } else { p.bg })
                                                .text_size(px(12.5))
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.env = e;
                                                    cx.notify();
                                                }))
                                                .child(ui::dot(p.env(e), 8.))
                                                .child(e.name())
                                        }),
                                    ))
                                    .when(self.env == EnvironmentLabel::Production && is_db, |d| {
                                        d.child(
                                            div()
                                                .id("lock-ro")
                                                .flex()
                                                .items_center()
                                                .gap(px(8.))
                                                .text_size(px(11.5))
                                                .text_color(p.fg2)
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.read_only = !this.read_only;
                                                    cx.notify();
                                                }))
                                                .child(
                                                    div()
                                                        .size(px(14.))
                                                        .flex()
                                                        .items_center()
                                                        .justify_center()
                                                        .border_1()
                                                        .border_color(if self.read_only {
                                                            p.acc
                                                        } else {
                                                            p.bd2
                                                        })
                                                        .rounded(px(3.))
                                                        .bg(if self.read_only {
                                                            p.acc
                                                        } else {
                                                            p.surface
                                                        })
                                                        .text_color(p.acc_fg)
                                                        .text_size(px(10.))
                                                        .child(if self.read_only {
                                                            "✓"
                                                        } else {
                                                            ""
                                                        }),
                                                )
                                                .child(
                                                    "Destructive statements ask for confirmation.",
                                                )
                                                .child(
                                                    div().text_color(p.fg).child("Lock read-only"),
                                                ),
                                        )
                                    }),
                            )
                            .when(is_db, |d| {
                                d.child(
                                    ui::checkbox(
                                        "history",
                                        self.history,
                                        if self.kind == ConnKind::Redis {
                                            "Record console commands in query history (passwords are masked)"
                                        } else {
                                            "Record executed statements in query history"
                                        },
                                        &p,
                                    )
                                    .on_click(cx.listener(
                                        |this, _, _, cx| {
                                            this.history = !this.history;
                                            cx.notify();
                                        },
                                    )),
                                )
                            })
                            .when(self.kind.is_sql(), |d| {
                                let label = if self.env.is_production() {
                                    "Allow coding agents (Production: read-only queries and \
                                     estimated plans; every call is recorded in history)"
                                } else {
                                    "Allow coding agents (read-only queries, plans, statistics; \
                                     every call is recorded in history)"
                                };
                                d.child(ui::checkbox("agents", self.agents, label, &p).on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.agents = !this.agents;
                                        cx.notify();
                                    }),
                                ))
                            })
                            .children(assistant_field)
                            .when(self.test == TestState::Missing, |d| {
                                d.child(self.render_driver_card(&p, cx))
                            })
                            .when_some(general_error, |d, e| {
                                d.child(
                                    div()
                                        .px(px(10.))
                                        .py(px(6.))
                                        .rounded(px(6.))
                                        .bg(p.prod_bg)
                                        .text_size(px(12.))
                                        .text_color(p.fg)
                                        .child(e),
                                )
                            }),
                    ),
            )
            .child(
                div()
                    .h(px(52.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(16.))
                    .border_t_1()
                    .border_color(p.bd)
                    .child(
                        ui::button("ce-test", "Test connection", Kind::Secondary, &p)
                            .h(px(28.))
                            .on_click(cx.listener(|this, _, _, cx| this.test(cx))),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(7.))
                            .min_w_0()
                            .text_size(px(12.))
                            .text_color(test_color)
                            .child(if testing {
                                ui::pulse_dot("test-dot", test_color, 6.)
                            } else {
                                ui::dot(test_color, 6.).into_any_element()
                            })
                            .child(div().truncate().child(test_label)),
                    )
                    .child(div().flex_1())
                    .child(
                        ui::button("ce-cancel", "Cancel", Kind::Ghost, &p)
                            .h(px(28.))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(ConnEditorEvent::Close))),
                    )
                    .child(
                        ui::button(
                            "ce-save",
                            if is_db { "Save and connect" } else { "Save" },
                            Kind::Primary,
                            &p,
                        )
                        .h(px(28.))
                        .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                    ),
            )
    }
}

/// SQL Server authentication choices: (select key, method), in menu order.
const MSSQL_AUTH: [(&str, DbAuthMethod); 7] = [
    ("password", DbAuthMethod::Password),
    ("entra-interactive", DbAuthMethod::EntraInteractive),
    ("entra-device", DbAuthMethod::EntraDeviceCode),
    ("entra-password", DbAuthMethod::EntraPassword),
    ("entra-sp", DbAuthMethod::EntraServicePrincipal),
    ("integrated", DbAuthMethod::Integrated),
    ("windows", DbAuthMethod::WindowsPassword),
];

/// The Driver Manager component Oracle connections need.
const ORACLE_CLIENT: &str = "oracle-instant-client";

/// Snowflake sign-in methods (its SQL API takes no passwords).
const SNOWFLAKE_AUTH: [(&str, DbAuthMethod); 2] = [
    ("key-pair", DbAuthMethod::KeyPair),
    ("token", DbAuthMethod::AccessToken),
];

fn snowflake_auth_key(m: DbAuthMethod) -> &'static str {
    SNOWFLAKE_AUTH
        .iter()
        .find(|(_, a)| *a == m)
        .map_or("key-pair", |(k, _)| k)
}

fn snowflake_auth_from_key(key: &str) -> DbAuthMethod {
    SNOWFLAKE_AUTH
        .iter()
        .find(|(k, _)| *k == key)
        .map_or(DbAuthMethod::KeyPair, |(_, a)| *a)
}

fn auth_key(m: DbAuthMethod) -> &'static str {
    MSSQL_AUTH
        .iter()
        .find(|(_, a)| *a == m)
        .map_or("password", |(k, _)| k)
}

fn auth_from_key(key: &str) -> DbAuthMethod {
    MSSQL_AUTH
        .iter()
        .find(|(k, _)| *k == key)
        .map_or(DbAuthMethod::Password, |(_, a)| *a)
}
