//! Schema compare against PostgreSQL: two schemas in the docker database, the generated
//! migration run on the target, then compared again. Needs the docker services
//! (`docker/compose.yml`); override the server with `SWITCHYARD_PG_*` like `pg.rs`.
//! Run with `cargo test -p switchyard-db --test compare_pg -- --ignored`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use futures::StreamExt;
use secrecy::SecretString;
use switchyard_db::compare::{self, DiffStatus, ScriptHeader};
use switchyard_db::pg::PgDriver;
use switchyard_db::{DbConfig, DbSession, Driver, Engine, ObjectKind, SslMode, dialect_for};

fn env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

async fn session() -> Box<dyn DbSession> {
    let mut cfg = DbConfig::new(
        Engine::Postgres,
        env("SWITCHYARD_PG_HOST", "127.0.0.1"),
        env("SWITCHYARD_PG_DB", "shop"),
    );
    cfg.port = env("SWITCHYARD_PG_PORT", "5432").parse().unwrap();
    cfg.user = env("SWITCHYARD_PG_USER", "switchyard");
    cfg.password = Some(SecretString::from(env(
        "SWITCHYARD_PG_PASSWORD",
        "switchyard",
    )));
    cfg.ssl_mode = SslMode::Disable;
    PgDriver.connect(&cfg, None).await.expect("connect")
}

/// Run every statement of `script` (comment-only units are skipped by the splitter).
async fn run_script(s: &mut Box<dyn DbSession>, script: &str) {
    let d = dialect_for(Engine::Postgres);
    for span in d.split_script(script) {
        let sql = span.text(script);
        let mut stream = s
            .execute(sql, &[])
            .await
            .unwrap_or_else(|e| panic!("{e}\n{sql}"));
        while let Some(ev) = stream.next().await {
            if let Err(e) = ev {
                panic!("{e}\n{sql}");
            }
        }
    }
}

const SETUP: &str = r#"
DROP SCHEMA IF EXISTS cmp_src CASCADE;
DROP SCHEMA IF EXISTS cmp_tgt CASCADE;
CREATE SCHEMA cmp_src;
CREATE SCHEMA cmp_tgt;

CREATE TABLE cmp_src.customers (
    id integer GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    email text NOT NULL UNIQUE
);
CREATE TABLE cmp_src.orders (
    id bigserial PRIMARY KEY,
    customer_id integer NOT NULL REFERENCES cmp_src.customers (id) ON DELETE CASCADE,
    qty bigint NOT NULL DEFAULT 1 CHECK (qty > 0),
    note text
);
CREATE INDEX orders_customer_idx ON cmp_src.orders (customer_id) WHERE qty > 1;
CREATE TABLE cmp_src.invoices (
    id serial PRIMARY KEY,
    order_id bigint NOT NULL REFERENCES cmp_src.orders (id)
);
CREATE VIEW cmp_src.big_orders AS SELECT id, qty FROM cmp_src.orders WHERE qty > 10;
CREATE FUNCTION cmp_src.order_qty(oid bigint) RETURNS bigint LANGUAGE sql
    AS $f$ SELECT qty FROM cmp_src.orders WHERE id = oid $f$;

CREATE TABLE cmp_tgt.customers (
    id integer GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    email text NOT NULL UNIQUE
);
CREATE TABLE cmp_tgt.orders (
    id bigserial PRIMARY KEY,
    customer_id integer NOT NULL REFERENCES cmp_tgt.customers (id),
    qty integer,
    legacy_code text
);
CREATE INDEX orders_legacy_idx ON cmp_tgt.orders (legacy_code);
CREATE TABLE cmp_tgt.audit (at timestamptz DEFAULT now());
CREATE VIEW cmp_tgt.big_orders AS SELECT id, qty FROM cmp_tgt.orders WHERE qty > 100;
CREATE VIEW cmp_src.all_orders AS SELECT id, qty FROM cmp_src.orders;
CREATE VIEW cmp_tgt.all_orders AS SELECT id, qty FROM cmp_tgt.orders;
"#;

async fn snapshots(
    s: &mut Box<dyn DbSession>,
) -> (compare::SchemaSnapshot, compare::SchemaSnapshot) {
    let a = compare::load_snapshot(s.as_mut(), Engine::Postgres, None, "cmp_src", |_, _| true)
        .await
        .unwrap();
    let b = compare::load_snapshot(s.as_mut(), Engine::Postgres, None, "cmp_tgt", |_, _| true)
        .await
        .unwrap();
    assert!(a.skipped.is_empty(), "{:?}", a.skipped);
    assert!(b.skipped.is_empty(), "{:?}", b.skipped);
    (a, b)
}

#[tokio::test]
#[ignore]
async fn two_schemas_converge_after_the_migration() {
    let mut s = session().await;
    run_script(&mut s, SETUP).await;
    let scripter = dialect_for(Engine::Postgres).schema_scripter().unwrap();

    let (a, b) = snapshots(&mut s).await;
    // Identity columns are read from the catalog.
    let customers = a
        .objects
        .iter()
        .find(|o| o.object.name == "customers")
        .unwrap();
    assert_eq!(
        customers.columns[0].identity.as_deref(),
        Some("GENERATED ALWAYS AS IDENTITY")
    );
    let diff = compare::compare(&a, &b, scripter);
    let names = |status: DiffStatus| {
        diff.objects
            .iter()
            .filter(|o| o.status == status)
            .map(|o| format!("{:?} {}", o.kind, o.name))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        names(DiffStatus::OnlyInSource),
        ["Table invoices", "Function order_qty"]
    );
    assert_eq!(names(DiffStatus::OnlyInTarget), ["Table audit"]);
    let detail: Vec<String> = diff
        .objects
        .iter()
        .flat_map(|o| {
            o.changes
                .iter()
                .map(move |c| format!("{}: {}", o.name, c.label()))
        })
        .collect();
    assert_eq!(
        names(DiffStatus::Different),
        ["Table orders", "View big_orders"],
        "{detail:#?}"
    );
    // Same objects in another schema compare equal (customers, all_orders).
    assert_eq!(diff.identical, 2);

    let script = compare::migration_script(
        &diff,
        &|_| true,
        scripter,
        &ScriptHeader {
            source: "test · cmp_src".into(),
            target: "test · cmp_tgt".into(),
            database: None,
        },
    );
    assert!(script.contains("-- DROP TABLE cmp_tgt.audit;"), "{script}");
    run_script(&mut s, &script).await;

    // Everything but the commented-out destructive statements now matches.
    let (a, b) = snapshots(&mut s).await;
    let diff = compare::compare(&a, &b, scripter);
    let left: Vec<String> = diff
        .objects
        .iter()
        .map(|o| {
            let changes: Vec<String> = o.changes.iter().map(|c| c.label()).collect();
            format!(
                "{:?} {:?} {} {}",
                o.status,
                o.kind,
                o.name,
                changes.join("; ")
            )
        })
        .collect();
    assert_eq!(
        left,
        [
            "OnlyInTarget Table audit ",
            "Different Table orders − column legacy_code"
        ],
        "{script}"
    );
    assert!(diff.objects.iter().all(|o| o.kind == ObjectKind::Table));

    run_script(
        &mut s,
        "DROP SCHEMA cmp_src CASCADE; DROP SCHEMA cmp_tgt CASCADE;",
    )
    .await;
}
