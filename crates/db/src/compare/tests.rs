//! Diff model and generated-script tests. Scripts are insta snapshots per engine.

use super::script::ScriptHeader;
use super::*;
use crate::catalog::{ColumnInfo, ConstraintInfo, ForeignKeyInfo, IndexInfo};

fn col(name: &str, ty: &str, nullable: bool, default: Option<&str>, ord: i32) -> ColumnInfo {
    ColumnInfo {
        name: name.into(),
        data_type: ty.into(),
        nullable,
        default: default.map(Into::into),
        ordinal: ord,
        ..ColumnInfo::default()
    }
}

fn pk(c: ColumnInfo) -> ColumnInfo {
    ColumnInfo {
        is_primary_key: true,
        nullable: false,
        ..c
    }
}

fn table(schema: &str, name: &str, columns: Vec<ColumnInfo>) -> ObjectDetail {
    ObjectDetail {
        object: ObjectInfo {
            schema: schema.into(),
            name: name.into(),
            kind: ObjectKind::Table,
            ..ObjectInfo::default()
        },
        columns,
        ..ObjectDetail::default()
    }
}

fn index(name: &str, cols: &[&str], unique: bool, primary: bool, def: &str) -> IndexInfo {
    IndexInfo {
        name: name.into(),
        columns: cols.iter().map(|c| (*c).into()).collect(),
        is_unique: unique,
        is_primary: primary,
        definition: def.into(),
        method: None,
    }
}

fn object(
    schema: &str,
    name: &str,
    kind: ObjectKind,
    ddl: &str,
    sig: Option<&str>,
) -> ObjectDetail {
    ObjectDetail {
        object: ObjectInfo {
            schema: schema.into(),
            name: name.into(),
            kind,
            detail: sig.map(Into::into),
            ..ObjectInfo::default()
        },
        ddl: ddl.into(),
        ..ObjectDetail::default()
    }
}

fn snap(engine: Engine, schema: &str, objects: Vec<ObjectDetail>) -> SchemaSnapshot {
    SchemaSnapshot {
        engine,
        database: None,
        schema: schema.into(),
        objects,
        skipped: Vec::new(),
    }
}

