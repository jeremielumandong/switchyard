//! Oracle Database (Instant Client, loaded at runtime).

use switchyard_core::db::{DbAuthMethod, Engine};
use switchyard_core::store::DbConnection;

use crate::conn_editor::form::{EngineForm, Field, FieldError, FieldSet, Values};

/// The Driver Manager component Oracle connections need.
const ORACLE_CLIENT: &str = "oracle-instant-client";

pub(crate) struct Oracle;

impl EngineForm for Oracle {
    fn engine(&self) -> Engine {
        Engine::Oracle
    }

    fn name_placeholder(&self) -> &'static str {
        "erp"
    }

    fn init(&self, d: &DbConnection, f: &mut FieldSet<'_, '_, '_>) {
        f.text("host", &d.server, "localhost");
        f.text("port", &d.port.to_string(), "");
        f.text("database", &d.database, "FREEPDB1");
        f.text("user", &d.user, "app_ro");
        f.secret(d, "");
        f.via(d);
    }

    fn layout(&self, _v: &Values<'_>) -> Vec<Field> {
        vec![
            Field::new("host", "Host").span(4).mono(),
            Field::new("port", "Port").span(2).mono(),
            Field::new("database", "Service name")
                .mono()
                .hint("FREEPDB1, ORCLPDB1… or, with Host empty, a TNS alias or descriptor"),
            Field::new("user", "User").span(3),
            Field::password("Password"),
            Field::via(),
        ]
    }

    fn apply(&self, v: &Values<'_>, d: &mut DbConnection) -> Result<(), FieldError> {
        d.server = v.text("host");
        d.port = v.port(Engine::Oracle.default_port())?;
        d.database = v.text("database");
        d.user = v.text("user");
        d.auth = DbAuthMethod::Password;
        d.via_host = v.via();
        Ok(())
    }

    fn required_component(&self, _d: &DbConnection) -> Option<&'static str> {
        Some(ORACLE_CLIENT)
    }
}
