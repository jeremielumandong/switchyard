//! SQL Server integrated auth through core on Linux: the Driver Manager loads GSSAPI, the
//! user's Kerberos ticket becomes a service ticket for `MSSQLSvc/<server>:<port>`, and the
//! driver sends it in the login.
//!
//!   eval "$(scripts/kerberos-test-kdc.sh start)"
//!   echo "127.0.0.1 db.switchyard.test" | sudo tee -a /etc/hosts
//!   SSL_CERT_FILE=<CA that signed SQL Server's certificate> SSL_CERT_DIR=/nonexistent \
//!     cargo test -p switchyard-core --test integrated -- --ignored --test-threads 1
//!
//! SQL Server's certificate must name db.switchyard.test (`scripts/mssql-test-server.sh`
//! issues one that does). `SSL_CERT_DIR` is overridden because cargo points it at the
//! system store, which would hide a missing CA.
//! The test KDC is not Active Directory, so SQL Server can't accept the ticket; the test
//! proves everything up to the server's verdict and that the verdict reads well.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use futures::StreamExt;
use switchyard_core::db::{DbAuthMethod, Engine, SslMode};
use switchyard_core::store::DbConnection;
use switchyard_core::{Command, Core, Event, ServiceConfig};

async fn test_connection(server: &str) -> Result<String, String> {
    let (core, mut rx) = Core::start(ServiceConfig::in_memory()).unwrap();
    let mut c = DbConnection::new("ad", Engine::SqlServer);
    c.server = server.into();
    c.port = 1433;
    c.database = "master".into();
    c.auth = DbAuthMethod::Integrated;
    c.ssl_mode = SslMode::Prefer;
    core.handle().send(Command::TestConnection {
        request: 1,
        connection: c,
        secret: None,
    });
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Event::TestResult { request: 1, result } = rx.next().await.unwrap() {
                return result;
            }
        }
    })
    .await
    .expect("a test result")
}

#[tokio::test]
#[ignore = "needs scripts/kerberos-test-kdc.sh and SQL Server on localhost"]
async fn the_ticket_reaches_sql_server() {
    let err = test_connection("db.switchyard.test")
        .await
        .expect_err("the test KDC is not Active Directory");
    // Kerberos itself succeeded (a service ticket was issued) and TLS was fine; SQL Server
    // read the ticket and refused the login (18452: not an Active Directory domain).
    assert!(!err.contains("Kerberos could not"), "{err}");
    assert!(!err.contains("TLS"), "{err}");
    assert!(err.contains("Login failed"), "{err}");
}

#[tokio::test]
#[ignore = "needs scripts/kerberos-test-kdc.sh"]
async fn a_server_unknown_to_kerberos_is_explained() {
    let err = test_connection("localhost")
        .await
        .expect_err("no MSSQLSvc/localhost principal");
    assert!(
        err.contains("Kerberos could not sign in to the server"),
        "{err}"
    );
}