/// PostgreSQL fixture: `app` (source) against `app_old` (target).
fn pg_pair() -> (SchemaSnapshot, SchemaSnapshot) {
    let mut customers = table(
        "app",
        "customers",
        vec![
            pk(col("id", "integer", false, None, 1)),
            col("email", "text", false, None, 2),
        ],
    );
    customers.columns[0].identity = Some("GENERATED ALWAYS AS IDENTITY".into());
    customers.indexes = vec![
        index("customers_pkey", &["id"], true, true, ""),
        index("customers_email_key", &["email"], true, false, ""),
    ];
    customers.constraints = vec![
        ConstraintInfo {
            name: "customers_pkey".into(),
            kind: "PRIMARY KEY".into(),
            definition: "PRIMARY KEY (id)".into(),
        },
        ConstraintInfo {
            name: "customers_email_key".into(),
            kind: "UNIQUE".into(),
            definition: "UNIQUE (email)".into(),
        },
    ];
    let mut old_customers = customers.clone();
    old_customers.object.schema = "app_old".into();

    let mut orders = table(
        "app",
        "orders",
        vec![
            pk(col(
                "id",
                "bigint",
                false,
                Some("nextval('app.orders_id_seq'::regclass)"),
                1,
            )),
            col("customer_id", "integer", false, None, 2),
            col("qty", "bigint", false, Some("1"), 3),
            col("note", "text", true, None, 4),
        ],
    );
    orders.indexes = vec![
        index("orders_pkey", &["id"], true, true, ""),
        index(
            "orders_customer_idx",
            &["customer_id"],
            false,
            false,
            "CREATE INDEX orders_customer_idx ON app.orders USING btree (customer_id)",
        ),
    ];
    orders.constraints = vec![ConstraintInfo {
        name: "orders_qty_check".into(),
        kind: "CHECK".into(),
        definition: "CHECK ((qty > 0))".into(),
    }];
    orders.foreign_keys = vec![ForeignKeyInfo {
        name: "orders_customer_id_fkey".into(),
        columns: vec!["customer_id".into()],
        references: "app.customers".into(),
        referenced_columns: vec!["id".into()],
        on_delete: Some("CASCADE".into()),
        on_update: Some("NO ACTION".into()),
    }];

    let mut old_orders = table(
        "app_old",
        "orders",
        vec![
            pk(col(
                "id",
                "bigint",
                false,
                Some("nextval('app_old.orders_id_seq'::regclass)"),
                1,
            )),
            col("customer_id", "integer", false, None, 2),
            col("qty", "integer", true, None, 3),
            col("legacy_code", "text", true, None, 4),
        ],
    );
    old_orders.indexes = vec![
        index("orders_pkey", &["id"], true, true, ""),
        index(
            "orders_legacy_idx",
            &["legacy_code"],
            false,
            false,
            "CREATE INDEX orders_legacy_idx ON app_old.orders USING btree (legacy_code)",
        ),
    ];
    old_orders.foreign_keys = vec![ForeignKeyInfo {
        name: "orders_customer_id_fkey".into(),
        columns: vec!["customer_id".into()],
        references: "app_old.customers".into(),
        referenced_columns: vec!["id".into()],
        on_delete: None,
        on_update: None,
    }];

    let mut invoices = table(
        "app",
        "invoices",
        vec![
            pk(col(
                "id",
                "integer",
                false,
                Some("nextval('app.invoices_id_seq'::regclass)"),
                1,
            )),
            col("order_id", "bigint", false, None, 2),
        ],
    );
    invoices.indexes = vec![index("invoices_pkey", &["id"], true, true, "")];
    invoices.constraints = vec![ConstraintInfo {
        name: "invoices_pkey".into(),
        kind: "PRIMARY KEY".into(),
        definition: "PRIMARY KEY (id)".into(),
    }];
    invoices.foreign_keys = vec![ForeignKeyInfo {
        name: "invoices_order_id_fkey".into(),
        columns: vec!["order_id".into()],
        references: "app.orders".into(),
        referenced_columns: vec!["id".into()],
        on_delete: None,
        on_update: None,
    }];

    let audit = table(
        "app_old",
        "audit",
        vec![col(
            "at",
            "timestamp with time zone",
            true,
            Some("now()"),
            1,
        )],
    );

    let view = object(
        "app",
        "big_orders",
        ObjectKind::View,
        "CREATE OR REPLACE VIEW app.big_orders AS\n SELECT id, qty\n   FROM app.orders\n  WHERE qty > 10",
        None,
    );
    let old_view = object(
        "app_old",
        "big_orders",
        ObjectKind::View,
        "CREATE OR REPLACE VIEW app_old.big_orders AS\n SELECT id, qty\n   FROM app_old.orders\n  WHERE qty > 100",
        None,
    );
    let same_view = object(
        "app",
        "all_orders",
        ObjectKind::View,
        "CREATE OR REPLACE VIEW app.all_orders AS\n SELECT id\n   FROM app.orders",
        None,
    );
    let old_same_view = object(
        "app_old",
        "all_orders",
        ObjectKind::View,
        "CREATE OR REPLACE VIEW app_old.all_orders AS\n SELECT id\n   FROM app_old.orders",
        None,
    );
    let func = object(
        "app",
        "order_total",
        ObjectKind::Function,
        "CREATE OR REPLACE FUNCTION app.order_total(oid bigint)\n RETURNS bigint\n LANGUAGE sql\nAS $function$ SELECT qty FROM app.orders WHERE id = oid $function$\n",
        Some("(oid bigint)"),
    );
    let old_func = object(
        "app_old",
        "purge",
        ObjectKind::Procedure,
        "CREATE OR REPLACE PROCEDURE app_old.purge()\n LANGUAGE sql\nAS $procedure$ DELETE FROM app_old.audit $procedure$\n",
        Some("()"),
    );
    (
        snap(
            Engine::Postgres,
            "app",
            vec![customers, orders, invoices, view, same_view, func],
        ),
        snap(
            Engine::Postgres,
            "app_old",
            vec![
                old_customers,
                old_orders,
                audit,
                old_view,
                old_same_view,
                old_func,
            ],
        ),
    )
}

fn header() -> ScriptHeader {
    ScriptHeader {
        source: "dev · app".into(),
        target: "staging · app_old".into(),
        database: None,
    }
}

fn summary(d: &SchemaDiff) -> String {
    let mut out = format!("identical: {}\n", d.identical);
    for o in &d.objects {
        out.push_str(&format!(
            "[{}] {:?} {}{}\n",
            o.status.label(),
            o.kind,
            o.label(),
            if o.is_destructive() {
                " (destructive)"
            } else {
                ""
            }
        ));
        for c in &o.changes {
            out.push_str(&format!("    {}\n", c.label()));
        }
    }
    out
}

