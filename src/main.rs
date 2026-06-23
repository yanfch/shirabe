mod cli;
mod config;
mod db;
mod importers;
mod pricing;
pub mod projection;
mod rollup;
mod server;
mod skills;

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
        Commands::Serve { bind, ui_dir } => {
            database.migrate()?;
            if let Some(report) = rollup::refresh_if_missing(&database)? {
                tracing::info!(
                    source_rollups = report.source_rollups,
                    model_rollups = report.model_rollups,
                    "created missing usage rollups"
                );
            }
            let ui_dir = ui_dir.unwrap_or(paths.ui_dist);
            server::serve(paths.catalog_db, bind, ui_dir).await?;
        }
        Commands::Import { source, path } => {
            database.migrate()?;
            let report = match source {
                ImportSource::Codex => importers::codex::import(&database, path)?,
                ImportSource::Claude => importers::claude::import(&database, path)?,
                ImportSource::Kanade => importers::kanade::import(&database, path)?,
                ImportSource::Pi => importers::pi::import(&database, path)?,
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
