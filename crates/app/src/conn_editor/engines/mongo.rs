//! MongoDB.

use switchyard_core::db::{DbAuthMethod, Engine};
use switchyard_core::store::DbConnection;

use crate::conn_editor::form::{EngineForm, Field, FieldError, FieldSet, Values};

/// Settings kept in `DbConnection::options`.
const OPTIONS: [&str; 2] = ["auth_source", "replica_set"];

pub(crate) struct Mongo;

impl EngineForm for Mongo {
    fn engine(&self) -> Engine {
        Engine::MongoDb
    }

    fn name_placeholder(&self) -> &'static str {
        "catalog"
    }

    fn new_profile(&self) -> DbConnection {
        let mut d = DbConnection::new("", Engine::MongoDb);
        d.database = "test".into();
        d
    }

    fn init(&self, d: &DbConnection, f: &mut FieldSet<'_, '_, '_>) {
        let option = |k: &str| d.option(k).unwrap_or_default().to_owned();
        f.select(
            "srv",
            vec![
                ("Host and port".into(), String::new()),
                ("SRV record (mongodb+srv, Atlas)".into(), "true".into()),
            ],
            d.option("srv").unwrap_or_default(),
        );
        f.text("host", &d.server, "localhost");
        f.text("port", &d.port.to_string(), "");
        f.text("database", &d.database, "test");
        f.text("user", &d.user, "Leave empty without authentication");
        f.secret(d, "");
        f.text("auth_source", &option("auth_source"), "admin");
        f.text("replica_set", &option("replica_set"), "Optional");
        f.ssl(d);
        f.via(d);
    }

    fn layout(&self, v: &Values<'_>) -> Vec<Field> {
        let srv = v.chosen("srv") == "true";
        let mut f = vec![Field::new("srv", "Connect with")];
        if srv {
            f.push(
                Field::new("host", "Cluster host")
                    .mono()
                    .hint("cluster0.abcde.mongodb.net: members and TLS come from its DNS records"),
            );
        } else {
            f.push(
                Field::new("host", "Host")
                    .span(4)
                    .mono()
                    .hint("Several members: a.example:27017, b.example:27017"),
            );
            f.push(Field::new("port", "Port").span(2).mono());
        }
        f.extend([
            Field::new("database", "Database").span(3).mono(),
            Field::new("auth_source", "Auth database").span(3).mono(),
            Field::new("user", "User").span(3),
            Field::password("Password"),
        ]);
        if !srv {
            f.push(Field::new("replica_set", "Replica set").span(3).mono());
        }
        f.push(
            Field::new("ssl", "TLS")
                .span(3)
                .hint("MongoDB cannot negotiate TLS: prefer means off, or on for SRV"),
        );
        if !srv {
            f.push(Field::via());
        }
        f
    }

    fn apply(&self, v: &Values<'_>, d: &mut DbConnection) -> Result<(), FieldError> {
        let srv = v.chosen("srv") == "true";
        d.server = v.text("host");
        d.port = v.port(Engine::MongoDb.default_port())?;
        d.database = v.text("database");
        d.user = v.text("user");
        d.auth = DbAuthMethod::Password;
        d.ssl_mode = v.ssl();
        d.via_host = if srv { None } else { v.via() };
        if srv {
            d.options.insert("srv".into(), "true".into());
        } else {
            d.options.remove("srv");
        }
        for key in OPTIONS {
            match v.opt(key) {
                Some(value) if !(srv && key == "replica_set") => {
                    d.options.insert(key.to_owned(), value);
                }
                _ => {
                    d.options.remove(key);
                }
            }
        }
        Ok(())
    }
}
