//! Redis.

use switchyard_core::db::{DbAuthMethod, Engine, SslMode};
use switchyard_core::store::DbConnection;

use crate::conn_editor::form::{EngineForm, Field, FieldError, FieldSet, Values};

pub(crate) struct Redis;

impl EngineForm for Redis {
    fn engine(&self) -> Engine {
        Engine::Redis
    }

    fn name_placeholder(&self) -> &'static str {
        "cache"
    }

    fn new_profile(&self) -> DbConnection {
        let mut d = DbConnection::new("", Engine::Redis);
        d.database = "0".into();
        // Redis has no STARTTLS; a server either speaks TLS or not.
        d.ssl_mode = SslMode::Disable;
        d
    }

    fn init(&self, d: &DbConnection, f: &mut FieldSet<'_, '_, '_>) {
        f.text("host", &d.server, "localhost");
        f.text("port", &d.port.to_string(), "");
        f.text("database", &d.database, "0");
        f.text("user", &d.user, "default");
        f.secret(d, "");
        f.ssl(d);
        f.via(d);
    }

    fn layout(&self, _v: &Values<'_>) -> Vec<Field> {
        vec![
            Field::new("host", "Host").span(4).mono(),
            Field::new("port", "Port").span(2).mono(),
            Field::new("database", "Database")
                .span(2)
                .hint("Number, 0 by default"),
            Field::new("user", "ACL user")
                .span(2)
                .hint("Empty for the default user"),
            Field::password("Password").span(2),
            Field::new("ssl", "TLS")
                .span(3)
                .hint("Require for TLS ports (cloud Redis); Prefer means plain TCP"),
            Field::via().span(3),
        ]
    }

    fn apply(&self, v: &Values<'_>, d: &mut DbConnection) -> Result<(), FieldError> {
        d.server = v.text("host");
        d.port = v.port(Engine::Redis.default_port())?;
        d.database = v.text("database");
        d.user = v.text("user");
        d.auth = DbAuthMethod::Password;
        d.ssl_mode = v.ssl();
        d.via_host = v.via();
        Ok(())
    }
}
