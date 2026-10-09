//! PostgreSQL.

use switchyard_core::db::{DbAuthMethod, Engine};
use switchyard_core::store::DbConnection;

use crate::conn_editor::form::{EngineForm, Field, FieldError, FieldSet, Values};

pub(crate) struct Postgres;

impl EngineForm for Postgres {
    fn engine(&self) -> Engine {
        Engine::Postgres
    }

    fn name_placeholder(&self) -> &'static str {
        "shop_prod"
    }

    fn new_profile(&self) -> DbConnection {
        let mut d = DbConnection::new("", Engine::Postgres);
        d.database = "postgres".into();
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
    }

    fn layout(&self, _v: &Values<'_>) -> Vec<Field> {
        vec![
            Field::new("host", "Host").span(4).mono(),
            Field::new("port", "Port").span(2).mono(),
            Field::new("database", "Database").span(3),
            Field::new("user", "User").span(3),
            Field::password("Password"),
            Field::new("ssl", "SSL mode").span(3),
            Field::via(),
        ]
    }

    fn apply(&self, v: &Values<'_>, d: &mut DbConnection) -> Result<(), FieldError> {
        d.server = v.text("host");
        d.port = v.port(Engine::Postgres.default_port())?;
        d.database = v.text("database");
        d.user = v.text("user");
        d.auth = DbAuthMethod::Password;
        d.ssl_mode = v.ssl();
        d.via_host = v.via();
        Ok(())
    }
}
