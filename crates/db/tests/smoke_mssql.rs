//! Smoke test for the docker compose `mssql` service (M0-2): the driver connects with
//! certificate verification on (trusting the CA that `mssql-tls` wrote to
//! `docker/mssql/tls/ca.pem`) and reads the seeded sample schema.
//!
//! ```text
//! docker compose -f docker/compose.yml up -d --wait mssql
//! docker compose -f docker/compose.yml up mssql-seed
//! cargo test -p switchyard-db --test smoke_mssql -- --ignored
//! ```
//!
//! `SWITCHYARD_SMOKE_MSSQL_PORT` (1433) and `SWITCHYARD_SMOKE_MSSQL_CA` override the defaults.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use futures::StreamExt;
use secrecy::SecretString;
use switchyard_db::batch::{BatchList, ColumnMeta};
use switchyard_db::driver::{DbConfig, DbSession, Driver, SslMode};
use switchyard_db::error::DbError;
use switchyard_db::mssql::MssqlDriver;
use switchyard_db::stream::ResultEvent;
use switchyard_db::value::Engine;

fn config(database: &str) -> DbConfig {
    let mut cfg = DbConfig::new(Engine::SqlServer, "localhost", database);
    cfg.port = std::env::var("SWITCHYARD_SMOKE_MSSQL_PORT")
        .map(|p| p.parse().unwrap())
        .unwrap_or(1433);
    cfg.user = "sa".into();
    cfg.password = Some(SecretString::from("Switchyard!2026".to_owned()));
    cfg.ssl_mode = SslMode::Require;
    let ca = std::env::var("SWITCHYARD_SMOKE_MSSQL_CA").unwrap_or_else(|_| {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../docker/mssql/tls/ca.pem").to_owned()
    });
    cfg.trusted_ca_pem = Some(
        std::fs::read_to_string(&ca)
            .unwrap_or_else(|e| panic!("{ca}: {e} (start the compose mssql service first)")),
    );
    cfg
}

/// The first cell of the first row, as displayed.
async fn scalar(s: &mut dyn DbSession, sql: &str) -> Result<String, DbError> {
    let mut stream = s.execute(sql, &[]).await?;
    let mut cols: Vec<ColumnMeta> = Vec::new();
    let mut rows = BatchList::default();
    while let Some(ev) = stream.next().await {
        match ev? {
            ResultEvent::Columns(c) if cols.is_empty() => cols = c.to_vec(),
            ResultEvent::Rows(b) if rows.is_empty() => rows.push(b),
            _ => {}
        }
    }
    let cell = rows.cell(0, 0).expect("one row");
    Ok(cell.to_value(cols[0].data_type).to_display())
}

#[tokio::test]
#[ignore = "needs the docker compose mssql service"]
async fn compose_sql_server_accepts_a_verified_tls_login() {
    let mut s = MssqlDriver
        .connect(&config("master"), None)
        .await
        .expect("connect");
    assert!(
        s.server_version().starts_with("SQL Server"),
        "{}",
        s.server_version()
    );
    assert_eq!(
        scalar(
            s.as_mut(),
            "SELECT encrypt_option FROM sys.dm_exec_connections WHERE session_id = @@SPID"
        )
        .await
        .unwrap(),
        "TRUE"
    );

    // `mssql-seed` runs once the server is healthy; give it time to finish.
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        match scalar(s.as_mut(), "SELECT COUNT(*) FROM shop.dbo.customers").await {
            Ok(n) if n == "1000" => break,
            other if Instant::now() > deadline => panic!("shop not seeded: {other:?}"),
            _ => tokio::time::sleep(Duration::from_secs(2)).await,
        }
    }
}