#[test]
fn pg_diff_model() {
    let (a, b) = pg_pair();
    let d = compare(&a, &b, &PostgresScripter);
    insta::assert_snapshot!("compare_pg_diff", summary(&d));
    // Grouped: only in source, only in target, then different.
    let statuses: Vec<DiffStatus> = d.objects.iter().map(|o| o.status).collect();
    let mut sorted = statuses.clone();
    sorted.sort();
    assert_eq!(statuses, sorted);
    // Same schema content under another name compares equal (customers, all_orders).
    assert_eq!(d.identical, 2);
}

#[test]
fn identical_snapshots_have_no_diff() {
    let (a, _) = pg_pair();
    let d = compare(&a, &a, &PostgresScripter);
    assert!(d.objects.is_empty());
    assert_eq!(d.identical, a.objects.len());
}

#[test]
fn constraints_with_generated_names_pair_by_shape() {
    let mut a = table("dbo", "t", vec![pk(col("id", "int", false, None, 1))]);
    a.indexes = vec![index("PK__t__3213E83F0A1B2C3D", &["id"], true, true, "")];
    a.constraints = vec![ConstraintInfo {
        name: "CK__t__id__1A2B3C".into(),
        kind: "CHECK".into(),
        definition: "([id]>(0))".into(),
    }];
    let mut b = a.clone();
    b.indexes[0].name = "PK__t__3213E83F99887766".into();
    b.constraints[0].name = "CK__t__id__9F8E7D".into();
    let d = compare(
        &snap(Engine::SqlServer, "dbo", vec![a]),
        &snap(Engine::SqlServer, "dbo", vec![b]),
        &TSqlScripter,
    );
    assert!(d.objects.is_empty(), "{}", summary(&d));
}

#[test]
fn pg_script() {
    let (a, b) = pg_pair();
    let d = compare(&a, &b, &PostgresScripter);
    let script = migration_script(&d, &|_| true, &PostgresScripter, &header());
    insta::assert_snapshot!("compare_pg_script", script);
    // Destructive statements are commented out under a warning.
    assert!(script.contains("-- WARNING: 2 destructive statements"));
    assert!(script.contains("-- DROP TABLE app_old.audit;"));
    assert!(script.contains("-- ALTER TABLE app_old.orders DROP COLUMN legacy_code;"));
    assert!(!script.lines().any(|l| l.starts_with("DROP TABLE")));
}

#[test]
fn pg_script_respects_the_selection() {
    let (a, b) = pg_pair();
    let d = compare(&a, &b, &PostgresScripter);
    let only = d
        .objects
        .iter()
        .position(|o| o.name == "invoices")
        .unwrap_or_default();
    let script = migration_script(&d, &|i| i == only, &PostgresScripter, &header());
    assert!(script.contains("CREATE TABLE app_old.invoices"));
    assert!(script.contains("ADD CONSTRAINT invoices_order_id_fkey FOREIGN KEY (order_id) REFERENCES app_old.orders (id)"));
    assert!(!script.contains("orders_legacy_idx"));
    assert!(!script.contains("WARNING"));
    let none = migration_script(&d, &|_| false, &PostgresScripter, &header());
    assert!(none.contains("-- Nothing selected."));
}

#[test]
fn pg_display_ddl_shows_only_real_differences() {
    let (a, b) = pg_pair();
    let d = compare(&a, &b, &PostgresScripter);
    let orders = d.objects.iter().find(|o| o.name == "orders").unwrap();
    let (src, tgt) = script::display_ddl(&d, orders, &PostgresScripter);
    insta::assert_snapshot!("compare_pg_display_orders", format!("{src}\n----\n{tgt}"));
}

