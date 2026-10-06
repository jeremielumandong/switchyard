//! `swy`: Switchyard's command-line tool. It shares `switchyard-core` with the app.
//!
//! Implemented now: `swy connections`. `query`, `explain`, `workload` and `mcp` arrive
//! with milestone M5.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use switchyard_core::store::{AppPaths, Profile, Store};

#[derive(Parser)]
#[command(name = "swy", version, about = "Switchyard command-line tool")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List saved connections (names only: no hosts, users or secrets).
    Connections {
        /// Output JSON.
        #[arg(long)]
        json: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Cmd::Connections { json } => {
            let paths = AppPaths::resolve().context("no home directory")?;
            let store = Store::open(&paths.store_file())?;
            let rows: Vec<(String, String, String)> = store
                .profiles()?
                .into_iter()
                .filter_map(|p| match p {
                    Profile::Db(d) => Some((
                        d.name,
                        d.engine.display_name().to_owned(),
                        d.environment.name().to_owned(),
                    )),
                    _ => None,
                })
                .collect();
            if json {
                let v: Vec<_> = rows
                    .iter()
                    .map(|(n, e, env)| serde_json::json!({"name": n, "engine": e, "environment": env}))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                for (n, e, env) in rows {
                    println!("{n}\t{e}\t{env}");
                }
            }
        }
    }
    Ok(())
}
