//! `swy`: Switchyard's command-line tool and MCP server. It drives the same
//! `switchyard-core` as the app, so connections, guards and history behave the same.
//!
//! Secrets come from the OS keychain. Without one (servers, CI), set
//! `SWITCHYARD_SECRETS=vault` and `SWITCHYARD_VAULT_PASSWORD` to use the fallback vault.

mod client;
mod mcp;
mod output;
mod render;

use std::io::{IsTerminal as _, Read as _};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context as _, Result, anyhow, bail};
use clap::{Parser, Subcommand, ValueEnum};
use switchyard_core::QueryEvent;
use switchyard_core::agent_run::{TOKEN_ENV, verify_token};
use switchyard_core::handoff::{self, Handoff, HandoffError};
use switchyard_core::store::AppPaths;

use client::Client;
use output::{Format, ResultWriter};

#[derive(Parser)]
#[command(name = "swy", version, about = "Switchyard command-line tool")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum PlanFormat {
    /// An indented operator tree with hotspots.
    Text,
    /// The normalized plan and findings as JSON.
    Json,
}

#[derive(Subcommand)]
enum Cmd {
    /// List saved database connections (names only: no hosts, users or secrets).
    Connections {
        /// Output JSON.
        #[arg(long)]
        json: bool,
    },
    /// Run SQL on a connection and print the results.
    Query {
        /// Connection name.
        connection: String,
        /// SQL text. Omit to read `--file` or standard input.
        sql: Option<String>,
        /// Read SQL from this file.
        #[arg(short, long, conflicts_with = "sql")]
        file: Option<PathBuf>,
        /// Output format.
        #[arg(long, value_enum, default_value_t = Format::Table)]
        format: Format,
        /// Rows shown per result set in a table (CSV and JSON print every row).
        #[arg(long, default_value_t = 1000)]
        limit: usize,
        /// Run destructive statements on Production without asking.
        #[arg(long)]
        yes: bool,
    },
    /// Capture and print a query plan with its hotspots.
    Explain {
        /// Connection name.
        connection: String,
        /// The statement. Omit to read `--file` or standard input.
        sql: Option<String>,
        /// Read the statement from this file.
        #[arg(short, long, conflicts_with = "sql")]
        file: Option<PathBuf>,
        /// Run the statement for an actual plan (inside a transaction that is rolled back).
        #[arg(long)]
        analyze: bool,
        /// Also show the plan in the running Switchyard app.
        #[arg(long)]
        open: bool,
        /// Output format.
        #[arg(long, value_enum, default_value_t = PlanFormat::Text)]
        format: PlanFormat,
        /// Allow an actual plan of a writing statement on Production.
        #[arg(long)]
        yes: bool,
    },
    /// Show the busiest statements and table / index usage statistics.
    Workload {
        /// Connection name.
        connection: String,
        /// Output JSON.
        #[arg(long)]
        json: bool,
    },
    /// Serve the MCP tools for coding agents on standard input / output.
    Mcp,
}

fn read_sql(sql: Option<String>, file: Option<PathBuf>) -> Result<String> {
    let text = match (sql, file) {
        (Some(s), _) => s,
        (None, Some(f)) => {
            std::fs::read_to_string(&f).with_context(|| format!("reading {}", f.display()))?
        }
        (None, None) => {
            if std::io::stdin().is_terminal() {
                bail!("pass SQL as an argument, with --file, or on standard input");
            }
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s)?;
            s
        }
    };
    if text.trim().is_empty() {
        bail!("no SQL given");
    }
    Ok(text)
}