/// SQL Server fixture: `dbo` in two databases.
fn mssql_pair() -> (SchemaSnapshot, SchemaSnapshot) {
    let mut a = table(
        "dbo",
        "Orders",
        vec![
            pk(col("Id", "int", false, None, 1)),
            col("Qty", "bigint", false, Some("((1))"), 2),
            col("Status", "nvarchar(20)", true, None, 3),
        ],
    );
    a.columns[0].identity = Some("IDENTITY(1,1)".into());
    a.indexes = vec![
        IndexInfo {
            method: Some("clustered".into()),
            ..index("PK__Orders__3214EC07AAAA", &["Id"], true, true, "")
        },
        IndexInfo {
            method: Some("nonclustered".into()),
            ..index(
                "IX_Orders_Status",
                &["Status", "Qty DESC"],
                false,
                false,
                "",
            )
        },
    ];
    a.constraints = vec![ConstraintInfo {
        name: "CK_Orders_Qty".into(),
        kind: "CHECK".into(),
        definition: "([Qty]>(0))".into(),
    }];
    let mut b = table(
        "dbo",
        "Orders",
        vec![
            pk(col("Id", "int", false, None, 1)),
            col("Qty", "int", true, Some("((0))"), 2),
            col("Legacy", "varchar(10)", true, Some("('x')"), 3),
        ],
    );
    b.columns[0].identity = Some("IDENTITY(1,1)".into());
    b.indexes = vec![IndexInfo {
        method: Some("clustered".into()),
        ..index("PK__Orders__3214EC07BBBB", &["Id"], true, true, "")
    }];
    let view = object(
        "dbo",
        "BigOrders",
        ObjectKind::View,
        "CREATE VIEW dbo.BigOrders AS\nSELECT Id FROM dbo.Orders WHERE Qty > 10",
        None,
    );
    let proc_ = object(
        "dbo",
        "Purge",
        ObjectKind::Procedure,
        "CREATE PROCEDURE [dbo].[Purge] AS\nBEGIN\n  DELETE FROM dbo.Orders WHERE Qty = 0;\nEND",
        None,
    );
    let mut old = table("dbo", "Old", vec![col("x", "int", true, None, 1)]);
    old.object.schema = "dbo".into();
    (
        snap(Engine::SqlServer, "dbo", vec![a, view, proc_]),
        snap(Engine::SqlServer, "dbo", vec![b, old]),
    )
}

#[test]
fn mssql_script() {
    let (a, b) = mssql_pair();
    let d = compare(&a, &b, &TSqlScripter);
    insta::assert_snapshot!("compare_mssql_diff", summary(&d));
    let script = migration_script(&d, &|_| true, &TSqlScripter, &header());
    insta::assert_snapshot!("compare_mssql_script", script);
    let in_db = migration_script(
        &d,
        &|_| false,
        &TSqlScripter,
        &ScriptHeader {
            database: Some("Shop Test".into()),
            ..header()
        },
    );
    assert!(in_db.contains("USE [Shop Test];\nGO\n"), "{in_db}");
}

/// MySQL fixture: database `shop` against `shop_test`.
fn mysql_pair() -> (SchemaSnapshot, SchemaSnapshot) {
    let mut a = table(
        "shop",
        "orders",
        vec![
            pk(col("id", "int unsigned", false, None, 1)),
            col("status", "varchar(20)", false, Some("new"), 2),
            col("created", "timestamp", false, Some("CURRENT_TIMESTAMP"), 3),
        ],
    );
    a.columns[0].identity = Some("AUTO_INCREMENT".into());
    a.indexes = vec![
        index("PRIMARY", &["id"], true, true, ""),
        index("orders_status", &["status(10)"], false, false, ""),
    ];
    a.constraints = vec![ConstraintInfo {
        name: "PRIMARY".into(),
        kind: "PRIMARY KEY".into(),
        definition: "PRIMARY KEY (`id`)".into(),
    }];
    let mut b = table(
        "shop_test",
        "orders",
        vec![
            pk(col("id", "int unsigned", false, None, 1)),
            col("status", "varchar(10)", true, Some("'new'"), 2),
        ],
    );
    b.columns[0].identity = Some("AUTO_INCREMENT".into());
    b.indexes = vec![index("PRIMARY", &["id"], true, true, "")];
    b.constraints = a.constraints.clone();
    let view = object(
        "shop",
        "open_orders",
        ObjectKind::View,
        "CREATE ALGORITHM=UNDEFINED DEFINER=`root`@`%` SQL SECURITY DEFINER VIEW `shop`.`open_orders` AS select `shop`.`orders`.`id` AS `id` from `shop`.`orders` where (`shop`.`orders`.`status` = 'new')",
        None,
    );
    let routine = object(
        "shop",
        "count_orders",
        ObjectKind::Procedure,
        "CREATE DEFINER=`root`@`%` PROCEDURE `count_orders`()\nBEGIN\n  SELECT COUNT(*) FROM orders;\nEND",
        Some("()"),
    );
    (
        snap(Engine::MySql, "shop", vec![a, view, routine]),
        snap(Engine::MySql, "shop_test", vec![b]),
    )
}

#[test]
fn mysql_script() {
    let (a, b) = mysql_pair();
    let d = compare(&a, &b, &MySqlScripter);
    insta::assert_snapshot!("compare_mysql_diff", summary(&d));
    let script = migration_script(&d, &|_| true, &MySqlScripter, &header());
    insta::assert_snapshot!("compare_mysql_script", script);
    // The routine body keeps its `;` inside a DELIMITER block, so it splits as one unit.
    let spans = crate::dialect_for(Engine::MySql).split_script(&script);
    assert!(spans.iter().any(|s| {
        s.text(&script)
            .contains("SELECT COUNT(*) FROM orders;\nEND")
    }));
}

