//! Cloudflare D1 over the Cloudflare REST API.

use switchyard_core::db::Engine;
use switchyard_core::store::DbConnection;

use crate::conn_editor::form::{EngineForm, Field, FieldError, FieldSet, Values};

pub(crate) struct D1;

impl EngineForm for D1 {
    fn engine(&self) -> Engine {
        Engine::D1
    }

    fn name_placeholder(&self) -> &'static str {
        "edge_prod"
    }

    fn new_profile(&self) -> DbConnection {
        let mut d = DbConnection::new("", Engine::D1);
        d.server.clear();
        d
    }

    fn init(&self, d: &DbConnection, f: &mut FieldSet<'_, '_, '_>) {
        f.text("server", &d.server, "0123456789abcdef0123456789abcdef");
        f.text(
            "database",
            &d.database,
            "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx",
        );
        f.secret(d, "API token with D1 Read or Edit");
    }

    fn layout(&self, _v: &Values<'_>) -> Vec<Field> {
        vec![
            Field::new("server", "Account ID").mono().hint(
                "Cloudflare dashboard → Workers & Pages overview (right column), or the dashboard URL",
            ),
            Field::new("database", "Database ID")
                .mono()
                .hint("From `wrangler d1 list` or the D1 database page"),
            Field::new("password", "API token")
                .hint("Stored in the OS keychain · needs the D1 Read or D1 Edit permission"),
        ]
    }

    fn apply(&self, v: &Values<'_>, d: &mut DbConnection) -> Result<(), FieldError> {
        d.server = v.text("server");
        d.port = Engine::D1.default_port();
        d.database = v.text("database");
        d.user.clear();
        d.via_host = None;
        Ok(())
    }
}
