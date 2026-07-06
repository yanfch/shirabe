use std::{
    collections::{HashMap, HashSet},
    env, fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    time::Instant,
};

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};
use walkdir::WalkDir;

use crate::db::{Database, now_ns, system_time_ns};
use crate::projection::{
    event::{
        EntityHint, NormalizedEvent, Operation, OperationStatus, OperationType, TraceContext,
        TurnHint, Usage,
    },
    projector::{ProjectionCache, Projector},
};

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
    let trace_path = path.unwrap_or_else(default_kanade_traces_dir);
    let scan_started = Instant::now();
    let scan = collect_trace_path(&trace_path, identity, writer, modified_since_ns)?;
    let scan_elapsed_ms = scan_started.elapsed().as_millis();
    let warnings = if scan.warnings > 0 {
        vec![format!(
            "{} trace lines could not be parsed and were skipped",
            scan.warnings
        )]
    } else {
        Vec::new()
    };

    Ok(ImportReport {
        source: "kanade".to_string(),
        source_id: format!("{}:kanade:collector", identity.profile_id),
        root_path: trace_path,
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
    let trace_path = path.unwrap_or_else(default_kanade_traces_dir);
    let source_kind = if trace_path.is_file() {
        "otlp_jsonl_file"
    } else {
        "otlp_jsonl_directory"
    };
    let source_id = db.record_import_source_for_profile(
        "kanade",
        source_kind,
        &trace_path,
        &identity.profile_id,
        &identity.device_id,
    )?;

    let scan_started = Instant::now();
    let tx = db.begin_batch()?;
    let scan = scan_trace_path(db, &source_id, &trace_path, modified_since_ns, identity)?;
    tx.commit()?;
    let scan_elapsed_ms = scan_started.elapsed().as_millis();

    let catalog_started = Instant::now();
    let shirabe_bytes_written = catalog_size(db.path())?;
    let catalog_elapsed_ms = catalog_started.elapsed().as_millis();

    let mut warnings = vec![
        "kanade importer reads trace files only; it does not read kanade state.db".to_string(),
    ];
    if scan.warnings > 0 {
        warnings.push(format!(
            "{} trace lines could not be parsed and were skipped",
            scan.warnings
        ));
    }

    Ok(ImportReport {
        source: "kanade".to_string(),
        source_id,
        root_path: trace_path,
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

fn default_kanade_traces_dir() -> PathBuf {
    env::var_os("KANADE_TRACES_DIR")
        .map(PathBuf::from)
        .or_else(|| env::var_os("KANADE_DIR").map(|root| PathBuf::from(root).join("traces")))
        .or_else(|| {
            env::var_os("HOME").map(|home| PathBuf::from(home).join(".kanade").join("traces"))
        })
        .unwrap_or_else(|| PathBuf::from(".kanade").join("traces"))
}

fn collect_trace_path(
    path: &Path,
    identity: &ImportIdentity,
    writer: &mut dyn Write,
    modified_since_ns: Option<i64>,
) -> Result<ScanSize> {
    if path.is_file() {
        let metadata = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
        let modified_ns = metadata.modified().map(system_time_ns).unwrap_or_default();
        if modified_since_ns.is_some_and(|since| modified_ns < since) {
            return Ok(ScanSize::default());
        }
        let collected = collect_trace_file(path, identity, writer)?;
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

    if !path.exists() {
        return Ok(ScanSize::default());
    }

    for entry in WalkDir::new(path).follow_links(false) {
        let entry = entry.with_context(|| format!("scan {}", path.display()))?;
        if entry.file_type().is_file() && is_jsonl(entry.path()) {
            let metadata = entry.metadata()?;
            let modified_ns = metadata.modified().map(system_time_ns).unwrap_or_default();
            if modified_since_ns.is_some_and(|since| modified_ns < since) {
                continue;
            }
            files_seen += 1;
            bytes = bytes.saturating_add(metadata.len());
            let collected = collect_trace_file(entry.path(), identity, writer)?;
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

fn collect_trace_file(
    path: &Path,
    identity: &ImportIdentity,
    writer: &mut dyn Write,
) -> Result<ImportedFile> {
    let file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut events = 0usize;
    let mut warnings = 0usize;

    for (index, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("read {}", path.display()))?;
        match parse_span_line(&line, path, index + 1, identity) {
            Ok(Some(event)) => {
                write_collected_event(writer, &event)?;
                events += 1;
            }
            Ok(None) => {}
            Err(_) => warnings += 1,
        }
    }

    Ok(ImportedFile { events, warnings })
}

fn scan_trace_path(
    db: &Database,
    source_id: &str,
    path: &Path,
    modified_since_ns: Option<i64>,
    identity: &ImportIdentity,
) -> Result<ScanSize> {
    if path.is_file() {
        let metadata = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
        let modified_ns = metadata.modified().map(system_time_ns).unwrap_or_default();
        if modified_since_ns.is_some_and(|since| modified_ns < since) {
            return Ok(ScanSize::default());
        }
        let prepared = db.prepare_import_file(source_id, path, metadata.len(), modified_ns)?;
        let imported = if prepared.should_import {
            import_trace_file(db, &prepared.file_id, path, identity)?
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

    if !path.exists() {
        return Ok(ScanSize {
            files_seen,
            files_imported,
            files_skipped,
            bytes,
            events,
            warnings,
        });
    }

    for entry in WalkDir::new(path).follow_links(false) {
        let entry = entry.with_context(|| format!("scan {}", path.display()))?;
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
            let imported = import_trace_file(db, &prepared.file_id, entry.path(), identity)?;
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

fn import_trace_file(
    db: &Database,
    file_id: &str,
    path: &Path,
    identity: &ImportIdentity,
) -> Result<ImportedFile> {
    let file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let projector = Projector::new(db);
    let mut line = String::new();
    let mut line_number = 0usize;
    let mut offset = 0u64;
    let mut events = 0usize;
    let mut warnings = 0usize;
    let mut first_event_ns: Option<i64> = None;
    let mut last_event_ns: Option<i64> = None;
    let mut projection_cache = ProjectionCache::default();
    let mut touched_session_ids = HashSet::new();
    let mut touched_run_ids = HashSet::new();
    let mut turn_indexes = HashMap::<String, i64>::new();

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

        match parse_span_line(&line, path, line_number, identity) {
            Ok(Some(mut event)) => {
                if let Some(turn) = event.turn.as_mut() {
                    let index = turn_indexes
                        .entry(event.run.external_id.clone())
                        .and_modify(|value| *value += 1)
                        .or_insert(1);
                    turn.index = Some(*index);
                }
                let occurred_at_ns = event.occurred_at_ns;
                let projection = projector.project_with_cache(&event, &mut projection_cache)?;
                if let Some(session_id) = projection.session_id.as_ref() {
                    touched_session_ids.insert(session_id.clone());
                }
                touched_run_ids.insert(projection.run_id);
                events += 1;
                first_event_ns =
                    Some(first_event_ns.map_or(occurred_at_ns, |value| value.min(occurred_at_ns)));
                last_event_ns =
                    Some(last_event_ns.map_or(occurred_at_ns, |value| value.max(occurred_at_ns)));
            }
            Ok(None) => {}
            Err(_) => warnings += 1,
        }
    }

    projector.refresh_run_summaries(touched_run_ids.iter().map(String::as_str))?;
    db.refresh_observed_llm_latency_for_runs(touched_run_ids.iter().map(String::as_str))?;
    projector.refresh_session_summaries(touched_session_ids.iter().map(String::as_str))?;

    let status = if warnings > 0 { "partial" } else { "imported" };
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

    Ok(ImportedFile { events, warnings })
}

fn parse_span_line(
    line: &str,
    path: &Path,
    line_number: usize,
    identity: &ImportIdentity,
) -> Result<Option<NormalizedEvent>> {
    let value: Value = serde_json::from_str(line).context("parse kanade trace json")?;
    let attributes = value
        .get("attributes")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    let Some(task_id) = string_attr(&attributes, "kanade.task.id") else {
        return Ok(None);
    };

    let name = value
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("span")
        .to_string();
    let started_at_ns = time_ns(value.get("startTime")).unwrap_or_default();
    let ended_at_ns = time_ns(value.get("endTime"));
    let duration_ns = ended_at_ns.map(|end| end.saturating_sub(started_at_ns));
    let span_id = value.get("spanId").and_then(Value::as_str);
    let source_event_id = span_id.map(ToOwned::to_owned);
    let source_ref = Some(format!("{}:{line_number}", path.display()));
    let usage = usage_from_attributes(&attributes);
    let operation_type = operation_type(&name, &attributes, &usage);
    let status = operation_status(value.get("status"), &attributes);
    let title = string_attr(&attributes, "kanade.workflow.name")
        .or_else(|| string_attr(&attributes, "kanade.workflow.phase"))
        .or_else(|| string_attr(&attributes, "kanade.task.source"));

    let mut metadata = Map::new();
    metadata.insert("span_name".to_string(), json!(name));
    metadata.insert("attributes".to_string(), Value::Object(attributes.clone()));
    if let Some(resource) = value.get("resource") {
        metadata.insert("resource".to_string(), resource.clone());
    }

    Ok(Some(NormalizedEvent {
        profile_id: identity.profile_id.clone(),
        device_id: identity.device_id.clone(),
        source: "kanade".to_string(),
        source_kind: "otlp_jsonl_span".to_string(),
        source_event_id,
        observed_at_ns: now_ns(),
        occurred_at_ns: started_at_ns,
        source_ref,
        cwd: None,
        project_id: string_attr(&attributes, "kanade.project.id"),
        trace: TraceContext {
            trace_id: value
                .get("traceId")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            span_id: span_id.map(ToOwned::to_owned),
            parent_span_id: value
                .get("parentSpanId")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            root_span_id: value
                .get("parentSpanId")
                .and_then(Value::as_str)
                .is_none()
                .then(|| span_id.map(ToOwned::to_owned))
                .flatten(),
        },
        session: Some(EntityHint {
            external_id: format!("task:{task_id}"),
            kind: "workflow_task".to_string(),
            title: title.clone(),
        }),
        run: EntityHint {
            external_id: task_id.clone(),
            kind: "workflow_task".to_string(),
            title,
        },
        turn: kanade_turn_hint(&task_id, &name, &attributes, span_id, line_number),
        operation: Operation {
            operation_type,
            name,
            status,
            error_type: status
                .eq(&OperationStatus::Failed)
                .then(|| "otel_status_error".to_string()),
            started_at_ns,
            ended_at_ns,
            duration_ns,
            order_index: Some(line_number.min(i64::MAX as usize) as i64),
            metadata: Some(Value::Object(metadata)),
        },
        usage,
        skill_events: Vec::new(),
        confidence: 1.0,
    }))
}

fn operation_type(name: &str, attributes: &Map<String, Value>, usage: &Usage) -> OperationType {
    if usage.input_tokens > 0 || usage.output_tokens > 0 {
        return OperationType::LlmCall;
    }

    match name {
        "workflow.task" => OperationType::Workflow,
        "workflow.phase" => OperationType::Phase,
        "workflow.agent" | "workflow.author" => OperationType::Agent,
        _ if attributes.contains_key("kanade.agent.label") => OperationType::Agent,
        _ => OperationType::System,
    }
}

fn operation_status(status: Option<&Value>, attributes: &Map<String, Value>) -> OperationStatus {
    if string_attr(attributes, "kanade.agent.from_cache").as_deref() == Some("true") {
        return OperationStatus::FromCache;
    }

    if string_attr(attributes, "kanade.task.status").as_deref() == Some("failed") {
        return OperationStatus::Failed;
    }

    match status
        .and_then(|status| status.get("code"))
        .and_then(Value::as_i64)
    {
        Some(2) => OperationStatus::Failed,
        _ => OperationStatus::Success,
    }
}

fn usage_from_attributes(attributes: &Map<String, Value>) -> Usage {
    let uncached_input_tokens =
        int_attr(attributes, "gen_ai.usage.input_tokens").unwrap_or_default();
    let output_tokens = int_attr(attributes, "gen_ai.usage.output_tokens").unwrap_or_default();
    let cache_read_tokens =
        int_attr(attributes, "gen_ai.usage.cache_read.input_tokens").unwrap_or_default();
    let cache_write_tokens =
        int_attr(attributes, "gen_ai.usage.cache_creation.input_tokens").unwrap_or_default();
    let reported_total_tokens = int_attr(attributes, "gen_ai.usage.total_tokens");
    let input_tokens = reported_total_tokens
        .map(|total| total.saturating_sub(output_tokens).max(0))
        .unwrap_or_else(|| {
            uncached_input_tokens
                .saturating_add(cache_read_tokens)
                .saturating_add(cache_write_tokens)
        });
    let (provider, model) = normalize_kanade_model(
        non_empty_string_attr(attributes, "kanade.agent.model")
            .or_else(|| non_empty_string_attr(attributes, "kanade.author.model")),
    );

    Usage {
        provider,
        model,
        input_tokens,
        output_tokens,
        uncached_input_tokens,
        cache_read_tokens,
        cache_write_tokens,
        total_cost_usd: float_attr(attributes, "gen_ai.usage.cost_usd").unwrap_or_default(),
        pricing_status: Some("reported".to_string()),
        cost_confidence: Some("reported".to_string()),
        ..Usage::default()
    }
}

fn kanade_turn_hint(
    task_id: &str,
    name: &str,
    attributes: &Map<String, Value>,
    span_id: Option<&str>,
    line_number: usize,
) -> Option<TurnHint> {
    let is_turn_span = name == "workflow.agent"
        || name == "workflow.author"
        || attributes.contains_key("kanade.agent.label")
        || attributes.contains_key("kanade.author.model")
        || attributes.contains_key("gen_ai.usage.input_tokens")
        || attributes.contains_key("gen_ai.usage.output_tokens");
    if !is_turn_span {
        return None;
    }

    let external_id = span_id
        .map(|span_id| format!("{task_id}:span:{span_id}"))
        .unwrap_or_else(|| format!("{task_id}:line:{line_number}"));
    let role = non_empty_string_attr(attributes, "kanade.agent.role")
        .or_else(|| non_empty_string_attr(attributes, "kanade.agent.label"))
        .or_else(|| (name == "workflow.author").then(|| "author".to_string()))
        .or_else(|| (name == "workflow.agent").then(|| "agent".to_string()));

    Some(TurnHint {
        external_id,
        index: None,
        role,
    })
}

fn normalize_kanade_model(raw: Option<String>) -> (Option<String>, Option<String>) {
    let Some(raw) = raw else {
        return (Some("kanade".to_string()), None);
    };

    if let Some((provider, model)) = raw.split_once('/') {
        return (
            non_empty(provider).or_else(|| Some("kanade".to_string())),
            non_empty(model),
        );
    }

    if let Some((provider, model)) = raw.split_once(':') {
        return (
            non_empty(provider).or_else(|| Some("kanade".to_string())),
            non_empty(model),
        );
    }

    (Some("kanade".to_string()), Some(raw))
}

fn non_empty_string_attr(attributes: &Map<String, Value>, key: &str) -> Option<String> {
    string_attr(attributes, key).and_then(|value| non_empty(&value))
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn string_attr(attributes: &Map<String, Value>, key: &str) -> Option<String> {
    attributes.get(key).and_then(|value| match value {
        Value::String(value) => Some(value.clone()),
        Value::Bool(value) => Some(value.to_string()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    })
}

fn int_attr(attributes: &Map<String, Value>, key: &str) -> Option<i64> {
    attributes.get(key).and_then(|value| match value {
        Value::Number(value) => value.as_i64(),
        Value::String(value) => value.parse().ok(),
        _ => None,
    })
}

fn float_attr(attributes: &Map<String, Value>, key: &str) -> Option<f64> {
    attributes.get(key).and_then(|value| match value {
        Value::Number(value) => value.as_f64(),
        Value::String(value) => value.parse().ok(),
        _ => None,
    })
}

fn time_ns(value: Option<&Value>) -> Option<i64> {
    let values = value?.as_array()?;
    let seconds = values.first()?.as_i64()?;
    let nanos = values.get(1).and_then(Value::as_i64).unwrap_or_default();
    Some(seconds.saturating_mul(1_000_000_000).saturating_add(nanos))
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
    use std::{fs, path::PathBuf};

    use anyhow::Result;

    use crate::db::Database;

    use super::import;

    #[test]
    fn imports_kanade_trace_spans_into_operation_tables() -> Result<()> {
        let root = temp_dir("kanade-import");
        fs::create_dir_all(&root)?;
        let trace_path = root.join("kanade.jsonl");
        fs::write(
            &trace_path,
            r#"{"traceId":"trace-1","spanId":"root","name":"workflow.task","kind":1,"startTime":[100,0],"endTime":[101,0],"status":{"code":1},"attributes":{"kanade.task.id":"T-1","kanade.task.source":"test","kanade.task.status":"success"},"events":[],"resource":{"service.name":"kanade"}}"#
                .to_string()
                + "\n"
                + r#"{"traceId":"trace-1","spanId":"llm-1","parentSpanId":"root","name":"workflow.agent","kind":1,"startTime":[100,100],"endTime":[100,200],"status":{"code":1},"attributes":{"kanade.task.id":"T-1","kanade.agent.label":"writer","kanade.agent.role":"author","kanade.agent.model":"xiaomi/mimo-v2.5-pro","gen_ai.usage.input_tokens":100,"gen_ai.usage.output_tokens":20,"gen_ai.usage.cache_read.input_tokens":10,"gen_ai.usage.cache_creation.input_tokens":5,"gen_ai.usage.total_tokens":135,"gen_ai.usage.cost_usd":0.001},"events":[],"resource":{"service.name":"kanade"}}"#
                + "\n",
        )?;

        let db_path = root.join("catalog.sqlite");
        let db = Database::open(&db_path)?;
        db.migrate()?;

        let report = import(&db, Some(trace_path))?;

        assert_eq!(report.files_seen, 1);
        assert_eq!(count(&db, "import_files")?, 1);
        assert_eq!(count(&db, "sessions")?, 1);
        assert_eq!(count(&db, "runs")?, 1);
        assert_eq!(count(&db, "turns")?, 1);
        assert_eq!(count(&db, "run_steps")?, 2);
        assert_eq!(count(&db, "llm_calls")?, 1);
        let turn_index = db
            .connection()
            .query_row("SELECT turn_index FROM turns", [], |row| {
                row.get::<_, i64>(0)
            })?;
        assert_eq!(turn_index, 1);
        let usage = db.connection().query_row(
            "SELECT provider, model, input_tokens, uncached_input_tokens, cache_read_tokens, cache_write_tokens, turn_id FROM llm_calls",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            },
        )?;
        assert_eq!(
            usage,
            (
                "xiaomi".to_string(),
                "mimo-v2.5-pro".to_string(),
                115,
                100,
                10,
                5,
                Some("kanade:T-1:span:llm-1".to_string())
            )
        );

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
