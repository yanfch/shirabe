use std::{
    collections::{HashMap, HashSet},
    env, fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
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
    let root_path = path.unwrap_or_else(default_pi_sessions_dir);
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
        source: "pi".to_string(),
        source_id: format!("{}:pi:collector", identity.profile_id),
        root_path,
        files_seen: scan.files_seen,
        files_imported: scan.files_seen,
        files_skipped: 0,
        events_projected: scan.events,
        source_bytes_scanned: scan.bytes,
        shirabe_bytes_written: 0,
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
    let root_path = path.unwrap_or_else(default_pi_sessions_dir);
    let source_kind = if root_path.is_file() {
        "session_jsonl_file"
    } else {
        "session_jsonl_directory"
    };
    let source_id = db.record_import_source_for_profile(
        "pi",
        source_kind,
        &root_path,
        &identity.profile_id,
        &identity.device_id,
    )?;

    let scan_started = Instant::now();
    let tx = db.begin_batch()?;
    let scan = scan_sessions(db, &source_id, &root_path, modified_since_ns, identity)?;
    tx.commit()?;
    let scan_elapsed_ms = scan_started.elapsed().as_millis();

    let catalog_started = Instant::now();
    let shirabe_bytes_written = catalog_size(db.path())?;
    let catalog_elapsed_ms = catalog_started.elapsed().as_millis();

    let mut warnings = vec![
        "pi importer uses metadata-first parsing and does not copy prompt, response, tool output, or image content".to_string(),
    ];
    if scan.warnings > 0 {
        warnings.push(format!(
            "{} session lines could not be parsed and were skipped",
            scan.warnings
        ));
    }

    Ok(ImportReport {
        source: "pi".to_string(),
        source_id,
        root_path,
        files_seen: scan.files_seen,
        files_imported: scan.files_imported,
        files_skipped: scan.files_skipped,
        events_projected: scan.events,
        source_bytes_scanned: scan.bytes,
        shirabe_bytes_written,
        timings: vec![
            ImportTiming {
                stage: "scan_project",
                elapsed_ms: scan_elapsed_ms,
            },
            ImportTiming {
                stage: "parse_json",
                elapsed_ms: scan.parse_elapsed_ms,
            },
            ImportTiming {
                stage: "project_events",
                elapsed_ms: scan.project_elapsed_ms,
            },
            ImportTiming {
                stage: "refresh_summaries",
                elapsed_ms: scan.summary_elapsed_ms,
            },
            ImportTiming {
                stage: "finish_files",
                elapsed_ms: scan.finish_elapsed_ms,
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

fn default_pi_sessions_dir() -> PathBuf {
    env::var_os("PI_SESSIONS_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("PI_DIR").map(|root| PathBuf::from(root).join("agent").join("sessions"))
        })
        .or_else(|| {
            crate::config::home_dir().map(|home| home.join(".pi").join("agent").join("sessions"))
        })
        .unwrap_or_else(|| PathBuf::from(".pi").join("agent").join("sessions"))
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
            parse_elapsed_ms: 0,
            project_elapsed_ms: 0,
            summary_elapsed_ms: 0,
            finish_elapsed_ms: 0,
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

    for entry in WalkDir::new(root).follow_links(false) {
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
        parse_elapsed_ms: 0,
        project_elapsed_ms: 0,
        summary_elapsed_ms: 0,
        finish_elapsed_ms: 0,
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

    Ok(ImportedFile {
        events,
        parse_elapsed_ms: 0,
        project_elapsed_ms: 0,
        summary_elapsed_ms: 0,
        finish_elapsed_ms: 0,
        warnings,
    })
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
            import_session_file(db, &prepared.file_id, root, identity)?
        } else {
            ImportedFile::skipped()
        };
        return Ok(ScanSize {
            files_seen: 1,
            files_imported: usize::from(prepared.should_import),
            files_skipped: usize::from(!prepared.should_import),
            bytes: metadata.len(),
            events: imported.events,
            parse_elapsed_ms: imported.parse_elapsed_ms,
            project_elapsed_ms: imported.project_elapsed_ms,
            summary_elapsed_ms: imported.summary_elapsed_ms,
            finish_elapsed_ms: imported.finish_elapsed_ms,
            warnings: imported.warnings,
        });
    }

    let mut files_seen = 0usize;
    let mut files_imported = 0usize;
    let mut files_skipped = 0usize;
    let mut bytes = 0u64;
    let mut events = 0usize;
    let mut parse_elapsed_ms = 0u128;
    let mut project_elapsed_ms = 0u128;
    let mut summary_elapsed_ms = 0u128;
    let mut finish_elapsed_ms = 0u128;
    let mut warnings = 0usize;

    if !root.exists() {
        return Ok(ScanSize {
            files_seen,
            files_imported,
            files_skipped,
            bytes,
            events,
            parse_elapsed_ms,
            project_elapsed_ms,
            summary_elapsed_ms,
            finish_elapsed_ms,
            warnings,
        });
    }

    for entry in WalkDir::new(root).follow_links(false) {
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
            let imported = import_session_file(db, &prepared.file_id, entry.path(), identity)?;
            events += imported.events;
            parse_elapsed_ms += imported.parse_elapsed_ms;
            project_elapsed_ms += imported.project_elapsed_ms;
            summary_elapsed_ms += imported.summary_elapsed_ms;
            finish_elapsed_ms += imported.finish_elapsed_ms;
            warnings += imported.warnings;
        }
    }

    Ok(ScanSize {
        files_seen,
        files_imported,
        files_skipped,
        bytes,
        events,
        parse_elapsed_ms,
        project_elapsed_ms,
        summary_elapsed_ms,
        finish_elapsed_ms,
        warnings,
    })
}

fn import_session_file(
    db: &Database,
    file_id: &str,
    path: &Path,
    identity: &ImportIdentity,
) -> Result<ImportedFile> {
    let file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let projector = Projector::new(db);
    let mut state = SessionState::from_path(path);
    let mut line = String::new();
    let mut line_number = 0usize;
    let mut offset = 0u64;
    let mut events = 0usize;
    let mut parse_elapsed = Duration::ZERO;
    let mut project_elapsed = Duration::ZERO;
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

        let parse_started = Instant::now();
        let parsed = parse_session_line(&mut state, &line, path, line_number, identity);
        parse_elapsed += parse_started.elapsed();

        match parsed {
            Ok(parsed) => {
                for event in parsed {
                    let occurred_at_ns = event.occurred_at_ns;
                    let project_started = Instant::now();
                    let projection = projector.project_with_cache(&event, &mut projection_cache)?;
                    project_elapsed += project_started.elapsed();
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

    let summary_started = Instant::now();
    projector.refresh_run_summaries(touched_run_ids.iter().map(String::as_str))?;
    db.refresh_observed_llm_latency_for_runs(touched_run_ids.iter().map(String::as_str))?;
    projector.refresh_session_summaries(touched_session_ids.iter().map(String::as_str))?;
    let summary_elapsed_ms = summary_started.elapsed().as_millis();

    let status = if warnings > 0 { "partial" } else { "imported" };
    let finish_started = Instant::now();
    db.finish_import_file(
        file_id,
        status,
        events,
        warnings,
        Some(offset),
        first_event_ns,
        last_event_ns,
        None,
    )?;
    let finish_elapsed_ms = finish_started.elapsed().as_millis();

    Ok(ImportedFile {
        events,
        parse_elapsed_ms: parse_elapsed.as_millis(),
        project_elapsed_ms: project_elapsed.as_millis(),
        summary_elapsed_ms,
        finish_elapsed_ms,
        warnings,
    })
}

fn parse_session_line(
    state: &mut SessionState,
    line: &str,
    path: &Path,
    line_number: usize,
    identity: &ImportIdentity,
) -> Result<Vec<NormalizedEvent>> {
    let entry: Map<String, Value> = serde_json::from_str(line).context("parse pi session json")?;
    let timestamp_ns = entry_timestamp_ns(&entry).unwrap_or_else(now_ns);
    let entry_type = string_field(&entry, "type").unwrap_or_default();

    match entry_type.as_str() {
        "session" => {
            state.apply_session(&entry);
            Ok(Vec::new())
        }
        "session_info" => {
            state.apply_session_info(&entry);
            Ok(Vec::new())
        }
        "model_change" => {
            state.apply_model_change(&entry);
            Ok(Vec::new())
        }
        "thinking_level_change" => {
            state.apply_thinking_level(&entry);
            Ok(Vec::new())
        }
        "message" => parse_message_entry(state, &entry, path, line_number, timestamp_ns, identity),
        "compaction" => Ok(state
            .has_turn()
            .then(|| {
                state.event(
                    identity,
                    path,
                    line_number,
                    0,
                    timestamp_ns,
                    state.entry_id(&entry, line_number),
                    OperationType::System,
                    "compaction".to_string(),
                    OperationStatus::Success,
                    Usage::default(),
                    compaction_metadata(&entry),
                )
            })
            .into_iter()
            .collect()),
        _ => Ok(Vec::new()),
    }
}

fn parse_message_entry(
    state: &mut SessionState,
    entry: &Map<String, Value>,
    path: &Path,
    line_number: usize,
    timestamp_ns: i64,
    identity: &ImportIdentity,
) -> Result<Vec<NormalizedEvent>> {
    let Some(message) = entry.get("message").and_then(Value::as_object) else {
        return Ok(Vec::new());
    };
    let role = string_field(message, "role").unwrap_or_default();
    let entry_id = state.entry_id(entry, line_number);

    match role.as_str() {
        "user" => {
            state.start_next_turn();
            let mut event = state.event(
                identity,
                path,
                line_number,
                0,
                timestamp_ns,
                entry_id,
                OperationType::User,
                "user_message".to_string(),
                OperationStatus::Success,
                Usage::default(),
                message_metadata(entry, message),
            );
            event.skill_events = skills::pi::skill_events_from_user_message(message);
            Ok(vec![event])
        }
        "assistant" => {
            state.ensure_turn();
            let operation_name = state
                .message_model(message)
                .unwrap_or_else(|| "assistant_message".to_string());
            let status = assistant_status(message);
            let usage = state.usage_from_message(message);
            let metadata = message_metadata(entry, message);
            let mut events = vec![state.event(
                identity,
                path,
                line_number,
                0,
                timestamp_ns,
                entry_id.clone(),
                OperationType::LlmCall,
                operation_name,
                status,
                usage,
                metadata,
            )];

            for (index, tool_call) in tool_calls_from_message(message).into_iter().enumerate() {
                let call_id = tool_call
                    .id
                    .unwrap_or_else(|| format!("{entry_id}:tool:{index}"));
                state
                    .tool_names
                    .insert(call_id.clone(), tool_call.name.clone());
                let mut event = state.event(
                    identity,
                    path,
                    line_number,
                    index.saturating_add(1),
                    timestamp_ns,
                    state.scoped_id(&call_id),
                    OperationType::ToolCall,
                    tool_call.name,
                    OperationStatus::Running,
                    Usage::default(),
                    tool_call.metadata,
                );
                event.skill_events = tool_call.skill_events;
                events.push(event);
            }

            Ok(events)
        }
        "toolResult" => {
            state.ensure_turn();
            let call_id = string_field(message, "toolCallId").unwrap_or(entry_id);
            let tool_name = string_field(message, "toolName")
                .or_else(|| state.tool_names.get(&call_id).cloned())
                .unwrap_or_else(|| "tool".to_string());
            Ok(vec![state.event(
                identity,
                path,
                line_number,
                0,
                timestamp_ns,
                state.scoped_id(&call_id),
                OperationType::ToolCall,
                tool_name,
                tool_result_status(message),
                Usage::default(),
                tool_result_metadata(entry, message),
            )])
        }
        "bashExecution" => {
            state.ensure_turn();
            Ok(vec![state.event(
                identity,
                path,
                line_number,
                0,
                timestamp_ns,
                entry_id,
                OperationType::ToolCall,
                "bash".to_string(),
                bash_execution_status(message),
                Usage::default(),
                bash_execution_metadata(entry, message),
            )])
        }
        _ => Ok(Vec::new()),
    }
}

struct SessionState {
    session_external_id: String,
    cwd: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    api: Option<String>,
    title: Option<String>,
    thinking_level: Option<String>,
    turn_index: i64,
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
            provider: None,
            api: None,
            title: None,
            thinking_level: None,
            turn_index: 0,
            tool_names: HashMap::new(),
        }
    }

    fn apply_session(&mut self, entry: &Map<String, Value>) {
        if let Some(id) = string_field(entry, "id") {
            self.session_external_id = id;
        }
        self.cwd = string_field(entry, "cwd").or_else(|| self.cwd.clone());
        self.refresh_title_from_cwd();
    }

    fn apply_session_info(&mut self, entry: &Map<String, Value>) {
        self.title = string_field(entry, "name").or_else(|| self.title.clone());
    }

    fn apply_model_change(&mut self, entry: &Map<String, Value>) {
        let provider = string_field(entry, "provider").or_else(|| self.provider.clone());
        let model = string_field(entry, "modelId").or_else(|| self.model.clone());
        let (provider, model) = normalize_provider_and_model(provider, model);
        self.provider = provider;
        self.model = model;
    }

    fn apply_thinking_level(&mut self, entry: &Map<String, Value>) {
        self.thinking_level =
            string_field(entry, "thinkingLevel").or_else(|| self.thinking_level.clone());
    }

    fn refresh_title_from_cwd(&mut self) {
        if self.title.is_some() {
            return;
        }
        self.title = self.cwd.as_ref().and_then(|cwd| {
            Path::new(cwd)
                .file_name()
                .and_then(|value| value.to_str())
                .map(ToOwned::to_owned)
        });
    }

    fn start_next_turn(&mut self) {
        self.turn_index += 1;
        self.tool_names.clear();
        if self.title.is_none() {
            self.title = Some(format!("Pi turn {}", self.turn_index));
        }
    }

    fn ensure_turn(&mut self) {
        if self.turn_index == 0 {
            self.turn_index = 1;
        }
    }

    fn has_turn(&self) -> bool {
        self.turn_index > 0
    }

    fn run_external_id(&self) -> String {
        format!(
            "{}:turn:{}",
            self.session_external_id,
            self.turn_index.max(1)
        )
    }

    fn scoped_id(&self, id: &str) -> String {
        format!("{}:{id}", self.session_external_id)
    }

    fn entry_id(&self, entry: &Map<String, Value>, line_number: usize) -> String {
        string_field(entry, "id")
            .map(|id| self.scoped_id(&id))
            .unwrap_or_else(|| self.scoped_id(&format!("line-{line_number}")))
    }

    fn message_model(&self, message: &Map<String, Value>) -> Option<String> {
        let provider = string_field(message, "provider").or_else(|| self.provider.clone());
        let model = string_field(message, "model").or_else(|| self.model.clone());
        let (_, model) = normalize_provider_and_model(provider, model);
        model
    }

    fn usage_from_message(&mut self, message: &Map<String, Value>) -> Usage {
        let provider = string_field(message, "provider").or_else(|| self.provider.clone());
        let model = string_field(message, "model").or_else(|| self.model.clone());
        let (provider, model) = normalize_provider_and_model(provider, model);
        self.provider = provider;
        self.model = model;
        self.api = string_field(message, "api").or_else(|| self.api.clone());

        let usage = message.get("usage").and_then(Value::as_object);
        let uncached_input_tokens = nested_i64(usage, "input").unwrap_or_default();
        let cache_read_tokens = nested_i64(usage, "cacheRead").unwrap_or_default();
        let cache_write_tokens = nested_i64(usage, "cacheWrite")
            .unwrap_or_default()
            .saturating_add(nested_i64(usage, "cacheWrite1h").unwrap_or_default());
        let total_cost_usd = usage
            .and_then(|usage| usage.get("cost"))
            .and_then(Value::as_object)
            .and_then(|cost| float_field(cost, "total"))
            .unwrap_or_default();

        Usage {
            provider: self.provider.clone().or_else(|| Some("pi".to_string())),
            model: self.model.clone(),
            input_tokens: uncached_input_tokens.saturating_add(cache_read_tokens),
            output_tokens: nested_i64(usage, "output").unwrap_or_default(),
            uncached_input_tokens,
            cache_read_tokens,
            cache_write_tokens,
            total_cost_usd,
            pricing_status: Some(if total_cost_usd > 0.0 {
                "exact_from_source".to_string()
            } else {
                "unavailable".to_string()
            }),
            cost_confidence: Some(if total_cost_usd > 0.0 {
                "exact".to_string()
            } else {
                "unknown".to_string()
            }),
            ..Usage::default()
        }
    }

    fn event(
        &self,
        identity: &ImportIdentity,
        path: &Path,
        line_number: usize,
        sub_index: usize,
        timestamp_ns: i64,
        source_event_id: String,
        operation_type: OperationType,
        name: String,
        status: OperationStatus,
        usage: Usage,
        metadata: Option<Value>,
    ) -> NormalizedEvent {
        let run_external_id = self.run_external_id();
        let order_index = line_number
            .saturating_mul(1000)
            .saturating_add(sub_index)
            .min(i64::MAX as usize) as i64;
        NormalizedEvent {
            profile_id: identity.profile_id.clone(),
            device_id: identity.device_id.clone(),
            source: "pi".to_string(),
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
                kind: "pi_session".to_string(),
                title: self.title.clone(),
            }),
            run: EntityHint {
                external_id: run_external_id.clone(),
                kind: "pi_turn".to_string(),
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
                    .then(|| "pi_event_failed".to_string()),
                started_at_ns: timestamp_ns,
                ended_at_ns: status.ne(&OperationStatus::Running).then_some(timestamp_ns),
                duration_ns: None,
                order_index: Some(order_index),
                metadata,
            },
            usage,
            skill_events: Vec::new(),
            confidence: 0.9,
        }
    }
}

struct ToolCallEvent {
    id: Option<String>,
    name: String,
    metadata: Option<Value>,
    skill_events: Vec<crate::projection::event::SkillEventHint>,
}

fn tool_calls_from_message(message: &Map<String, Value>) -> Vec<ToolCallEvent> {
    message
        .get("content")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let item = item.as_object()?;
                    if string_field(item, "type").as_deref() != Some("toolCall") {
                        return None;
                    }

                    let id = string_field(item, "id");
                    let name = string_field(item, "name").unwrap_or_else(|| "tool".to_string());
                    let skill_events = skills::pi::skill_events_from_tool_call(item, &name);
                    Some(ToolCallEvent {
                        id,
                        name: name.clone(),
                        metadata: tool_call_metadata(item, &name),
                        skill_events,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn assistant_status(message: &Map<String, Value>) -> OperationStatus {
    if message.contains_key("errorMessage") {
        return OperationStatus::Failed;
    }

    match string_field(message, "stopReason").as_deref() {
        Some("error" | "aborted" | "cancelled") => OperationStatus::Failed,
        _ => OperationStatus::Success,
    }
}

fn tool_result_status(message: &Map<String, Value>) -> OperationStatus {
    if bool_field(message, "isError").unwrap_or(false) {
        OperationStatus::Failed
    } else {
        OperationStatus::Success
    }
}

fn bash_execution_status(message: &Map<String, Value>) -> OperationStatus {
    if bool_field(message, "cancelled").unwrap_or(false) {
        return OperationStatus::Failed;
    }
    if int_field(message, "exitCode").is_some_and(|exit_code| exit_code != 0) {
        return OperationStatus::Failed;
    }
    OperationStatus::Success
}

fn message_metadata(_entry: &Map<String, Value>, _message: &Map<String, Value>) -> Option<Value> {
    None
}

fn tool_call_metadata(_item: &Map<String, Value>, _name: &str) -> Option<Value> {
    None
}

fn tool_result_metadata(
    _entry: &Map<String, Value>,
    message: &Map<String, Value>,
) -> Option<Value> {
    let mut metadata = Map::new();
    if let Some(content) = message.get("content") {
        metadata.insert("output_bytes".to_string(), json!(value_len(content)));
    }
    finish_metadata(metadata)
}

fn bash_execution_metadata(
    _entry: &Map<String, Value>,
    message: &Map<String, Value>,
) -> Option<Value> {
    let mut metadata = Map::new();
    if let Some(output) = message.get("output") {
        metadata.insert("output_bytes".to_string(), json!(value_len(output)));
    }
    finish_metadata(metadata)
}

fn compaction_metadata(_entry: &Map<String, Value>) -> Option<Value> {
    None
}

fn finish_metadata(metadata: Map<String, Value>) -> Option<Value> {
    if metadata.is_empty() {
        None
    } else {
        Some(Value::Object(metadata))
    }
}

fn entry_timestamp_ns(entry: &Map<String, Value>) -> Option<i64> {
    entry
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339_ns)
        .or_else(|| {
            entry
                .get("message")
                .and_then(Value::as_object)
                .and_then(|message| int_field(message, "timestamp"))
                .map(|millis| millis.saturating_mul(1_000_000))
        })
}

fn normalize_provider_and_model(
    provider: Option<String>,
    model: Option<String>,
) -> (Option<String>, Option<String>) {
    let provider = provider.and_then(|value| non_empty(&value));
    let model = model.and_then(|value| non_empty(&value));

    let Some(model) = model else {
        return (provider, None);
    };

    if let Some(provider_value) = provider.as_deref() {
        for separator in ['/', ':'] {
            if let Some(rest) = model.strip_prefix(provider_value)
                && let Some(rest) = rest.strip_prefix(separator)
            {
                return (provider, non_empty(rest));
            }
        }
        if let Some((_, derived_model)) = split_model_slug(&model) {
            return (provider, Some(derived_model));
        }
        return (provider, Some(model));
    }

    if let Some((_, derived_model)) = split_model_slug(&model) {
        return (None, Some(derived_model));
    }

    (provider, Some(model))
}

fn split_model_slug(model: &str) -> Option<(String, String)> {
    for separator in ['/', ':'] {
        if let Some((derived_provider, derived_model)) = model.split_once(separator) {
            let derived_provider = non_empty(derived_provider);
            let derived_model = non_empty(derived_model);
            if let (Some(derived_provider), Some(derived_model)) = (derived_provider, derived_model)
            {
                return Some((derived_provider, derived_model));
            }
        }
    }
    None
}

fn string_field(payload: &Map<String, Value>, key: &str) -> Option<String> {
    payload.get(key).and_then(|value| match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        Value::Bool(value) => Some(value.to_string()),
        _ => None,
    })
}

fn int_field(payload: &Map<String, Value>, key: &str) -> Option<i64> {
    payload.get(key).and_then(|value| match value {
        Value::Number(value) => value.as_i64(),
        Value::String(value) => value.parse().ok(),
        _ => None,
    })
}

fn nested_i64(payload: Option<&Map<String, Value>>, key: &str) -> Option<i64> {
    payload.and_then(|payload| int_field(payload, key))
}

fn float_field(payload: &Map<String, Value>, key: &str) -> Option<f64> {
    payload.get(key).and_then(|value| match value {
        Value::Number(value) => value.as_f64(),
        Value::String(value) => value.parse().ok(),
        _ => None,
    })
}

fn bool_field(payload: &Map<String, Value>, key: &str) -> Option<bool> {
    payload.get(key).and_then(|value| match value {
        Value::Bool(value) => Some(*value),
        Value::String(value) => value.parse().ok(),
        _ => None,
    })
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn value_len(value: &Value) -> usize {
    match value {
        Value::String(value) => value.len(),
        _ => value.to_string().len(),
    }
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
    parse_elapsed_ms: u128,
    project_elapsed_ms: u128,
    summary_elapsed_ms: u128,
    finish_elapsed_ms: u128,
    warnings: usize,
}

struct ImportedFile {
    events: usize,
    parse_elapsed_ms: u128,
    project_elapsed_ms: u128,
    summary_elapsed_ms: u128,
    finish_elapsed_ms: u128,
    warnings: usize,
}

impl ImportedFile {
    fn skipped() -> Self {
        Self {
            events: 0,
            parse_elapsed_ms: 0,
            project_elapsed_ms: 0,
            summary_elapsed_ms: 0,
            finish_elapsed_ms: 0,
            warnings: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use anyhow::Result;

    use crate::db::Database;

    use super::import;

    #[test]
    fn imports_pi_session_metadata_into_operation_tables() -> Result<()> {
        let root = temp_dir("pi-import");
        fs::create_dir_all(&root)?;
        let session_path = root.join("pi-session.jsonl");
        fs::write(
            &session_path,
            r#"{"type":"session","id":"session-1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp/project","version":1}"#
                .to_string()
                + "\n"
                + r#"{"type":"model_change","id":"model-1","parentId":"session-1","timestamp":"2026-01-01T00:00:00.100Z","provider":"openai-codex","modelId":"gpt-test"}"#
                + "\n"
                + r#"{"type":"message","id":"user-1","parentId":"model-1","timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"redacted"}],"timestamp":1767225601000}}"#
                + "\n"
                + r#"{"type":"message","id":"assistant-1","parentId":"user-1","timestamp":"2026-01-01T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"redacted"},{"type":"toolCall","id":"call-1","name":"bash","arguments":{"command":"true"}}],"api":"openai-codex-responses","provider":"openai-codex","model":"gpt-test","responseId":"resp-1","stopReason":"toolUse","usage":{"input":60,"output":20,"cacheRead":40,"cacheWrite":0,"totalTokens":120,"cost":{"input":0.01,"output":0.02,"cacheRead":0.003,"cacheWrite":0,"total":0.033}},"timestamp":1767225602000}}"#
                + "\n"
                + r#"{"type":"message","id":"tool-1","parentId":"assistant-1","timestamp":"2026-01-01T00:00:03.000Z","message":{"role":"toolResult","toolCallId":"call-1","toolName":"bash","content":[{"type":"text","text":"ok"}],"isError":false,"timestamp":1767225603000}}"#
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

        let (input_tokens, cache_read_tokens, total_cost_usd): (i64, i64, f64) =
            db.connection().query_row(
                "SELECT input_tokens, cache_read_tokens, total_cost_usd FROM llm_calls",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
        assert_eq!(input_tokens, 100);
        assert_eq!(cache_read_tokens, 40);
        assert!((total_cost_usd - 0.033).abs() < f64::EPSILON);

        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn splits_provider_prefix_from_pi_model_names() -> Result<()> {
        let root = temp_dir("pi-import-provider-model");
        fs::create_dir_all(&root)?;
        let session_path = root.join("pi-session.jsonl");
        fs::write(
            &session_path,
            r#"{"type":"session","id":"session-1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp/project","version":1}"#
                .to_string()
                + "\n"
                + r#"{"type":"model_change","id":"model-1","parentId":"session-1","timestamp":"2026-01-01T00:00:00.100Z","provider":"cline-pass","modelId":"cline-pass/deepseek-v4-flash"}"#
                + "\n"
                + r#"{"type":"message","id":"assistant-1","parentId":"model-1","timestamp":"2026-01-01T00:00:02.000Z","message":{"role":"assistant","content":[],"provider":"cline-pass","model":"cline-pass/deepseek-v4-flash","usage":{"input":60,"output":20,"cacheRead":40,"totalTokens":120},"timestamp":1767225602000}}"#
                + "\n",
        )?;

        let db_path = root.join("catalog.sqlite");
        let db = Database::open(&db_path)?;
        db.migrate()?;

        import(&db, Some(session_path))?;

        let (provider, model): (Option<String>, Option<String>) = db.connection().query_row(
            "SELECT provider, model FROM llm_calls LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(provider.as_deref(), Some("cline-pass"));
        assert_eq!(model.as_deref(), Some("deepseek-v4-flash"));

        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn derives_provider_from_model_slug_when_pi_provider_is_generic() -> Result<()> {
        let root = temp_dir("pi-import-generic-provider-model");
        fs::create_dir_all(&root)?;
        let session_path = root.join("pi-session.jsonl");
        fs::write(
            &session_path,
            r#"{"type":"session","id":"session-1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp/project","version":1}"#
                .to_string()
                + "\n"
                + r#"{"type":"model_change","id":"model-1","parentId":"session-1","timestamp":"2026-01-01T00:00:00.100Z","provider":"cline","modelId":"cline-pass/deepseek-v4-flash"}"#
                + "\n"
                + r#"{"type":"message","id":"assistant-1","parentId":"model-1","timestamp":"2026-01-01T00:00:02.000Z","message":{"role":"assistant","content":[],"provider":"cline","model":"cline-pass/deepseek-v4-flash","usage":{"input":60,"output":20,"cacheRead":40,"totalTokens":120},"timestamp":1767225602000}}"#
                + "\n",
        )?;

        let db_path = root.join("catalog.sqlite");
        let db = Database::open(&db_path)?;
        db.migrate()?;

        import(&db, Some(session_path))?;

        let (provider, model): (Option<String>, Option<String>) = db.connection().query_row(
            "SELECT provider, model FROM llm_calls LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(provider.as_deref(), Some("cline"));
        assert_eq!(model.as_deref(), Some("deepseek-v4-flash"));

        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    fn count(db: &Database, table: &str) -> Result<i64> {
        let sql = format!("SELECT COUNT(*) FROM {table}");
        Ok(db.connection().query_row(&sql, [], |row| row.get(0))?)
    }

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("shirabe-{name}-{}", std::process::id()))
    }
}
