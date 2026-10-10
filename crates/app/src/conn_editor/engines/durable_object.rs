//! A Cloudflare Durable Object's SQLite storage over the Cloudflare REST API.

use switchyard_core::db::Engine;
use switchyard_core::db::d1::{JURISDICTION_OPTION, OBJECT_KIND_OPTION, OBJECT_OPTION};
use switchyard_core::store::DbConnection;

use crate::conn_editor::form::{EngineForm, Field, FieldError, FieldSet, Values};

pub(crate) struct DurableObject;

impl EngineForm for DurableObject {
    fn engine(&self) -> Engine {
        Engine::DurableObject
    }

    fn name_placeholder(&self) -> &'static str {
        "chat-room-lobby"
    }

    fn new_profile(&self) -> DbConnection {
        let mut d = DbConnection::new("", Engine::DurableObject);
        d.server.clear();
        d
    }

    fn init(&self, d: &DbConnection, f: &mut FieldSet<'_, '_, '_>) {
        f.text("server", &d.server, "0123456789abcdef0123456789abcdef");
        f.text("database", &d.database, "5fd1cafff895419c8bcc647fc64ab8f0");
        f.select(
            "object_kind",
            vec![
                ("Name (idFromName)".into(), "name".into()),
                ("Object ID".into(), "id".into()),
            ],
            d.option(OBJECT_KIND_OPTION).unwrap_or("name"),
        );
        f.text(
            "object",
            d.option(OBJECT_OPTION).unwrap_or_default(),
            "lobby",
        );
        f.select(
            "jurisdiction",
            vec![
                ("None".into(), "none".into()),
                ("EU".into(), "eu".into()),
                ("FedRAMP".into(), "fedramp".into()),
            ],
            d.option(JURISDICTION_OPTION).unwrap_or("none"),
        );
        f.secret(d, "API token with Workers Scripts Write");
    }

    fn layout(&self, v: &Values<'_>) -> Vec<Field> {
        let by_id = v.chosen("object_kind") == "id";
        let mut fields = vec![
            Field::new("server", "Account ID")
                .mono()
                .hint("Cloudflare dashboard → Workers & Pages overview (right column)"),
            Field::new("database", "Namespace ID").mono().hint(
                "Workers & Pages → Durable Objects → the namespace; SQLite-backed classes only",
            ),
            Field::new("object_kind", "Open object by").span(2),
        ];
        if by_id {
            fields.push(
                Field::new("object", "Object ID")
                    .span(4)
                    .mono()
                    .hint("64 hex digits, as idFromName(…).toString() prints it"),
            );
        } else {
            fields.push(
                Field::new("object", "Object name")
                    .span(2)
                    .mono()
                    .hint("The name passed to idFromName"),
            );
            fields.push(Field::new("jurisdiction", "Jurisdiction").span(2));
        }
        fields.push(Field::new("password", "API token").hint(
            "Stored in the OS keychain · needs Workers Scripts Write; sees only data stored \
             through the SQL API",
        ));
        fields
    }

    fn apply(&self, v: &Values<'_>, d: &mut DbConnection) -> Result<(), FieldError> {
        d.server = v.text("server");
        d.port = Engine::DurableObject.default_port();
        d.database = v.text("database");
        d.user.clear();
        d.via_host = None;
        let by_id = v.chosen("object_kind") == "id";
        let object = v.text("object");
        if object.is_empty() {
            return Err((
                Some("object"),
                if by_id {
                    "Enter the object's id".into()
                } else {
                    "Enter the name the Worker passes to idFromName".into()
                },
            ));
        }
        d.options.insert(OBJECT_OPTION.into(), object);
        d.options.insert(
            OBJECT_KIND_OPTION.into(),
            if by_id { "id" } else { "name" }.into(),
        );
        if by_id {
            d.options.remove(JURISDICTION_OPTION);
        } else {
            d.options
                .insert(JURISDICTION_OPTION.into(), v.chosen("jurisdiction"));
        }
        Ok(())
    }
}
