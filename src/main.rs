mod cli;
mod config;
mod db;
mod importers;
mod pricing;
pub mod projection;
mod rollup;
mod server;
mod skills;
mod workspace;

use std::time::Duration;

use anyhow::{Result, bail};
use clap::Parser;
use cli::{Cli, Commands, ImportSource, PricingCommand, RollupCommand, WorkspaceCommand};
use config::{Paths, is_shared_workspace_dir};
use db::Database;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    let paths = Paths::resolve(cli.data_dir)?;

    match cli.command {
        Commands::Init => {
            ensure_not_shared_workspace_direct_write(&paths, "init")?;
            let (_database, _adopted_legacy_profile) = open_registered_database(&paths)?;
            println!("initialized {}", paths.data_dir.display());
            println!("config {}", paths.config_dir.display());
            println!("catalog {}", paths.catalog_db.display());
        }
        Commands::Serve {
            bind,
            ui_dir,
            exit_when_parent_exits,
        } => {
            let (database, adopted_legacy_profile) = open_registered_database(&paths)?;
            let rollup_report = if adopted_legacy_profile {
                Some(rollup::refresh_all(&database)?)
            } else {
                rollup::refresh_if_missing(&database)?
            };
            if let Some(report) = rollup_report {
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
            server::serve(
                paths.catalog_db,
                bind,
                ui_dir,
                paths.source_paths,
                paths.source_settings,
                paths.identity,
            )
            .await?;
        }
        Commands::Import { source, path } => {
            ensure_not_shared_workspace_direct_write(&paths, "import")?;
            let (database, _adopted_legacy_profile) = open_registered_database(&paths)?;
            let import_identity = importers::ImportIdentity::new(
                paths.identity.profile_id.clone(),
                paths.identity.device_id.clone(),
            );
            let report = match source {
                ImportSource::Amp => match path {
                    Some(path) => importers::amp::import_local_with_identity(
                        &database,
                        path,
                        &import_identity,
                    )?,
                    None => importers::amp::sync_cli_first_with_identity(
                        &database,
                        &paths.source_paths.amp,
                        &import_identity,
                    )?,
                },
                ImportSource::Codex => importers::codex::import_with_identity(
                    &database,
                    path.or_else(|| Some(paths.source_paths.codex.clone())),
                    &import_identity,
                )?,
                ImportSource::Claude => importers::claude::import_with_identity(
                    &database,
                    path.or_else(|| Some(paths.source_paths.claude.clone())),
                    &import_identity,
                )?,
                ImportSource::Kanade => importers::kanade::import_with_identity(
                    &database,
                    path.or_else(|| Some(paths.source_paths.kanade.clone())),
                    &import_identity,
                )?,
                ImportSource::Pi => importers::pi::import_with_identity(
                    &database,
                    path.or_else(|| Some(paths.source_paths.pi.clone())),
                    &import_identity,
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
        Commands::Collect { workspace } => {
            let report = workspace::collect_to_inbox(
                workspace,
                &paths.source_paths,
                &paths.source_settings,
                &paths.identity,
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Commands::Pricing { command } => {
            ensure_not_shared_workspace_direct_write(&paths, "pricing refresh")?;
            let (database, _adopted_legacy_profile) = open_registered_database(&paths)?;
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
            ensure_not_shared_workspace_direct_write(&paths, "rollup refresh")?;
            let (database, _adopted_legacy_profile) = open_registered_database(&paths)?;
            let report = match command {
                RollupCommand::Refresh => rollup::refresh_all(&database)?,
            };
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Commands::Workspace { command } => {
            let workspace_path = match command {
                WorkspaceCommand::Init { path } | WorkspaceCommand::Repair { path } => path,
            };
            let report = workspace::prepare(workspace_path)?;
            let shared_paths = Paths::resolve(Some(report.workspace_dir.clone()))?;
            let shared_db = Database::open(&shared_paths.catalog_db)?;
            shared_db.migrate()?;
            shared_db.register_identity(&shared_paths.identity)?;
            let mut pricing_changed = workspace::seed_pricing_cache(&shared_db, &paths.catalog_db)?;
            if !pricing_changed {
                pricing_changed = workspace::seed_pricing_cache_from_default_local(&shared_db)?;
            }
            if pricing_changed {
                rollup::refresh_all(&shared_db)?;
            }
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
    }

    Ok(())
}

fn open_registered_database(paths: &Paths) -> Result<(Database, bool)> {
    let database = Database::open(&paths.catalog_db)?;
    database.migrate()?;
    database.register_identity(&paths.identity)?;
    let adopted_legacy_profile = if is_shared_workspace_dir(&paths.data_dir) {
        false
    } else {
        database.adopt_legacy_local_profile(&paths.identity)?
    };
    let removed_duplicate_pi_calls = database.migrate_pi_llm_event_ids()?;
    if removed_duplicate_pi_calls > 0 {
        tracing::info!(
            removed_duplicate_pi_calls,
            "removed duplicated Pi usage copied across sessions"
        );
    }
    Ok((database, adopted_legacy_profile))
}

fn ensure_not_shared_workspace_direct_write(paths: &Paths, action: &str) -> Result<()> {
    if is_shared_workspace_dir(&paths.data_dir) {
        bail!(
            "shared workspace {} is server-owned; run `shirabe serve` for DB writes and use `shirabe collect --workspace {}` from other accounts instead of direct `{action}`",
            paths.data_dir.display(),
            paths.data_dir.display()
        );
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