/// SQLite fixture: two files, schema `main`.
fn sqlite_pair() -> (SchemaSnapshot, SchemaSnapshot) {
    let mut a = table(
        "main",
        "items",
        vec![
            pk(col("id", "INTEGER", false, None, 1)),
            col("name", "TEXT", false, None, 2),
            col("price", "REAL", true, Some("0"), 3),
        ],
    );
    a.ddl = "CREATE TABLE items (\n  id INTEGER PRIMARY KEY,\n  name TEXT NOT NULL CHECK (name <> ''),\n  price REAL DEFAULT 0\n);\nCREATE INDEX items_name ON items(name);\nCREATE TRIGGER items_touch AFTER UPDATE ON items BEGIN SELECT 1; END;".into();
    a.indexes = vec![index(
        "items_name",
        &["name"],
        false,
        false,
        "CREATE INDEX items_name ON items(name)",
    )];
    let mut b = table(
        "main",
        "items",
        vec![
            pk(col("id", "INTEGER", false, None, 1)),
            col("name", "TEXT", true, None, 2),
            col("old", "TEXT", true, None, 3),
        ],
    );
    b.ddl = "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT, old TEXT)".into();

    let mut tags = table(
        "main",
        "tags",
        vec![
            pk(col("id", "INTEGER", false, None, 1)),
            col("label", "TEXT", true, None, 2),
        ],
    );
    tags.ddl = "CREATE TABLE \"tags\" (id INTEGER PRIMARY KEY, label TEXT)".into();
    let mut old_tags = tags.clone();
    old_tags.columns.truncate(1);
    old_tags.ddl = "CREATE TABLE \"tags\" (id INTEGER PRIMARY KEY)".into();

    let mut notes = table("main", "notes", vec![col("body", "TEXT", true, None, 1)]);
    notes.ddl = "CREATE TABLE notes (body TEXT, item_id INTEGER REFERENCES items(id))".into();
    let view = object(
        "main",
        "cheap",
        ObjectKind::View,
        "CREATE VIEW cheap AS SELECT * FROM items WHERE price < 1",
        None,
    );
    (
        snap(Engine::Sqlite, "main", vec![a, tags, notes, view]),
        snap(Engine::Sqlite, "main", vec![b, old_tags]),
    )
}

#[test]
fn sqlite_script_rebuilds_tables() {
    let (a, b) = sqlite_pair();
    let d = compare(&a, &b, &SqliteScripter);
    insta::assert_snapshot!("compare_sqlite_diff", summary(&d));
    let script = migration_script(&d, &|_| true, &SqliteScripter, &header());
    insta::assert_snapshot!("compare_sqlite_script", script);
    // A plain new column is a simple ALTER, not a rebuild.
    assert!(script.contains("ALTER TABLE tags ADD COLUMN label TEXT;"));
}

#[test]
fn unsupported_engines_say_so() {
    for e in [
        Engine::Oracle,
        Engine::Snowflake,
        Engine::D1,
        Engine::MongoDb,
    ] {
        assert!(crate::dialect_for(e).schema_scripter().is_none());
    }
    for e in [
        Engine::Postgres,
        Engine::SqlServer,
        Engine::MySql,
        Engine::Sqlite,
    ] {
        assert!(crate::dialect_for(e).schema_scripter().is_some());
    }
    assert_eq!(
        unsupported_message(Engine::Oracle),
        "Schema compare isn't supported for Oracle yet"
    );
}

#[tokio::test]
async fn snapshot_loads_through_a_session_and_cancels() {
    use crate::mock::MockSession;
    let mut session = MockSession::default();
    let mut calls = 0;
    let s = load_snapshot(&mut session, Engine::Postgres, None, "public", |_, _| {
        calls += 1;
        true
    })
    .await
    .unwrap();
    // The mock lists three objects per folder and has no detail: all are skipped.
    assert_eq!(s.objects.len(), 0);
    assert_eq!(s.skipped.len(), 15);
    assert!(calls > 15);
    let mut session = MockSession::default();
    let r = load_snapshot(&mut session, Engine::Postgres, None, "public", |done, _| {
        done < 2
    })
    .await;
    assert!(matches!(r, Err(DbError::Cancelled)));
    let r = load_snapshot(&mut session, Engine::Oracle, None, "X", |_, _| true).await;
    assert!(matches!(r, Err(DbError::Unsupported(_))));
}
