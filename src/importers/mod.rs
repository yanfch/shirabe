use std::path::PathBuf;

use serde::Serialize;

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