async fn connections(json: bool) -> Result<()> {
    let client = Client::start().await?;
    let rows: Vec<_> = client.connections();
    if json {
        let v: Vec<_> = rows
            .iter()
            .map(|c| {
                serde_json::json!({
                    "name": c.name,
                    "engine": c.engine.display_name(),
                    "environment": c.environment.name(),
                    "agent_access": c.agent_access,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        for c in rows {
            println!(
                "{}\t{}\t{}",
                c.name,
                c.engine.display_name(),
                c.environment.name()
            );
        }
    }
    Ok(())
}

async fn query(connection: &str, sql: &str, format: Format, limit: usize, yes: bool) -> Result<()> {
    let mut client = Client::start().await?;
    let conn = client.connection(connection)?;
    let session = client.open(&conn).await?;
    let mut w = ResultWriter::new(std::io::stdout().lock(), format, limit);
    let mut had_rows = false;
    let r = client
        .execute(session, conn.engine, sql, yes, vec!["cli".into()], |ev| {
            match ev {
                QueryEvent::StatementStarted { .. } => had_rows = false,
                QueryEvent::Columns(cols) => {
                    had_rows = true;
                    w.columns(cols)?;
                }
                QueryEvent::Rows(batch) => w.rows(&batch)?,
                QueryEvent::Notice(n) => eprintln!("{}", n.message),
                QueryEvent::StatementDone { completion, .. } if !had_rows => {
                    if let Some(n) = completion.affected {
                        eprintln!("{n} row{} affected", if n == 1 { "" } else { "s" });
                    }
                }
                _ => {}
            }
            Ok(())
        })
        .await;
    drop(w.finish()?);
    client.close(session);
    r
}

#[allow(clippy::too_many_arguments)]
async fn explain(
    connection: &str,
    sql: &str,
    analyze: bool,
    open: bool,
    format: PlanFormat,
    yes: bool,
) -> Result<()> {
    let mut client = Client::start().await?;
    let conn = client.connection(connection)?;
    let session = client.open(&conn).await?;
    let r = client
        .explain(session, sql, analyze, yes, vec!["cli".into()])
        .await;
    client.close(session);
    let r = r?;
    match format {
        PlanFormat::Text => print!("{}", render::plan_text(&r.plan, &r.findings)),
        PlanFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "plan": &*r.plan,
                "findings": r.findings,
                "history_id": r.history_id,
            }))?
        ),
    }
    if open {
        let Some(history_id) = r.history_id else {
            bail!(
                "the plan was not saved (history is off for this connection), so it cannot be opened in the app"
            );
        };
        let paths = AppPaths::resolve().context("could not determine a home directory")?;
        let request = Handoff::OpenPlan {
            history_id,
            sql: sql.to_owned(),
        };
        match tokio::task::spawn_blocking(move || handoff::send(&paths.data, request)).await? {
            Ok(()) => eprintln!("Opened in Switchyard."),
            Err(HandoffError::NotRunning) => eprintln!(
                "Switchyard is not running; the plan is in its history (Plan history) when you start it."
            ),
            Err(e) => bail!("could not open the plan in Switchyard: {e}"),
        }
    }
    Ok(())
}

async fn workload(connection: &str, json: bool) -> Result<()> {
    let mut client = Client::start().await?;
    let conn = client.connection(connection)?;
    let session = client.open(&conn).await?;
    let r = client.workload(session).await;
    client.close(session);
    let w = r?;
    if json {
        println!("{}", serde_json::to_string_pretty(&*w)?);
    } else {
        print!("{}", render::workload_text(&w));
    }
    Ok(())
}

async fn mcp() -> Result<()> {
    let client = Client::start().await?;
    // A run started by the app carries a session token naming its connections.
    let session = match std::env::var(TOKEN_ENV) {
        Ok(token) if !token.trim().is_empty() => {
            let scope = verify_token(client.data_dir(), &token).map_err(|e| anyhow!("{e}"))?;
            Some(mcp::Session { token, scope })
        }
        _ => None,
    };
    let mut tools = mcp::Tools::new(client, session);
    let r = mcp::server::serve(
        &mut tools,
        tokio::io::BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
    )
    .await;
    tools.close();
    r
}

async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Cmd::Connections { json } => connections(json).await,
        Cmd::Query {
            connection,
            sql,
            file,
            format,
            limit,
            yes,
        } => query(&connection, &read_sql(sql, file)?, format, limit, yes).await,
        Cmd::Explain {
            connection,
            sql,
            file,
            analyze,
            open,
            format,
            yes,
        } => {
            explain(
                &connection,
                &read_sql(sql, file)?,
                analyze,
                open,
                format,
                yes,
            )
            .await
        }
        Cmd::Workload { connection, json } => workload(&connection, json).await,
        Cmd::Mcp => mcp().await,
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // Logs go to stderr (stdout carries results and MCP messages); quiet unless RUST_LOG.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("error")),
        )
        .init();
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("swy: {e}");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(run(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("swy: {e:#}");
            ExitCode::FAILURE
        }
    }
}
