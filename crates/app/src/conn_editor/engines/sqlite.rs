//! A local SQLite file.

use switchyard_core::db::Engine;
use switchyard_core::store::DbConnection;

use crate::conn_editor::form::{EngineForm, Field, FieldError, FieldSet, Values};

pub(crate) struct Sqlite;

impl EngineForm for Sqlite {
    fn engine(&self) -> Engine {
        Engine::Sqlite
    }

    fn name_placeholder(&self) -> &'static str {
        "app_local"
    }

    fn init(&self, d: &DbConnection, f: &mut FieldSet<'_, '_, '_>) {
        f.text("database", &d.database, "~/data/app.db  or  :memory:");
    }

    fn layout(&self, v: &Values<'_>) -> Vec<Field> {
        vec![
            Field::new("database", "Database file")
                .mono()
                .browse()
                .hint(if v.read_only() {
                    "Read-only: the file must exist"
                } else {
                    "Created if it does not exist · :memory: for a scratch database"
                }),
        ]
    }

    fn apply(&self, v: &Values<'_>, d: &mut DbConnection) -> Result<(), FieldError> {
        d.server.clear();
        d.port = Engine::Sqlite.default_port();
        d.database = v.text("database");
        d.user.clear();
        d.via_host = None;
        Ok(())
    }
}
