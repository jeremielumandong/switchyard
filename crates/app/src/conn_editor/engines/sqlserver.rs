//! SQL Server: SQL logins, Microsoft Entra, Kerberos and NTLM.

use switchyard_core::db::{DbAuthMethod, Engine};
use switchyard_core::store::DbConnection;

use crate::conn_editor::form::{EngineForm, Field, FieldError, FieldSet, Values};

/// SQL Server authentication choices: (select key, method), in menu order.
const AUTH: [(&str, DbAuthMethod); 7] = [
    ("password", DbAuthMethod::Password),
    ("entra-interactive", DbAuthMethod::EntraInteractive),
    ("entra-device", DbAuthMethod::EntraDeviceCode),
    ("entra-password", DbAuthMethod::EntraPassword),
    ("entra-sp", DbAuthMethod::EntraServicePrincipal),
    ("integrated", DbAuthMethod::Integrated),
    ("windows", DbAuthMethod::WindowsPassword),
];

fn auth_key(m: DbAuthMethod) -> &'static str {
    AUTH.iter()
        .find(|(_, a)| *a == m)
        .map_or("password", |(k, _)| k)
}

fn auth_from_key(key: &str) -> DbAuthMethod {
    AUTH.iter()
        .find(|(k, _)| *k == key)
        .map_or(DbAuthMethod::Password, |(_, a)| *a)
}

pub(crate) struct SqlServer;

impl EngineForm for SqlServer {
    fn engine(&self) -> Engine {
        Engine::SqlServer
    }

    fn name_placeholder(&self) -> &'static str {
        "Reporting"
    }

    fn new_profile(&self) -> DbConnection {
        let mut d = DbConnection::new("", Engine::SqlServer);
        d.database = "master".into();
        d
    }

    fn init(&self, d: &DbConnection, f: &mut FieldSet<'_, '_, '_>) {
        f.text("host", &d.server, "localhost");
        f.text("port", &d.port.to_string(), "");
        f.text("database", &d.database, "");
        f.text("user", &d.user, "app_ro");
        f.secret(d, "");
        f.ssl(d);
        f.via(d);
        f.select(
            "auth",
            AUTH.iter()
                .map(|(key, m)| (m.label().into(), (*key).into()))
                .collect(),
            auth_key(d.auth),
        );
        f.text(
            "tenant",
            d.tenant.as_deref().unwrap_or_default(),
            "contoso.onmicrosoft.com",
        );
        f.text(
            "client_id",
            d.entra_client_id.as_deref().unwrap_or_default(),
            "Switchyard's own",
        );
    }

    fn layout(&self, v: &Values<'_>) -> Vec<Field> {
        let auth = auth_from_key(&v.chosen("auth"));
        let integrated = auth == DbAuthMethod::Integrated;
        let mut f =
            vec![
            Field::new("host", "Server").span(4).mono().hint_opt(integrated.then_some(
                "Full host name (db.corp.example.com): Kerberos looks up MSSQLSvc/<server>:<port>",
            )),
            Field::new("port", "Port").span(2).mono(),
            Field::new("database", "Database").span(3),
            Field::new("auth", "Authentication")
                .span(3)
                .hint_opt(integrated.then_some(if cfg!(windows) {
                    "Signs in as the current Windows user"
                } else {
                    "Uses your Kerberos ticket (kinit or your desktop's sign-in)"
                })),
        ];
        match auth {
            DbAuthMethod::Integrated | DbAuthMethod::KeyPair | DbAuthMethod::AccessToken => {}
            DbAuthMethod::WindowsPassword => {
                f.push(
                    Field::new("user", "Windows account")
                        .span(3)
                        .hint("DOMAIN\\user"),
                );
                f.push(Field::password("Password"));
            }
            DbAuthMethod::EntraInteractive | DbAuthMethod::EntraDeviceCode => {
                f.push(
                    Field::new("user", "Account (optional)")
                        .span(3)
                        .hint("Pre-fills the Microsoft sign-in, e.g. name@company.com"),
                );
                f.push(
                    Field::new("tenant", "Tenant (optional)")
                        .span(3)
                        .mono()
                        .hint("Directory id or domain; blank = any work or school account"),
                );
                f.push(
                    Field::new("client_id", "Application (client) id")
                        .span(3)
                        .mono()
                        .hint("Leave empty to sign in as Microsoft's SQL client (like SSMS)"),
                );
            }
            DbAuthMethod::EntraServicePrincipal => {
                f.push(Field::new("user", "Application (client) id").span(3).mono());
                f.push(Field::password("Client secret"));
                f.push(Field::new("tenant", "Tenant").span(3).mono());
            }
            DbAuthMethod::Password | DbAuthMethod::EntraPassword => {
                let entra = auth == DbAuthMethod::EntraPassword;
                f.push(
                    Field::new("user", if entra { "Microsoft account" } else { "User" }).span(3),
                );
                f.push(Field::password("Password"));
                if entra {
                    f.push(
                        Field::new("tenant", "Tenant (optional)")
                            .span(3)
                            .mono()
                            .hint("No MFA with this method; use browser sign-in for MFA"),
                    );
                }
            }
        }
        f.push(Field::new("ssl", "Encrypt").span(3));
        f.push(Field::new("via", "Connect via Host").span(3));
        f
    }

    fn apply(&self, v: &Values<'_>, d: &mut DbConnection) -> Result<(), FieldError> {
        d.server = v.text("host");
        d.port = v.port(Engine::SqlServer.default_port())?;
        d.database = v.text("database");
        d.user = v.text("user");
        d.ssl_mode = v.ssl();
        d.via_host = v.via();
        d.auth = auth_from_key(&v.chosen("auth"));
        d.tenant = v.opt("tenant");
        d.entra_client_id = v.opt("client_id");
        Ok(())
    }

    fn required_component(&self, d: &DbConnection) -> Option<&'static str> {
        (d.auth == DbAuthMethod::Integrated).then_some("gssapi")
    }
}
