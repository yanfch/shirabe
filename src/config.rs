use std::{
    env,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Paths {
    pub data_dir: PathBuf,
    pub catalog_db: PathBuf,
    pub ui_dist: PathBuf,
}

impl Paths {
    pub fn resolve(data_dir: Option<PathBuf>) -> Result<Self> {
        let data_dir = data_dir.unwrap_or_else(default_data_dir);
        let data_dir = expand_tilde(data_dir)?;
        let catalog_db = data_dir.join("catalog.sqlite");
        let ui_dist = env::current_dir()
            .context("resolve current directory")?
            .join("ui")
            .join("dist");

        Ok(Self {
            data_dir,
            catalog_db,
            ui_dist,
        })
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
