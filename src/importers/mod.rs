use std::io::Write;
use std::path::PathBuf;

use crate::projection::event::NormalizedEvent;
use anyhow::Result;
use serde::Serialize;

pub mod amp;
pub mod claude;
pub mod codex;
pub mod kanade;
pub mod pi;

#[derive(Debug, Serialize)]
pub struct ImportReport {
    pub source: String,
    pub source_id: String,
    pub root_path: PathBuf,
    pub files_seen: usize,
    pub files_imported: usize,
    pub files_skipped: usize,
    pub events_projected: usize,
    pub source_bytes_scanned: u64,
    pub shirabe_bytes_written: u64,
    pub timings: Vec<ImportTiming>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ImportTiming {
    pub stage: &'static str,
    pub elapsed_ms: u128,
}

#[derive(Debug, Clone)]
pub struct ImportIdentity {
    pub profile_id: String,
    pub device_id: String,
}

impl ImportIdentity {
    pub fn new(profile_id: impl Into<String>, device_id: impl Into<String>) -> Self {
        Self {
            profile_id: profile_id.into(),
            device_id: device_id.into(),
        }
    }

    pub fn local() -> Self {
        Self::new("local", "local_device")
    }
}

pub fn write_collected_event(writer: &mut dyn Write, event: &NormalizedEvent) -> Result<()> {
    serde_json::to_writer(&mut *writer, event)?;
    writer.write_all(b"\n")?;
    Ok(())
}
