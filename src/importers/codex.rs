use std::{
    collections::{HashMap, HashSet},
    env, fs,
    io::{BufRead, BufReader, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::Instant,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use walkdir::WalkDir;

use crate::db::{Database, now_ns, stable_hash, system_time_ns};
use crate::projection::{
    event::{
        EntityHint, NormalizedEvent, Operation, OperationStatus, OperationType, TraceContext,
        TurnHint, Usage,
    },
    projector::{ProjectionCache, Projector},
};
use crate::skills;

use super::{ImportIdentity, ImportReport, ImportTiming, write_collected_event};

#[allow(dead_code)]
pub fn import(db: &Database, path: Option<PathBuf>) -> Result<ImportReport> {
    import_with_identity(db, path, &ImportIdentity::local())
}

pub fn import_with_identity(
    db: &Database,
    path: Option<PathBuf>,
    identity: &ImportIdentity,
) -> Result<ImportReport> {
    import_with_modified_since(db, path, None, identity)
}

#[allow(dead_code)]
pub fn import_recent(
    db: &Database,
    path: Option<PathBuf>,
    modified_since_ns: i64,
) -> Result<ImportReport> {
    import_recent_with_identity(db, path, modified_since_ns, &ImportIdentity::local())
}

pub fn import_recent_with_identity(
    db: &Database,
    path: Option<PathBuf>,
    modified_since_ns: i64,
    identity: &ImportIdentity,
) -> Result<ImportReport> {
    import_with_modified_since(db, path, Some(modified_since_ns), identity)
}

#[allow(dead_code)]
pub fn collect(
    path: Option<PathBuf>,
    identity: &ImportIdentity,
    writer: &mut dyn Write,
) -> Result<ImportReport> {
    collect_with_modified_since(path, identity, writer, None)
}

pub fn collect_recent(
    path: Option<PathBuf>,
    identity: &ImportIdentity,
    writer: &mut dyn Write,
    modified_since_ns: i64,
) -> Result<ImportReport> {
    collect_with_modified_since(path, identity, writer, Some(modified_since_ns))
}

fn collect_with_modified_since(
    path: Option<PathBuf>,
    identity: &ImportIdentity,
    writer: &mut dyn Write,
    modified_since_ns: Option<i64>,
) -> Result<ImportReport> {
    let total_started = Instant::now();
    let root_path = path.unwrap_or_else(default_codex_sessions_dir);
    let scan_started = Instant::now();
    let scan = collect_sessions(&root_path, identity, writer, modified_since_ns)?;
    let scan_elapsed_ms = scan_started.elapsed().as_millis();
    let warnings = if scan.warnings > 0 {
        vec![format!(
            "{} session lines could not be parsed and were skipped",
            scan.warnings
        )]
    } else {
        Vec::new()
    };

    Ok(ImportReport {
        source: "codex".to_string(),
        source_id: format!("{}:codex:collector", identity.profile_id),
        root_path,
        files_seen: scan.files_seen,
        files_imported: scan.files_seen,
        files_skipped: 0,
        events_projected: scan.events,
        source_bytes_scanned: scan.bytes,
        shirabe_bytes_written: 0,
        mode: None,
        threads_enumerated: None,
        threads_exported: None,
        unchanged_threads_skipped: None,
        timings: vec![
            ImportTiming {
                stage: "collect",
                elapsed_ms: scan_elapsed_ms,
            },
            ImportTiming {
                stage: "total_collector",
                elapsed_ms: total_started.elapsed().as_millis(),
            },
        ],
        warnings,
    })
}

fn import_with_modified_since(
    db: &Database,
    path: Option<PathBuf>,
    modified_since_ns: Option<i64>,
    identity: &ImportIdentity,
) -> Result<ImportReport> {
    let total_started = Instant::now();
    let root_path = path.unwrap_or_else(default_codex_sessions_dir);
    let source_kind = if root_path.is_file() {
        "session_jsonl_file"
    } else {
        "session_jsonl_directory"
    };
    let source_id = db.record_import_source_for_profile(
        "codex",
        source_kind,
        &root_path,
        &identity.profile_id,
        &identity.device_id,
    )?;
    let rebuilding_rollout_events = if root_path.is_dir() {
        db.begin_codex_rollout_rebuild(&identity.profile_id, &identity.device_id)?
    } else {
        false
    };

    let scan_started = Instant::now();
    let tx = db.begin_batch()?;
    let scan = scan_sessions(
        db,
        &source_id,
        &root_path,
        if rebuilding_rollout_events {
            None
        } else {
            modified_since_ns
        },
        identity,
    )?;
    tx.commit()?;
    if rebuilding_rollout_events {
        db.finish_codex_rollout_rebuild(&identity.profile_id, &identity.device_id)?;
    }
    let scan_elapsed_ms = scan_started.elapsed().as_millis();

    let catalog_started = Instant::now();
    let shirabe_bytes_written = catalog_size(db.path())?;
    let catalog_elapsed_ms = catalog_started.elapsed().as_millis();

    let mut warnings = vec![
        "codex importer uses metadata-first parsing and does not copy prompt, response, or tool output content".to_string(),
    ];
    if scan.warnings > 0 {
        warnings.push(format!(
            "{} session lines could not be parsed and were skipped",
            scan.warnings
        ));
    }

    Ok(ImportReport {
        source: "codex".to_string(),
        source_id,
        root_path,
        files_seen: scan.files_seen,
        files_imported: scan.files_imported,
        files_skipped: scan.files_skipped,
        events_projected: scan.events,
        source_bytes_scanned: scan.bytes,
        shirabe_bytes_written,
        mode: None,
        threads_enumerated: None,
        threads_exported: None,
        unchanged_threads_skipped: None,
        timings: vec![
            ImportTiming {
                stage: "scan_project",
                elapsed_ms: scan_elapsed_ms,
            },
            ImportTiming {
                stage: "catalog_size",
                elapsed_ms: catalog_elapsed_ms,
            },
            ImportTiming {
                stage: "total_importer",
                elapsed_ms: total_started.elapsed().as_millis(),
            },
        ],
        warnings,
    })
}

fn default_codex_sessions_dir() -> PathBuf {
    env::var_os("CODEX_SESSIONS_DIR")
        .map(PathBuf::from)
        .or_else(|| crate::config::home_dir().map(|home| home.join(".codex").join("sessions")))
        .unwrap_or_else(|| PathBuf::from(".codex").join("sessions"))
}

fn collect_sessions(
    root: &Path,
    identity: &ImportIdentity,
    writer: &mut dyn Write,
    modified_since_ns: Option<i64>,
) -> Result<ScanSize> {
    if root.is_file() {
        let metadata = fs::metadata(root).with_context(|| format!("stat {}", root.display()))?;
        let modified_ns = metadata.modified().map(system_time_ns).unwrap_or_default();
        if modified_since_ns.is_some_and(|since| modified_ns < since) {
            return Ok(ScanSize::default());
        }
        let collected = collect_session_file(root, identity, writer)?;
        return Ok(ScanSize {
            files_seen: 1,
            files_imported: 1,
            files_skipped: 0,
            bytes: metadata.len(),
            events: collected.events,
            warnings: collected.warnings,
        });
    }

    let mut files_seen = 0usize;
    let mut bytes = 0u64;
    let mut events = 0usize;
    let mut warnings = 0usize;

    if !root.exists() {
        return Ok(ScanSize::default());
    }

    for entry in WalkDir::new(root).follow_links(false).sort_by_file_name() {
        let entry = entry.with_context(|| format!("scan {}", root.display()))?;
        if entry.file_type().is_file() && is_jsonl(entry.path()) {
            let metadata = entry.metadata()?;
            let modified_ns = metadata.modified().map(system_time_ns).unwrap_or_default();
            if modified_since_ns.is_some_and(|since| modified_ns < since) {
                continue;
            }
            files_seen += 1;
            bytes = bytes.saturating_add(metadata.len());
            let collected = collect_session_file(entry.path(), identity, writer)?;
            events += collected.events;
            warnings += collected.warnings;
        }
    }

    Ok(ScanSize {
        files_seen,
        files_imported: files_seen,
        files_skipped: 0,
        bytes,
        events,
        warnings,
    })
}

fn collect_session_file(
    path: &Path,
    identity: &ImportIdentity,
    writer: &mut dyn Write,
) -> Result<ImportedFile> {
    let file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut state = SessionState::from_path(path);
    let mut line = String::new();
    let mut line_number = 0usize;
    let mut events = 0usize;
    let mut warnings = 0usize;

    loop {
        line.clear();
        let bytes_read = reader
            .read_line(&mut line)
            .with_context(|| format!("read {}", path.display()))?;
        if bytes_read == 0 {
            break;
        }

        line_number += 1;
        match parse_session_line(&mut state, &line, path, line_number, identity) {
            Ok(parsed) => {
                for event in parsed {
                    write_collected_event(writer, &event)?;
                    events += 1;
                }
            }
            Err(_) => warnings += 1,
        }
    }

    Ok(ImportedFile { events, warnings })
}

fn scan_sessions(
    db: &Database,
    source_id: &str,
    root: &Path,
    modified_since_ns: Option<i64>,
    identity: &ImportIdentity,
) -> Result<ScanSize> {
    if root.is_file() {
        let metadata = fs::metadata(root).with_context(|| format!("stat {}", root.display()))?;
        let modified_ns = metadata.modified().map(system_time_ns).unwrap_or_default();
        if modified_since_ns.is_some_and(|since| modified_ns < since) {
            return Ok(ScanSize::default());
        }
        let prepared = db.prepare_import_file(source_id, root, metadata.len(), modified_ns)?;
        let imported = if prepared.should_import {
            import_session_file(db, &prepared, root, metadata.len(), identity)?
        } else {
            ImportedFile::skipped()
        };
        return Ok(ScanSize {
            files_seen: 1,
            files_imported: usize::from(prepared.should_import),
            files_skipped: usize::from(!prepared.should_import),
            bytes: metadata.len(),
            events: imported.events,
            warnings: imported.warnings,
        });
    }

    let mut files_seen = 0usize;
    let mut files_imported = 0usize;
    let mut files_skipped = 0usize;
    let mut bytes = 0u64;
    let mut events = 0usize;
    let mut warnings = 0usize;

    if !root.exists() {
        return Ok(ScanSize {
            files_seen,
            files_imported,
            files_skipped,
            bytes,
            events,
            warnings,
        });
    }

    for entry in WalkDir::new(root).follow_links(false).sort_by_file_name() {
        let entry = entry.with_context(|| format!("scan {}", root.display()))?;
        if entry.file_type().is_file() && is_jsonl(entry.path()) {
            let metadata = entry.metadata()?;
            let modified_ns = metadata.modified().map(system_time_ns).unwrap_or_default();
            if modified_since_ns.is_some_and(|since| modified_ns < since) {
                continue;
            }
            files_seen += 1;
            bytes = bytes.saturating_add(metadata.len());
            let prepared =
                db.prepare_import_file(source_id, entry.path(), metadata.len(), modified_ns)?;
            if !prepared.should_import {
                files_skipped += 1;
                continue;
            }

            files_imported += 1;
            let imported =
                import_session_file(db, &prepared, entry.path(), metadata.len(), identity)?;
            events += imported.events;
            warnings += imported.warnings;
        }
    }

    Ok(ScanSize {
        files_seen,
        files_imported,
        files_skipped,
        bytes,
        events,
        warnings,
    })
}

fn import_session_file(
    db: &Database,
    prepared: &crate::db::PreparedImportFile,
    path: &Path,
    size_bytes: u64,
    identity: &ImportIdentity,
) -> Result<ImportedFile> {
    let checkpoint = CodexImportCheckpoint::from_prepared(prepared, size_bytes);
    let mut file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    if checkpoint.start_offset > 0 {
        file.seek(SeekFrom::Start(checkpoint.start_offset))
            .with_context(|| format!("seek {}", path.display()))?;
    }
    let mut reader = BufReader::new(file);
    let projector = Projector::new(db);
    let mut state = checkpoint
        .state
        .unwrap_or_else(|| SessionState::from_path(path));
    let mut line = String::new();
    let mut line_number = checkpoint.line_number;
    let mut offset = checkpoint.start_offset;
    let mut events = 0usize;
    let mut warnings = 0usize;
    let mut first_event_ns: Option<i64> = None;
    let mut last_event_ns: Option<i64> = None;
    let mut projection_cache = ProjectionCache::default();
    let mut touched_session_ids = HashSet::new();
    let mut touched_run_ids = HashSet::new();

    loop {
        line.clear();
        let bytes_read = reader
            .read_line(&mut line)
            .with_context(|| format!("read {}", path.display()))?;
        if bytes_read == 0 {
            break;
        }

        line_number += 1;
        offset = offset.saturating_add(bytes_read as u64);

        match parse_session_line(&mut state, &line, path, line_number, identity) {
            Ok(parsed) => {
                for event in parsed {
                    let duplicate_llm_event =
                        matches!(event.operation.operation_type, OperationType::LlmCall)
                            && match event.source_event_id.as_deref() {
                                Some(source_event_id) => db
                                    .projected_llm_event_started_at_ns(
                                        &event.profile_id,
                                        &event.source,
                                        source_event_id,
                                    )?
                                    .is_some_and(|existing_started_at_ns| {
                                        existing_started_at_ns <= event.occurred_at_ns
                                    }),
                                None => false,
                            };
                    if duplicate_llm_event {
                        continue;
                    }
                    let occurred_at_ns = event.occurred_at_ns;
                    let projection = projector.project_with_cache(&event, &mut projection_cache)?;
                    if let Some(session_id) = projection.session_id.as_ref() {
                        touched_session_ids.insert(session_id.clone());
                    }
                    touched_run_ids.insert(projection.run_id);
                    events += 1;
                    first_event_ns = Some(
                        first_event_ns.map_or(occurred_at_ns, |value| value.min(occurred_at_ns)),
                    );
                    last_event_ns = Some(
                        last_event_ns.map_or(occurred_at_ns, |value| value.max(occurred_at_ns)),
                    );
                }
            }
            Err(_) => warnings += 1,
        }
    }

    projector.refresh_run_summaries(touched_run_ids.iter().map(String::as_str))?;
    db.refresh_observed_llm_latency_for_runs(touched_run_ids.iter().map(String::as_str))?;
    projector.refresh_session_summaries(touched_session_ids.iter().map(String::as_str))?;

    let status = if warnings > 0 { "partial" } else { "imported" };
    let cumulative_first_event_ns = match (checkpoint.first_event_ns, first_event_ns) {
        (Some(previous), Some(current)) => Some(previous.min(current)),
        (Some(previous), None) => Some(previous),
        (None, Some(current)) => Some(current),
        (None, None) => None,
    };
    let cumulative_last_event_ns = match (checkpoint.last_event_ns, last_event_ns) {
        (Some(previous), Some(current)) => Some(previous.max(current)),
        (Some(previous), None) => Some(previous),
        (None, Some(current)) => Some(current),
        (None, None) => None,
    };

    db.finish_import_file(
        &prepared.file_id,
        status,
        checkpoint.event_count.saturating_add(events),
        checkpoint.warning_count.saturating_add(warnings),
        Some(offset),
        cumulative_first_event_ns,
        cumulative_last_event_ns,
        None,
    )?;
    let checkpoint = CodexImportCheckpoint {
        start_offset: offset,
        line_number,
        event_count: checkpoint.event_count.saturating_add(events),
        warning_count: checkpoint.warning_count.saturating_add(warnings),
        first_event_ns: cumulative_first_event_ns,
        last_event_ns: cumulative_last_event_ns,
        state: Some(state),
    };
    db.update_import_file_metadata(&prepared.file_id, &serde_json::to_string(&checkpoint)?)?;

    Ok(ImportedFile { events, warnings })
}

fn parse_session_line(
    state: &mut SessionState,
    line: &str,
    path: &Path,
    line_number: usize,
    identity: &ImportIdentity,
) -> Result<Vec<NormalizedEvent>> {
    let value: Value = serde_json::from_str(line).context("parse codex session json")?;
    let timestamp_ns = value
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339_ns)
        .unwrap_or_else(now_ns);
    let outer_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let payload = value
        .get("payload")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    if outer_type == "session_meta" {
        state.apply_session_meta(&payload);
        return Ok(Vec::new());
    }

    let payload_type = payload
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();

    if outer_type == "turn_context" {
        state.apply_turn_context(&payload);
    }

    if is_user_turn_boundary(outer_type, payload_type, &payload) {
        state.start_next_turn(timestamp_ns);
        return Ok(vec![state.event(
            identity,
            path,
            line_number,
            timestamp_ns,
            state.user_event_id(line_number),
            OperationType::User,
            "user_message".to_string(),
            OperationStatus::Success,
            Usage::default(),
            metadata_without_content(&payload),
        )]);
    }

    match payload_type {
        "token_count" => {
            let source_event_id = state.next_llm_event_id(line_number);
            Ok(vec![
                state.event(
                    identity,
                    path,
                    line_number,
                    timestamp_ns,
                    source_event_id,
                    OperationType::LlmCall,
                    state
                        .model
                        .clone()
                        .unwrap_or_else(|| "model_call".to_string()),
                    OperationStatus::Success,
                    state.usage_from_token_count(&payload),
                    metadata_without_content(&payload),
                ),
            ])
        }
        "function_call" | "custom_tool_call" => {
            let call_id = string_field(&payload, "call_id")
                .or_else(|| string_field(&payload, "id"))
                .unwrap_or_else(|| format!("line-{line_number}"));
            let tool_name =
                string_field(&payload, "name").unwrap_or_else(|| payload_type.to_string());
            let skill_events = skills::codex::skill_events_from_function_call(&payload);
            state.tool_names.insert(call_id.clone(), tool_name.clone());
            let mut event = state.event(
                identity,
                path,
                line_number,
                timestamp_ns,
                call_id,
                OperationType::ToolCall,
                tool_name,
                OperationStatus::Running,
                Usage::default(),
                metadata_without_content(&payload),
            );
            event.skill_events = skill_events;
            Ok(vec![event])
        }
        "function_call_output" | "custom_tool_call_output" => {
            let call_id = string_field(&payload, "call_id")
                .or_else(|| string_field(&payload, "id"))
                .unwrap_or_else(|| format!("line-{line_number}"));
            let tool_name = state
                .tool_names
                .get(&call_id)
                .cloned()
                .unwrap_or_else(|| payload_type.to_string());
            Ok(vec![state.event(
                identity,
                path,
                line_number,
                timestamp_ns,
                call_id,
                OperationType::ToolCall,
                tool_name,
                tool_status(&payload),
                Usage::default(),
                metadata_without_content(&payload),
            )])
        }
        "turn_aborted" => Ok(vec![state.event(
            identity,
            path,
            line_number,
            timestamp_ns,
            format!("{}:aborted:{line_number}", state.run_external_id()),
            OperationType::System,
            "turn_aborted".to_string(),
            OperationStatus::Failed,
            Usage::default(),
            metadata_without_content(&payload),
        )]),
        _ => Ok(Vec::new()),
    }
}

#[derive(Deserialize, Serialize)]
struct CodexImportCheckpoint {
    start_offset: u64,
    line_number: usize,
    event_count: usize,
    warning_count: usize,
    first_event_ns: Option<i64>,
    last_event_ns: Option<i64>,
    state: Option<SessionState>,
}

impl CodexImportCheckpoint {
    fn from_prepared(prepared: &crate::db::PreparedImportFile, size_bytes: u64) -> Self {
        let stored = prepared
            .metadata_json
            .as_deref()
            .and_then(|value| serde_json::from_str::<Self>(value).ok());
        let stored = match stored {
            Some(stored)
                if prepared.can_resume
                    && prepared.last_offset == Some(stored.start_offset)
                    && prepared
                        .previous_size_bytes
                        .is_none_or(|previous_size| previous_size == stored.start_offset)
                    && stored.start_offset <= size_bytes =>
            {
                Some(stored)
            }
            _ => None,
        };

        stored.unwrap_or(Self {
            start_offset: 0,
            line_number: 0,
            event_count: 0,
            warning_count: 0,
            first_event_ns: None,
            last_event_ns: None,
            state: None,
        })
    }
}

#[derive(Deserialize, Serialize)]
struct SessionState {
    session_external_id: String,
    cwd: Option<String>,
    model: Option<String>,
    model_provider: Option<String>,
    title: Option<String>,
    turn_index: i64,
    #[serde(default)]
    turn_context_id: Option<String>,
    #[serde(default)]
    token_count_index: usize,
    tool_names: HashMap<String, String>,
}

impl SessionState {
    fn from_path(path: &Path) -> Self {
        let fallback = path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("session");
        Self {
            session_external_id: fallback.to_string(),
            cwd: None,
            model: None,
            model_provider: None,
            title: None,
            turn_index: 0,
            turn_context_id: None,
            token_count_index: 0,
            tool_names: HashMap::new(),
        }
    }

    fn apply_session_meta(&mut self, payload: &Map<String, Value>) {
        if let Some(id) = string_field(payload, "session_id")
            .or_else(|| string_field(payload, "parent_thread_id"))
            .or_else(|| string_field(payload, "forked_from_id"))
            .or_else(|| string_field(payload, "id"))
        {
            self.session_external_id = id;
        }
        self.cwd = string_field(payload, "cwd").or_else(|| self.cwd.clone());
        self.model = string_field(payload, "model").or_else(|| self.model.clone());
        self.model_provider =
            string_field(payload, "model_provider").or_else(|| self.model_provider.clone());
        self.title = self.cwd.as_ref().and_then(|cwd| {
            Path::new(cwd)
                .file_name()
                .and_then(|value| value.to_str())
                .map(ToOwned::to_owned)
        });
    }

    fn apply_turn_context(&mut self, payload: &Map<String, Value>) {
        self.turn_context_id = string_field(payload, "turn_id");
        self.token_count_index = 0;
        self.cwd = string_field(payload, "cwd").or_else(|| self.cwd.clone());
        self.model = string_field(payload, "model").or_else(|| self.model.clone());
        self.model_provider = string_field(payload, "model_provider")
            .or_else(|| self.model_provider.clone())
            .or_else(|| Some("codex".to_string()));
        self.title = self.cwd.as_ref().and_then(|cwd| {
            Path::new(cwd)
                .file_name()
                .and_then(|value| value.to_str())
                .map(ToOwned::to_owned)
        });
    }

    fn usage_from_token_count(&self, payload: &Map<String, Value>) -> Usage {
        let mut usage = usage_from_token_count(payload);
        usage.model = self.model.clone();
        usage.provider = self
            .model_provider
            .clone()
            .or_else(|| Some("codex".to_string()));
        usage.pricing_status = Some("estimated_from_model_prices".to_string());
        usage.cost_confidence = Some("estimated".to_string());
        usage
    }

    fn start_next_turn(&mut self, timestamp_ns: i64) {
        if self.turn_index == 0 {
            self.turn_index = 1;
        } else {
            self.turn_index += 1;
        }
        self.tool_names.clear();
        if self.title.is_none() {
            self.title = Some(format!("Codex turn {}", self.turn_index));
        }
        let _ = timestamp_ns;
    }

    fn run_external_id(&self) -> String {
        if let Some(turn_context_id) = self.turn_context_id.as_deref() {
            return format!("{}:turn:{turn_context_id}", self.session_external_id);
        }
        format!(
            "{}:turn:{}",
            self.session_external_id,
            self.turn_index.max(1)
        )
    }

    fn user_event_id(&self, line_number: usize) -> String {
        self.turn_context_id
            .as_deref()
            .map(|turn_context_id| {
                format!(
                    "{}:turn_context:{turn_context_id}:user",
                    self.session_external_id
                )
            })
            .unwrap_or_else(|| format!("{}:user:{line_number}", self.run_external_id()))
    }

    fn next_llm_event_id(&mut self, line_number: usize) -> String {
        let token_count_index = self.token_count_index;
        self.token_count_index = self.token_count_index.saturating_add(1);
        self.turn_context_id
            .as_deref()
            .map(|turn_context_id| {
                format!(
                    "{}:turn_context:{turn_context_id}:token_count:{token_count_index}",
                    self.session_external_id
                )
            })
            .unwrap_or_else(|| {
                format!(
                    "{}:llm:{line_number}:token_count:{token_count_index}",
                    self.run_external_id()
                )
            })
    }

    fn event(
        &self,
        identity: &ImportIdentity,
        path: &Path,
        line_number: usize,
        timestamp_ns: i64,
        source_event_id: String,
        operation_type: OperationType,
        name: String,
        status: OperationStatus,
        usage: Usage,
        metadata: Option<Value>,
    ) -> NormalizedEvent {
        let run_external_id = self.run_external_id();
        NormalizedEvent {
            profile_id: identity.profile_id.clone(),
            device_id: identity.device_id.clone(),
            source: "codex".to_string(),
            source_kind: "session_jsonl".to_string(),
            source_event_id: Some(source_event_id),
            observed_at_ns: now_ns(),
            occurred_at_ns: timestamp_ns,
            source_ref: Some(format!("{}:{line_number}", path.display())),
            cwd: self.cwd.clone(),
            project_id: self.cwd.as_ref().map(|cwd| stable_hash(cwd)),
            trace: TraceContext::default(),
            session: Some(EntityHint {
                external_id: self.session_external_id.clone(),
                kind: "codex_thread".to_string(),
                title: self.title.clone(),
            }),
            run: EntityHint {
                external_id: run_external_id.clone(),
                kind: "codex_turn".to_string(),
                title: self.title.clone(),
            },
            turn: Some(TurnHint {
                external_id: run_external_id,
                index: Some(self.turn_index.max(1) - 1),
                role: Some("user".to_string()),
            }),
            operation: Operation {
                operation_type,
                name,
                status,
                error_type: status
                    .eq(&OperationStatus::Failed)
                    .then(|| "codex_event_failed".to_string()),
                started_at_ns: timestamp_ns,
                ended_at_ns: status.ne(&OperationStatus::Running).then_some(timestamp_ns),
                duration_ns: None,
                order_index: Some(line_number.min(i64::MAX as usize) as i64),
                metadata,
            },
            usage,
            skill_events: Vec::new(),
            confidence: 0.9,
        }
    }
}

fn is_user_turn_boundary(
    outer_type: &str,
    payload_type: &str,
    payload: &Map<String, Value>,
) -> bool {
    if outer_type == "turn_context" {
        return true;
    }

    payload_type == "user_message"
        || (payload_type == "message"
            && string_field(payload, "role").as_deref() == Some("user")
            && !is_environment_context(payload))
}

fn is_environment_context(payload: &Map<String, Value>) -> bool {
    payload
        .get("content")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(|item| item.get("text"))
        .and_then(Value::as_str)
        .is_some_and(|text| text.starts_with("<environment_context>"))
}

fn usage_from_token_count(payload: &Map<String, Value>) -> Usage {
    let info = payload.get("info").and_then(Value::as_object);
    let last_usage = info
        .and_then(|info| info.get("last_token_usage"))
        .and_then(Value::as_object);
    let total_usage = info
        .and_then(|info| info.get("total_token_usage"))
        .and_then(Value::as_object);
    let usage = last_usage.or(total_usage);
    let input_tokens = nested_i64(usage, "input_tokens").unwrap_or_default();
    let cache_read_tokens = nested_i64(usage, "cached_input_tokens").unwrap_or_default();
    let model_context_window = info
        .and_then(|info| info.get("model_context_window"))
        .and_then(Value::as_i64);

    Usage {
        provider: Some("codex".to_string()),
        input_tokens,
        output_tokens: nested_i64(usage, "output_tokens").unwrap_or_default(),
        reasoning_tokens: nested_i64(usage, "reasoning_output_tokens").unwrap_or_default(),
        uncached_input_tokens: input_tokens.saturating_sub(cache_read_tokens),
        cache_read_tokens,
        model_context_window,
        context_window_percent: model_context_window
            .filter(|window| *window > 0)
            .map(|window| input_tokens as f64 / window as f64),
        pricing_status: Some("unavailable".to_string()),
        cost_confidence: Some("unknown".to_string()),
        ..Usage::default()
    }
}

fn tool_status(payload: &Map<String, Value>) -> OperationStatus {
    if let Some(status) = string_field(payload, "status") {
        return match status.as_str() {
            "failed" | "error" => OperationStatus::Failed,
            "success" | "completed" => OperationStatus::Success,
            _ => OperationStatus::Success,
        };
    }

    let output = payload
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if let Some(exit_code) = output.strip_prefix("Exit code: ").and_then(|rest| {
        rest.split_whitespace()
            .next()
            .and_then(|value| value.parse::<i64>().ok())
    }) && exit_code != 0
    {
        return OperationStatus::Failed;
    }

    OperationStatus::Success
}

fn metadata_without_content(payload: &Map<String, Value>) -> Option<Value> {
    let mut metadata = Map::new();

    for key in [
        "type", "role", "name", "call_id", "id", "model", "effort", "status", "reason",
    ] {
        if let Some(value) = payload.get(key) {
            metadata.insert(key.to_string(), value.clone());
        }
    }

    if let Some(arguments) = payload.get("arguments").and_then(Value::as_str) {
        metadata.insert("arguments_hash".to_string(), json!(stable_hash(arguments)));
        metadata.insert("arguments_bytes".to_string(), json!(arguments.len()));
    }

    if let Some(output) = payload.get("output").and_then(Value::as_str) {
        metadata.insert("output_hash".to_string(), json!(stable_hash(output)));
        metadata.insert("output_bytes".to_string(), json!(output.len()));
    }

    if let Some(info) = payload.get("info") {
        metadata.insert("info".to_string(), info.clone());
    }

    if metadata.is_empty() {
        None
    } else {
        Some(Value::Object(metadata))
    }
}

fn string_field(payload: &Map<String, Value>, key: &str) -> Option<String> {
    payload.get(key).and_then(|value| match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        Value::Bool(value) => Some(value.to_string()),
        _ => None,
    })
}

