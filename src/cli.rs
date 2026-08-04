use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "shirabe")]
#[command(about = "Local AI operations diagnostics")]
pub struct Cli {
    /// Data directory for catalog.sqlite, shards, and importer state.
    #[arg(long, env = "SHIRABE_DIR")]
    pub data_dir: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Initialize the local shirabe database.
    Init,

    /// Start the local API and web server.
    Serve {
        /// Address to bind.
        #[arg(long, default_value = "127.0.0.1:7778")]
        bind: String,

        /// Directory containing built UI assets.
        #[arg(long)]
        ui_dir: Option<PathBuf>,

        /// Exit when this parent process is no longer alive.
        #[arg(long, hide = true)]
        exit_when_parent_exits: Option<u32>,
    },

    /// Import historical local AI data.
    Import {
        #[arg(value_enum)]
        source: ImportSource,

        /// Source path. Defaults depend on the importer.
        #[arg(long)]
        path: Option<PathBuf>,
    },

    /// Collect current-account events into a shared workspace inbox.
    Collect {
        /// Shared workspace directory.
        #[arg(long)]
        workspace: Option<PathBuf>,
    },

    /// Manage model pricing cache.
    Pricing {
        #[command(subcommand)]
        command: PricingCommand,
    },

    /// Rebuild usage rollups from imported operation tables.
    Rollup {
        #[command(subcommand)]
        command: RollupCommand,
    },

    /// Manage the local shared workspace.
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCommand,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum ImportSource {
    Amp,
    Codex,
    Claude,
    Kanade,
    Pi,
}

#[derive(Debug, Subcommand)]
pub enum PricingCommand {
    /// Fetch and cache public LiteLLM model pricing.
    Refresh {
        /// Pricing JSON URL. Defaults to LiteLLM's public pricing snapshot.
        #[arg(long)]
        url: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum RollupCommand {
    /// Rebuild all usage rollups.
    Refresh,
}

#[derive(Debug, Subcommand)]
pub enum WorkspaceCommand {
    /// Create or repair the default shared workspace for multi-account local use.
    Init {
        /// Shared workspace directory.
        #[arg(long)]
        path: Option<PathBuf>,
    },

    /// Re-apply shared workspace permissions.
    Repair {
        /// Shared workspace directory.
        #[arg(long)]
        path: Option<PathBuf>,
    },
}
