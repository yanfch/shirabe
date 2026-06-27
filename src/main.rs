mod cli;
mod config;
mod db;
mod importers;
mod pricing;
pub mod projection;
mod rollup;
mod server;
mod skills;

use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Commands, ImportSource, PricingCommand, RollupCommand};
use config::Paths;
use db::Database;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    let paths = Paths::resolve(cli.data_dir)?;
    let database = Database::open(&paths.catalog_db)?;

    match cli.command {
        Commands::Init => {
            database.migrate()?;
            println!("initialized {}", paths.data_dir.display());
            println!("catalog {}", paths.catalog_db.display());
        }
        Commands::Serve {
            bind,
            ui_dir,
            exit_when_parent_exits,
        } => {
            database.migrate()?;
            if let Some(report) = rollup::refresh_if_missing(&database)? {
                tracing::info!(
                    source_rollups = report.source_rollups,
                    model_rollups = report.model_rollups,
                    "created missing usage rollups"
                );
            }
            if let Some(parent_pid) = exit_when_parent_exits {
                spawn_parent_exit_watchdog(parent_pid);
            }
            let ui_dir = ui_dir.unwrap_or(paths.ui_dist);
            server::serve(paths.catalog_db, bind, ui_dir, paths.source_paths).await?;
        }
        Commands::Import { source, path } => {
            database.migrate()?;
            let report = match source {
                ImportSource::Codex => importers::codex::import(
                    &database,
                    path.or_else(|| Some(paths.source_paths.codex.clone())),
                )?,
                ImportSource::Claude => importers::claude::import(
                    &database,
                    path.or_else(|| Some(paths.source_paths.claude.clone())),
                )?,
                ImportSource::Kanade => importers::kanade::import(
                    &database,
                    path.or_else(|| Some(paths.source_paths.kanade.clone())),
                )?,
                ImportSource::Pi => importers::pi::import(
                    &database,
                    path.or_else(|| Some(paths.source_paths.pi.clone())),
                )?,
            };
            let rollup_report = rollup::refresh_all(&database)?;
            tracing::info!(
                source_rollups = rollup_report.source_rollups,
                model_rollups = rollup_report.model_rollups,
                tool_rollups = rollup_report.tool_rollups,
                "refreshed usage rollups"
            );
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Commands::Pricing { command } => {
            database.migrate()?;
            let report = match command {
                PricingCommand::Refresh { url } => {
                    pricing::refresh_litellm_pricing(
                        &database,
                        url.as_deref().unwrap_or(pricing::LITELLM_PRICING_URL),
                    )
                    .await?
                }
            };
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Commands::Rollup { command } => {
            database.migrate()?;
            let report = match command {
                RollupCommand::Refresh => rollup::refresh_all(&database)?,
            };
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
    }

    Ok(())
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("shirabe=info,tower_http=warn"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}

fn spawn_parent_exit_watchdog(parent_pid: u32) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if !process_exists(parent_pid) {
                tracing::info!(parent_pid, "parent process exited; stopping bundled server");
                std::process::exit(0);
            }
        }
    });
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }

    let result = unsafe { kill(pid as i32, 0) };
    if result == 0 {
        return true;
    }

    std::io::Error::last_os_error().raw_os_error() == Some(1)
}

#[cfg(not(unix))]
fn process_exists(_pid: u32) -> bool {
    true
}