fn nested_i64(payload: Option<&Map<String, Value>>, key: &str) -> Option<i64> {
    payload?.get(key).and_then(|value| match value {
        Value::Number(value) => value.as_i64(),
        Value::String(value) => value.parse().ok(),
        _ => None,
    })
}

fn parse_rfc3339_ns(value: &str) -> Option<i64> {
    let date_time = value.strip_suffix('Z')?;
    let (date, time) = date_time.split_once('T')?;
    let mut date_parts = date.split('-');
    let year = date_parts.next()?.parse::<i64>().ok()?;
    let month = date_parts.next()?.parse::<i64>().ok()?;
    let day = date_parts.next()?.parse::<i64>().ok()?;
    let (clock, fraction) = time.split_once('.').unwrap_or((time, ""));
    let mut time_parts = clock.split(':');
    let hour = time_parts.next()?.parse::<i64>().ok()?;
    let minute = time_parts.next()?.parse::<i64>().ok()?;
    let second = time_parts.next()?.parse::<i64>().ok()?;
    let nanos = fraction
        .chars()
        .take(9)
        .collect::<String>()
        .parse::<i64>()
        .unwrap_or_default()
        * 10_i64.pow(9_u32.saturating_sub(fraction.len().min(9) as u32));

    let days = days_from_civil(year, month, day)?;
    Some(
        days.saturating_mul(86_400_000_000_000)
            .saturating_add(hour.saturating_mul(3_600_000_000_000))
            .saturating_add(minute.saturating_mul(60_000_000_000))
            .saturating_add(second.saturating_mul(1_000_000_000))
            .saturating_add(nanos),
    )
}

