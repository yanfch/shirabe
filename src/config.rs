use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct Paths {
    pub data_dir: PathBuf,
    pub catalog_db: PathBuf,
    pub ui_dist: PathBuf,
    pub source_paths: SourcePaths,
}

#[derive(Debug, Clone)]
pub struct SourcePaths {
    pub codex: PathBuf,
    pub pi: PathBuf,
    pub claude: PathBuf,
    pub kanade: PathBuf,
}

impl Paths {
    pub fn resolve(data_dir: Option<PathBuf>) -> Result<Self> {
        let data_dir = data_dir.unwrap_or_else(default_data_dir);
        let data_dir = expand_tilde(data_dir)?;
        let source_paths = SourcePaths::resolve(&data_dir)?;
        let catalog_db = data_dir.join("catalog.sqlite");
        let ui_dist = env::current_dir()
            .context("resolve current directory")?
            .join("ui")
            .join("dist");

        Ok(Self {
            data_dir,
            catalog_db,
            ui_dist,
            source_paths,
        })
    }
}

impl SourcePaths {
    fn resolve(data_dir: &Path) -> Result<Self> {
        let config = ConfigFile::load(data_dir)?;
        let sources = config.sources.unwrap_or_default();

        Ok(Self {
            codex: configured_path(
                sources.codex,
                &["SHIRABE_CODEX_SESSIONS_DIR", "CODEX_SESSIONS_DIR"],
                &[
                    ("SHIRABE_CODEX_DIR", &["sessions"]),
                    ("CODEX_DIR", &["sessions"]),
                ],
                &[".codex", "sessions"],
            )?,
            pi: configured_path(
                sources.pi,
                &["SHIRABE_PI_SESSIONS_DIR", "PI_SESSIONS_DIR"],
                &[
                    ("SHIRABE_PI_DIR", &["agent", "sessions"]),
                    ("PI_DIR", &["agent", "sessions"]),
                ],
                &[".pi", "agent", "sessions"],
            )?,
            claude: configured_path(
                sources.claude,
                &["SHIRABE_CLAUDE_PROJECTS_DIR", "CLAUDE_PROJECTS_DIR"],
                &[
                    ("SHIRABE_CLAUDE_DIR", &["projects"]),
                    ("CLAUDE_DIR", &["projects"]),
                ],
                &[".claude", "projects"],
            )?,
            kanade: configured_path(
                sources.kanade,
                &["SHIRABE_KANADE_TRACES_DIR", "KANADE_TRACES_DIR"],
                &[
                    ("SHIRABE_KANADE_DIR", &["traces"]),
                    ("KANADE_DIR", &["traces"]),
                ],
                &[".kanade", "traces"],
            )?,
        })
    }
}

#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    sources: Option<SourcePathConfig>,
}

#[derive(Debug, Default, Deserialize)]
struct SourcePathConfig {
    codex: Option<PathBuf>,
    pi: Option<PathBuf>,
    claude: Option<PathBuf>,
    kanade: Option<PathBuf>,
}

impl ConfigFile {
    fn load(data_dir: &Path) -> Result<Self> {
        let path = data_dir.join("config.json");
        if !path.exists() {
            return Ok(Self::default());
        }

        let raw = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))
    }
}

fn default_data_dir() -> PathBuf {
    env::var_os("SHIRABE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = env::var_os("HOME").unwrap_or_else(|| ".".into());
            PathBuf::from(home).join(".shirabe")
        })
}

fn expand_tilde(path: PathBuf) -> Result<PathBuf> {
    let raw = path.to_string_lossy();
    if raw == "~" {
        let home = env::var_os("HOME").context("HOME is not set")?;
        return Ok(PathBuf::from(home));
    }

    if let Some(rest) = raw.strip_prefix("~/") {
        let home = env::var_os("HOME").context("HOME is not set")?;
        return Ok(Path::new(&home).join(rest));
    }

    Ok(path)
}

fn configured_path(
    config_path: Option<PathBuf>,
    exact_envs: &[&str],
    root_envs: &[(&str, &[&str])],
    home_suffix: &[&str],
) -> Result<PathBuf> {
    if let Some(path) = config_path {
        return expand_tilde(path);
    }

    for env_name in exact_envs {
        if let Some(path) = env::var_os(env_name) {
            return expand_tilde(PathBuf::from(path));
        }
    }

    for (env_name, suffix) in root_envs {
        if let Some(root) = env::var_os(env_name) {
            return expand_tilde(join_segments(PathBuf::from(root), suffix));
        }
    }

    if let Some(home) = env::var_os("HOME") {
        return Ok(join_segments(PathBuf::from(home), home_suffix));
    }

    Ok(join_segments(PathBuf::new(), home_suffix))
}

fn join_segments(mut path: PathBuf, segments: &[&str]) -> PathBuf {
    for segment in segments {
        path.push(segment);
    }
    path
}
