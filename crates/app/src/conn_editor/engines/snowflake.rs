//! Snowflake over its SQL REST API.

use switchyard_core::db::{DbAuthMethod, Engine};
use switchyard_core::store::DbConnection;

use crate::conn_editor::form::{EngineForm, Field, FieldError, FieldSet, Values};

/// Snowflake sign-in methods (its SQL API takes no passwords).
const AUTH: [(&str, DbAuthMethod); 2] = [
    ("key-pair", DbAuthMethod::KeyPair),
    ("token", DbAuthMethod::AccessToken),
];

/// Settings kept in `DbConnection::options`.
const OPTIONS: [&str; 4] = ["schema", "warehouse", "role", "private_key_path"];

fn auth_key(m: DbAuthMethod) -> &'static str {
    AUTH.iter()
        .find(|(_, a)| *a == m)
        .map_or("key-pair", |(k, _)| k)
}

fn auth_from_key(key: &str) -> DbAuthMethod {
    AUTH.iter()
        .find(|(k, _)| *k == key)
        .map_or(DbAuthMethod::KeyPair, |(_, a)| *a)
}

pub(crate) struct Snowflake;

impl EngineForm for Snowflake {
    fn engine(&self) -> Engine {
        Engine::Snowflake
    }

    fn name_placeholder(&self) -> &'static str {
        "analytics"
    }

    fn new_profile(&self) -> DbConnection {
        let mut d = DbConnection::new("", Engine::Snowflake);
        d.server.clear();
        d.auth = DbAuthMethod::KeyPair;
        d
    }

    fn init(&self, d: &DbConnection, f: &mut FieldSet<'_, '_, '_>) {
        let option = |k: &str| d.option(k).unwrap_or_default().to_owned();
        f.text("server", &d.server, "myorg-myaccount");
        f.text("user", &d.user, "REPORTING_SVC");
        f.text("database", &d.database, "ANALYTICS");
        f.text("schema", &option("schema"), "PUBLIC");
        f.text("warehouse", &option("warehouse"), "COMPUTE_WH");
        f.text("role", &option("role"), "The user's default");
        f.text(
            "private_key_path",
            &option("private_key_path"),
            "~/.snowflake/rsa_key.p8",
        );
        f.secret(d, "");
        f.select(
            "auth",
            AUTH.iter()
                .map(|(key, m)| (m.label().into(), (*key).into()))
                .collect(),
            auth_key(d.auth),
        );
    }

    fn layout(&self, v: &Values<'_>) -> Vec<Field> {
        let mut f = vec![
            Field::new("server", "Account identifier")
                .mono()
                .hint("orgname-accountname, or the host before .snowflakecomputing.com"),
            Field::new("user", "User").span(3),
            Field::new("auth", "Authentication").span(3),
        ];
        if auth_from_key(&v.chosen("auth")) == DbAuthMethod::KeyPair {
            f.push(
                Field::new("private_key_path", "Private key file")
                    .mono()
                    .hint("PKCS#8 .p8 (or PKCS#1) PEM; read in place, never copied"),
            );
            f.push(
                Field::new("password", "Key passphrase (optional)")
                    .hint("Only for an encrypted key · stored in the OS keychain"),
            );
        } else {
            f.push(Field::new("password", "Programmatic access token").hint(
                "Snowsight → your profile → Programmatic access tokens · stored in the OS keychain",
            ));
        }
        f.extend([
            Field::new("warehouse", "Warehouse").span(3).mono(),
            Field::new("role", "Role").span(3).mono(),
            Field::new("database", "Database").span(3).mono(),
            Field::new("schema", "Schema").span(3).mono(),
        ]);
        f
    }

    fn apply(&self, v: &Values<'_>, d: &mut DbConnection) -> Result<(), FieldError> {
        d.server = v.text("server");
        d.port = Engine::Snowflake.default_port();
        d.database = v.text("database");
        d.user = v.text("user");
        d.auth = auth_from_key(&v.chosen("auth"));
        d.via_host = None;
        for key in OPTIONS {
            let value = v.text(key);
            if value.is_empty() || (key == "private_key_path" && d.auth != DbAuthMethod::KeyPair) {
                d.options.remove(key);
            } else {
                d.options.insert(key.to_owned(), value);
            }
        }
        Ok(())
    }
}