fn days_from_civil(year: i64, month: i64, day: i64) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(era * 146097 + day_of_era - 719468)
}

fn is_jsonl(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "jsonl")
}

fn catalog_size(path: &Path) -> Result<u64> {
    let mut total = file_len(path)?;

    if let Some(raw) = path.to_str() {
        total = total.saturating_add(file_len(Path::new(&format!("{raw}-wal")))?);
        total = total.saturating_add(file_len(Path::new(&format!("{raw}-shm")))?);
    }

    Ok(total)
}

fn file_len(path: &Path) -> Result<u64> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error).with_context(|| format!("stat {}", path.display())),
    }
}

#[derive(Default)]
struct ScanSize {
    files_seen: usize,
    files_imported: usize,
    files_skipped: usize,
    bytes: u64,
    events: usize,
    warnings: usize,
}

struct ImportedFile {
    events: usize,
    warnings: usize,
}

impl ImportedFile {
    fn skipped() -> Self {
        Self {
            events: 0,
            warnings: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::Write,
        path::{Path, PathBuf},
    };

    use anyhow::Result;

    use crate::db::Database;

    use super::import;

    #[test]
    fn imports_codex_session_metadata_into_operation_tables() -> Result<()> {
        let root = temp_dir("codex-import");
        fs::create_dir_all(&root)?;
        let session_path = root.join("rollout-test.jsonl");
        fs::write(
            &session_path,
            r#"{"timestamp":"2026-01-01T00:00:00.000Z","type":"session_meta","payload":{"id":"session-1","cwd":"/tmp/project","model":"gpt-test","model_provider":"openai"}}"#
                .to_string()
                + "\n"
                + r#"{"timestamp":"2026-01-01T00:00:01.000Z","type":"response_item","payload":{"type":"user_message","message":"redacted","images":[]}}"#
                + "\n"
                + r#"{"timestamp":"2026-01-01T00:00:02.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":20,"reasoning_output_tokens":5,"total_tokens":120},"model_context_window":1000}}}"#
                + "\n"
                + r#"{"timestamp":"2026-01-01T00:00:03.000Z","type":"response_item","payload":{"type":"function_call","name":"shell_command","arguments":"{\"command\":\"true\"}","call_id":"call-1"}}"#
                + "\n"
                + r#"{"timestamp":"2026-01-01T00:00:04.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-1","output":"Exit code: 0\nWall time: 0.1 seconds\nOutput:\n"}}"#
                + "\n",
        )?;

        let db_path = root.join("catalog.sqlite");
        let db = Database::open(&db_path)?;
        db.migrate()?;

        let report = import(&db, Some(session_path))?;

        assert_eq!(report.files_seen, 1);
        assert_eq!(count(&db, "import_files")?, 1);
        assert_eq!(count(&db, "sessions")?, 1);
        assert_eq!(count(&db, "runs")?, 1);
        assert_eq!(count(&db, "turns")?, 1);
        assert_eq!(count(&db, "run_steps")?, 3);
        assert_eq!(count(&db, "llm_calls")?, 1);
        assert_eq!(count(&db, "tool_calls")?, 1);

        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn imports_only_appended_codex_session_lines() -> Result<()> {
        let root = temp_dir("codex-incremental-import");
        fs::create_dir_all(&root)?;
        let session_path = root.join("rollout-test.jsonl");
        fs::write(
            &session_path,
            r#"{"timestamp":"2026-01-01T00:00:00.000Z","type":"session_meta","payload":{"id":"session-1","cwd":"/tmp/project","model":"gpt-test","model_provider":"openai"}}"#
                .to_string()
                + "\n"
                + r#"{"timestamp":"2026-01-01T00:00:01.000Z","type":"response_item","payload":{"type":"user_message","message":"redacted","images":[]}}"#
                + "\n"
                + r#"{"timestamp":"2026-01-01T00:00:02.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":20,"reasoning_output_tokens":5,"total_tokens":120},"model_context_window":1000}}}"#
                + "\n",
        )?;

        let db_path = root.join("catalog.sqlite");
        let db = Database::open(&db_path)?;
        db.migrate()?;

        let first = import(&db, Some(session_path.clone()))?;
        let first_offset = get_import_file_i64(&db, "last_offset")?;
        assert_eq!(first.files_imported, 1);
        assert_eq!(first.events_projected, 2);
        assert_eq!(count(&db, "llm_calls")?, 1);
        assert_eq!(count(&db, "tool_calls")?, 0);

        append_lines(
            &session_path,
            &[
                r#"{"timestamp":"2026-01-01T00:00:03.000Z","type":"response_item","payload":{"type":"function_call","name":"shell_command","arguments":"{\"command\":\"true\"}","call_id":"call-1"}}"#,
                r#"{"timestamp":"2026-01-01T00:00:04.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-1","output":"Exit code: 0\nWall time: 0.1 seconds\nOutput:\n"}}"#,
            ],
        )?;

        let second = import(&db, Some(session_path.clone()))?;
        let second_offset = get_import_file_i64(&db, "last_offset")?;
        assert_eq!(second.files_imported, 1);
        assert_eq!(second.events_projected, 2);
        assert!(second_offset > first_offset);
        assert_eq!(get_import_file_i64(&db, "event_count")?, 4);
        assert_eq!(count(&db, "sessions")?, 1);
        assert_eq!(count(&db, "runs")?, 1);
        assert_eq!(count(&db, "llm_calls")?, 1);
        assert_eq!(count(&db, "tool_calls")?, 1);

        let third = import(&db, Some(session_path))?;
        assert_eq!(third.files_imported, 0);
        assert_eq!(third.events_projected, 0);

        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn truncated_codex_session_ignores_stale_checkpoint() -> Result<()> {
        let root = temp_dir("codex-truncated-import");
        fs::create_dir_all(&root)?;
        let session_path = root.join("rollout-test.jsonl");
        fs::write(
            &session_path,
            r#"{"timestamp":"2026-01-01T00:00:00.000Z","type":"session_meta","payload":{"id":"session-1","cwd":"/tmp/project","model":"gpt-test","model_provider":"openai"}}"#
                .to_string()
                + "\n"
                + r#"{"timestamp":"2026-01-01T00:00:01.000Z","type":"response_item","payload":{"type":"user_message","message":"redacted","images":[]}}"#
                + "\n"
                + r#"{"timestamp":"2026-01-01T00:00:02.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":20,"reasoning_output_tokens":5,"total_tokens":120},"model_context_window":1000}}}"#
                + "\n",
        )?;

        let db_path = root.join("catalog.sqlite");
        let db = Database::open(&db_path)?;
        db.migrate()?;
        let first = import(&db, Some(session_path.clone()))?;
        assert_eq!(first.events_projected, 2);

        fs::write(
            &session_path,
            r#"{"timestamp":"2026-01-02T00:00:00.000Z","type":"session_meta","payload":{"id":"session-1","cwd":"/tmp/project","model":"gpt-test","model_provider":"openai"}}"#
                .to_string()
                + "\n"
                + r#"{"timestamp":"2026-01-02T00:00:01.000Z","type":"response_item","payload":{"type":"user_message","message":"redacted","images":[]}}"#
                + "\n",
        )?;

        let second = import(&db, Some(session_path.clone()))?;
        assert_eq!(second.files_imported, 1);
        assert_eq!(second.events_projected, 1);
        assert_eq!(
            get_import_file_i64(&db, "last_offset")?,
            fs::metadata(session_path)?.len() as i64
        );

        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn deduplicates_token_counts_copied_into_continued_rollouts() -> Result<()> {
        let root = temp_dir("codex-import-continued-rollout");
        fs::create_dir_all(&root)?;
        let original_path = root.join("00-original.jsonl");
        let continued_path = root.join("01-continued.jsonl");
        let shared_turn = "turn-shared";
        let root_session = "session-root";

        fs::write(
            &original_path,
            [
                r#"{"timestamp":"2026-01-01T00:00:00.000Z","type":"session_meta","payload":{"id":"session-root","session_id":"session-root","cwd":"/tmp/project","model":"gpt-test","model_provider":"openai"}}"#,
                r#"{"timestamp":"2026-01-01T00:00:01.000Z","type":"turn_context","payload":{"turn_id":"turn-shared","cwd":"/tmp/project","model":"gpt-test","model_provider":"openai"}}"#,
                r#"{"timestamp":"2026-01-01T00:00:02.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":20,"reasoning_output_tokens":5,"total_tokens":120},"model_context_window":1000}}}"#,
            ]
            .join("\n")
                + "\n",
        )?;
        fs::write(
            &continued_path,
            [
                r#"{"timestamp":"2026-01-02T00:00:00.000Z","type":"session_meta","payload":{"id":"session-child","session_id":"session-root","parent_thread_id":"session-root","forked_from_id":"session-root","cwd":"/tmp/project","model":"gpt-test","model_provider":"openai"}}"#,
                r#"{"timestamp":"2026-01-02T00:00:00.001Z","type":"session_meta","payload":{"id":"session-root","session_id":"session-root","cwd":"/tmp/project","model":"gpt-test","model_provider":"openai"}}"#,
                r#"{"timestamp":"2026-01-02T00:00:01.000Z","type":"turn_context","payload":{"turn_id":"turn-shared","cwd":"/tmp/project","model":"gpt-test","model_provider":"openai"}}"#,
                r#"{"timestamp":"2026-01-02T00:00:02.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":20,"reasoning_output_tokens":5,"total_tokens":120},"model_context_window":1000}}}"#,
                r#"{"timestamp":"2026-01-02T00:00:03.000Z","type":"turn_context","payload":{"turn_id":"turn-new","cwd":"/tmp/project","model":"gpt-test","model_provider":"openai"}}"#,
                r#"{"timestamp":"2026-01-02T00:00:04.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":50,"cached_input_tokens":20,"output_tokens":10,"reasoning_output_tokens":2,"total_tokens":60},"model_context_window":1000}}}"#,
            ]
            .join("\n")
                + "\n",
        )?;

        let db_path = root.join("catalog.sqlite");
        let db = Database::open(&db_path)?;
        db.migrate()?;
        db.connection().execute_batch(
            "INSERT INTO runs (
                run_id, profile_id, device_id, source, kind, status, started_at_ns
             ) VALUES
                ('legacy-codex-run', 'local', 'local_device', 'codex', 'codex_turn', 'success', 1),
                ('pi-run', 'local', 'local_device', 'pi', 'pi_turn', 'success', 1);
             INSERT INTO llm_calls (
                llm_call_id, profile_id, device_id, run_id, source, status,
                input_tokens, output_tokens, started_at_ns
             ) VALUES
                ('legacy-codex-call', 'local', 'local_device', 'legacy-codex-run', 'codex', 'success', 999, 0, 1),
                ('pi-call', 'local', 'local_device', 'pi-run', 'pi', 'success', 10, 2, 1);",
        )?;

        let report = import(&db, Some(root.clone()))?;
        assert_eq!(report.files_imported, 2);
        let first_pass: (i64, i64, i64, String) = db.connection().query_row(
            "SELECT COUNT(*), SUM(input_tokens), SUM(output_tokens), MIN(started_day)
             FROM llm_calls WHERE source = 'codex'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        assert_eq!(first_pass, (2, 150, 30, "2026-01-01".to_string()));
        let pi_totals: (i64, i64) = db.connection().query_row(
            "SELECT SUM(input_tokens), SUM(output_tokens) FROM llm_calls WHERE source = 'pi'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(pi_totals, (10, 2));
        let session_id: String = db.connection().query_row(
            "SELECT external_id FROM sessions WHERE source = 'codex'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(session_id, root_session);

        append_lines(
            &continued_path,
            &[
                r#"{"timestamp":"2026-01-02T00:00:05.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":25,"cached_input_tokens":10,"output_tokens":5,"reasoning_output_tokens":1,"total_tokens":30},"model_context_window":1000}}}"#,
            ],
        )?;
        import(&db, Some(root.clone()))?;
        let second_pass: (i64, i64, i64) = db.connection().query_row(
            "SELECT COUNT(*), SUM(input_tokens), SUM(output_tokens)
             FROM llm_calls WHERE source = 'codex'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(second_pass, (3, 175, 35));

        let event_id: String = db.connection().query_row(
            "SELECT source_event_id FROM run_steps
             WHERE source = 'codex' AND source_event_id LIKE '%token_count:0'
             ORDER BY started_at_ns LIMIT 1",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(
            event_id,
            format!("{root_session}:turn_context:{shared_turn}:token_count:0")
        );

        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    fn count(db: &Database, table: &str) -> Result<i64> {
        let sql = format!("SELECT COUNT(*) FROM {table}");
        Ok(db.connection().query_row(&sql, [], |row| row.get(0))?)
    }

    fn get_import_file_i64(db: &Database, column: &str) -> Result<i64> {
        let sql = format!("SELECT {column} FROM import_files LIMIT 1");
        Ok(db.connection().query_row(&sql, [], |row| row.get(0))?)
    }

    fn append_lines(path: &Path, lines: &[&str]) -> Result<()> {
        let mut file = fs::OpenOptions::new().append(true).open(path)?;
        for line in lines {
            writeln!(file, "{line}")?;
        }
        Ok(())
    }

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("shirabe-{name}-{}", std::process::id()))
    }
}
