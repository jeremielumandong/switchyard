//! Snapshots of the per-tab database / schema switch statements (DBX-4a).

use super::dialect_for;
use crate::value::Engine;

fn render(engine: Engine, database: &str, schema: &str) -> String {
    let d = dialect_for(engine);
    let show = |s: Option<String>| s.unwrap_or_else(|| "<reconnect / unsupported>".into());
    format!(
        "switches_context: {}\nuse_database({database:?}): {}\nuse_schema({schema:?}): {}",
        d.switches_context(),
        show(d.use_database(database)),
        show(d.use_schema(schema)),
    )
}

#[test]
fn postgres_context_switch() {
    insta::assert_snapshot!(render(Engine::Postgres, "Sales DB", "Reporting"));
    insta::assert_snapshot!(
        "postgres_public",
        render(Engine::Postgres, "shop", "public")
    );
}

#[test]
fn sql_server_context_switch() {
    insta::assert_snapshot!(render(Engine::SqlServer, "odd]name", "sales"));
}

#[test]
fn snowflake_context_switch() {
    insta::assert_snapshot!(render(Engine::Snowflake, "ANALYTICS", "Mixed Case"));
}

#[test]
fn oracle_context_switch() {
    insta::assert_snapshot!(render(Engine::Oracle, "ORCLPDB1", "HR"));
}

#[test]
fn d1_has_no_switcher() {
    insta::assert_snapshot!(render(Engine::D1, "main", "main"));
}

#[test]
fn mysql_context_switch() {
    insta::assert_snapshot!(render(Engine::MySql, "shop", "odd`name"));
}
