use std::{
    collections::{HashMap, HashSet, VecDeque},
    env, fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    time::Instant,
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

use super::{ImportReport, ImportTiming};

pub fn import(db: &Database, path: Option<PathBuf>) -> Result<ImportReport> {
    import_with_modified_since(db, path, None)
}

pub fn import_recent(
    db: &Database,
    path: Option<PathBuf>,
    modified_since_ns: i64,
) -> Result<ImportReport> {
    import_with_modified_since(db, path, Some(modified_since_ns))
}

fn import_with_modified_since(
    db: &Database,
    path: Option<PathBuf>,
    modified_since_ns: Option<i64>,
) -> Result<ImportReport> {
    let total_started = Instant::now();
    let root_path = path.unwrap_or_else(default_claude_projects_dir);
    let source_kind = if root_path.is_file() {
        "claude_project_jsonl_file"
    } else {
        "claude_project_jsonl_directory"
    };
    let source_id = db.record_import_source("claude", source_kind, &root_path)?;

    let scan_started = Instant::now();
    let tx = db.begin_batch()?;
    let scan = scan_sessions(db, &source_id, &root_path, modified_since_ns)?;
    tx.commit()?;
    let scan_elapsed_ms = scan_started.elapsed().as_millis();

    let catalog_started = Instant::now();
    let shirabe_bytes_written = catalog_size(db.path())?;
    let catalog_elapsed_ms = catalog_started.elapsed().as_millis();

    let mut warnings = vec![
        "claude importer uses metadata-first parsing and does not copy prompt, response, or tool output content".to_string(),
    ];
    if scan.warnings > 0 {
        warnings.push(format!(
            "{} Claude session lines could not be parsed and were skipped",
            scan.warnings
        ));
    }

    Ok(ImportReport {
        source: "claude".to_string(),
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

fn default_claude_projects_dir() -> PathBuf {
    env::var_os("CLAUDE_PROJECTS_DIR")
        .map(PathBuf::from)
        .or_else(|| env::var_os("CLAUDE_DIR").map(|root| PathBuf::from(root).join("projects")))
        .or_else(|| {
            env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude").join("projects"))
        })
        .unwrap_or_else(|| PathBuf::from(".claude").join("projects"))
}

fn scan_sessions(
    db: &Database,
    source_id: &str,
    root: &Path,
    modified_since_ns: Option<i64>,
) -> Result<ScanSize> {
    if root.is_file() {
        let metadata = fs::metadata(root).with_context(|| format!("stat {}", root.display()))?;
        let modified_ns = metadata.modified().map(system_time_ns).unwrap_or_default();
        if modified_since_ns.is_some_and(|since| modified_ns < since) {
            return Ok(ScanSize::default());
        }
        let prepared = db.prepare_import_file(source_id, root, metadata.len(), modified_ns)?;
        let imported = if prepared.should_import {
            import_session_file(db, &prepared.file_id, root)?
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
            let imported = import_session_file(db, &prepared.file_id, entry.path())?;
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

fn import_session_file(db: &Database, file_id: &str, path: &Path) -> Result<ImportedFile> {
    let file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let projector = Projector::new(db);
    let mut state = SessionState::from_path(path);
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

        match parse_session_line(&mut state, &line, path, line_number) {
            Ok(parsed) => {
                for event in parsed {
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

fn parse_session_line(
    state: &mut SessionState,
    line: &str,
    path: &Path,
    line_number: usize,
) -> Result<Vec<NormalizedEvent>> {
    let value: Value = serde_json::from_str(line).context("parse Claude session json")?;
    let Some(object) = value.as_object() else {
        return Ok(Vec::new());
    };

    state.apply_outer_fields(object);

    let timestamp_ns = object
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339_ns)
        .unwrap_or_else(now_ns);
    let outer_type = string_field(object, "type").unwrap_or_default();

    if outer_type == "custom-title" {
        state.title = string_field(object, "customTitle").or_else(|| state.title.clone());
        return Ok(Vec::new());
    }

    if outer_type == "user" && object.contains_key("toolUseResult") {
        return Ok(state.tool_result_events(object, path, line_number, timestamp_ns));
    }

    if outer_type == "user" {
        state.start_next_turn();
        return Ok(vec![state.event(
            path,
            line_number,
            timestamp_ns,
            format!("{}:user:{line_number}", state.run_external_id()),
            OperationType::User,
            "user_message".to_string(),
            OperationStatus::Success,
            Usage::default(),
            None,
        )]);
    }

    if outer_type == "assistant" {
        return Ok(state.assistant_events(object, path, line_number, timestamp_ns));
    }

    Ok(Vec::new())
}

#[derive(Clone, Debug)]
struct ToolUseRef {
    id: String,
    name: String,
}

struct SessionState {
    session_external_id: String,
    cwd: Option<String>,
    model: Option<String>,
    title: Option<String>,
    turn_index: i64,
    assistant_tools: HashMap<String, VecDeque<ToolUseRef>>,
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
            title: None,
            turn_index: 0,
            assistant_tools: HashMap::new(),
        }
    }

    fn apply_outer_fields(&mut self, object: &Map<String, Value>) {
        self.session_external_id =
            string_field(object, "sessionId").unwrap_or_else(|| self.session_external_id.clone());
        self.cwd = string_field(object, "cwd").or_else(|| self.cwd.clone());
        self.refresh_title_from_cwd();

        if let Some(message) = object.get("message").and_then(Value::as_object) {
            self.model = string_field(message, "model").or_else(|| self.model.clone());
        }
    }

    fn assistant_events(
        &mut self,
        object: &Map<String, Value>,
        path: &Path,
        line_number: usize,
        timestamp_ns: i64,
    ) -> Vec<NormalizedEvent> {
        self.ensure_turn();
        let mut events = Vec::new();
        let assistant_uuid =
            string_field(object, "uuid").unwrap_or_else(|| format!("{line_number}"));
        let Some(message) = object.get("message").and_then(Value::as_object) else {
            return events;
        };

        if let Some(model) = string_field(message, "model") {
            self.model = Some(model);
        }

        if let Some(usage) = message.get("usage").and_then(Value::as_object) {
            let source_event_id =
                string_field(message, "id").unwrap_or_else(|| assistant_uuid.clone());
            let model = self.model.clone().unwrap_or_else(|| "unknown".to_string());
            let mut event = self.event(
                path,
                line_number,
                timestamp_ns,
                source_event_id,
                OperationType::LlmCall,
                model.clone(),
                OperationStatus::Success,
                usage_from_message(message, usage),
                None,
            );
            event.skill_events = skills::claude::attributed_skill_events(object, message);
            events.push(event);
        }

        if let Some(content) = message.get("content").and_then(Value::as_array) {
            for item in content {
                let Some(item) = item.as_object() else {
                    continue;
                };
                if string_field(item, "type").as_deref() != Some("tool_use") {
                    continue;
                }

                let tool_id = string_field(item, "id")
                    .unwrap_or_else(|| format!("{assistant_uuid}:tool:{line_number}"));
                let tool_name =
                    string_field(item, "name").unwrap_or_else(|| "tool_use".to_string());
                let skill_events = skills::claude::skill_events_from_tool_use(item, &tool_name);
                self.assistant_tools
                    .entry(assistant_uuid.clone())
                    .or_default()
                    .push_back(ToolUseRef {
                        id: tool_id.clone(),
                        name: tool_name.clone(),
                    });
                let mut event = self.event(
                    path,
                    line_number,
                    timestamp_ns,
                    tool_id,
                    OperationType::ToolCall,
                    tool_name,
                    OperationStatus::Success,
                    Usage::default(),
                    None,
                );
                event.skill_events = skill_events;
                events.push(event);
            }
        }

        events
    }

    fn tool_result_events(
        &mut self,
        object: &Map<String, Value>,
        path: &Path,
        line_number: usize,
        timestamp_ns: i64,
    ) -> Vec<NormalizedEvent> {
        self.ensure_turn();
        let assistant_uuid = string_field(object, "sourceToolAssistantUUID")
            .or_else(|| string_field(object, "parentUuid"))
            .unwrap_or_else(|| format!("{line_number}"));
        let Some(tool) = self
            .assistant_tools
            .get_mut(&assistant_uuid)
            .and_then(VecDeque::pop_front)
        else {
            return Vec::new();
        };
        let result = object.get("toolUseResult");
        let metadata = result.and_then(tool_result_metadata);

        vec![self.event(
            path,
            line_number,
            timestamp_ns,
            tool.id,
            OperationType::ToolCall,
            tool.name,
            tool_result_status(result),
            Usage::default(),
            metadata,
        )]
    }

    fn start_next_turn(&mut self) {
        if self.turn_index == 0 {
            self.turn_index = 1;
        } else {
            self.turn_index += 1;
        }
    }

    fn ensure_turn(&mut self) {
        if self.turn_index == 0 {
            self.turn_index = 1;
        }
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

    fn run_external_id(&self) -> String {
        format!(
            "{}:turn:{}",
            self.session_external_id,
            self.turn_index.max(1)
        )
    }

    fn event(
        &self,
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
        let source_event_id = format!("{}:{source_event_id}", self.session_external_id);
        NormalizedEvent {
            source: "claude".to_string(),
            source_kind: "claude_project_jsonl".to_string(),
            source_event_id: Some(source_event_id),
            observed_at_ns: now_ns(),
            occurred_at_ns: timestamp_ns,
            source_ref: Some(format!("{}:{line_number}", path.display())),
            cwd: self.cwd.clone(),
            project_id: self.cwd.as_ref().map(|cwd| stable_hash(cwd)),
            trace: TraceContext::default(),
            session: Some(EntityHint {
                external_id: self.session_external_id.clone(),
                kind: "claude_session".to_string(),
                title: self.title.clone(),
            }),
            run: EntityHint {
                external_id: run_external_id.clone(),
                kind: "claude_turn".to_string(),
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
                    .then(|| "claude_tool_failed".to_string()),
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

fn usage_from_message(message: &Map<String, Value>, usage: &Map<String, Value>) -> Usage {
    let input_tokens = i64_field(usage, "input_tokens")
        .or_else(|| i64_field(usage, "prompt_tokens"))
        .unwrap_or_default();
    let output_tokens = i64_field(usage, "output_tokens")
        .or_else(|| i64_field(usage, "completion_tokens"))
        .unwrap_or_default();
    let cache_read_tokens =
        first_positive_i64_field(usage, &["cache_read_input_tokens", "cached_tokens"])
            .unwrap_or_default();
    let cache_write_tokens = i64_field(usage, "cache_creation_input_tokens")
        .unwrap_or_else(|| cache_creation_detail_tokens(usage));

    Usage {
        provider: Some("anthropic".to_string()),
        model: string_field(message, "model"),
        input_tokens,
        output_tokens,
        uncached_input_tokens: input_tokens,
        cache_read_tokens,
        cache_write_tokens,
        pricing_status: Some("estimated_from_model_prices".to_string()),
        cost_confidence: Some("estimated".to_string()),
        ..Usage::default()
    }
}

fn cache_creation_detail_tokens(usage: &Map<String, Value>) -> i64 {
    let Some(cache_creation) = usage.get("cache_creation").and_then(Value::as_object) else {
        return 0;
    };
    i64_field(cache_creation, "ephemeral_1h_input_tokens")
        .unwrap_or_default()
        .saturating_add(i64_field(cache_creation, "ephemeral_5m_input_tokens").unwrap_or_default())
}

fn tool_result_status(result: Option<&Value>) -> OperationStatus {
    let Some(result) = result else {
        return OperationStatus::Success;
    };
    if let Some(object) = result.as_object() {
        if bool_field(object, "interrupted").unwrap_or(false) {
            return OperationStatus::Failed;
        }
        if bool_field(object, "success").is_some_and(|success| !success) {
            return OperationStatus::Failed;
        }
        if matches!(
            string_field(object, "status").as_deref(),
            Some("failed" | "error" | "rejected")
        ) {
            return OperationStatus::Failed;
        }
    }

    OperationStatus::Success
}

fn tool_result_metadata(result: &Value) -> Option<Value> {
    let output_bytes = match result {
        Value::String(value) => Some(value.len() as i64),
        Value::Array(values) => Some(values.len().min(i64::MAX as usize) as i64),
        Value::Object(object) => object
            .get("bytes")
            .and_then(Value::as_i64)
            .or_else(|| string_field(object, "stdout").map(|value| value.len() as i64))
            .or_else(|| string_field(object, "content").map(|value| value.len() as i64)),
        _ => None,
    };

    output_bytes.map(|value| json!({ "output_bytes": value }))
}

fn string_field(payload: &Map<String, Value>, key: &str) -> Option<String> {
    payload.get(key)?.as_str().map(str::to_string)
}

fn i64_field(payload: &Map<String, Value>, key: &str) -> Option<i64> {
    payload.get(key)?.as_i64()
}

fn first_positive_i64_field(payload: &Map<String, Value>, keys: &[&str]) -> Option<i64> {
    keys.iter()
        .filter_map(|key| i64_field(payload, key))
        .find(|value| *value > 0)
}

fn bool_field(payload: &Map<String, Value>, key: &str) -> Option<bool> {
    payload.get(key)?.as_bool()
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

#[derive(Debug, Default)]
struct ScanSize {
    files_seen: usize,
    files_imported: usize,
    files_skipped: usize,
    bytes: u64,
    events: usize,
    warnings: usize,
}

#[derive(Debug)]
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
    use std::fs;

    use anyhow::Result;

    use crate::db::Database;

    use super::import;

    #[test]
    fn imports_claude_session_usage_and_tools() -> Result<()> {
        let root =
            std::env::temp_dir().join(format!("shirabe-claude-import-{}", std::process::id()));
        fs::create_dir_all(&root)?;
        let session_path = root.join("session-1.jsonl");
        fs::write(
            &session_path,
            r#"{"type":"user","uuid":"user-1","sessionId":"session-1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp/project","message":{"role":"user","content":"redacted"}}"#
                .to_string()
                + "\n"
                + r#"{"type":"assistant","uuid":"assistant-1","sessionId":"session-1","timestamp":"2026-01-01T00:00:01.000Z","cwd":"/tmp/project","message":{"id":"msg-1","type":"message","role":"assistant","model":"claude-test","content":[{"type":"tool_use","id":"tool-1","name":"Bash","input":{"command":"true"}}],"usage":{"input_tokens":100,"output_tokens":20,"cache_read_input_tokens":40,"cache_creation_input_tokens":5}}}"#
                + "\n"
                + r#"{"type":"user","uuid":"result-1","parentUuid":"assistant-1","sourceToolAssistantUUID":"assistant-1","sessionId":"session-1","timestamp":"2026-01-01T00:00:02.000Z","cwd":"/tmp/project","message":{"role":"user","content":""},"toolUseResult":{"stdout":"ok","stderr":"","interrupted":false}}"#
                + "\n",
        )?;

        let db_path = root.join("catalog.sqlite");
        let db = Database::open(&db_path)?;
        db.migrate()?;

        let report = import(&db, Some(session_path))?;

        assert_eq!(report.source, "claude");
        assert_eq!(report.files_seen, 1);
        assert_eq!(report.files_imported, 1);
        assert_eq!(report.events_projected, 4);

        let (input, output, cache_read, cache_write): (i64, i64, i64, i64) =
            db.connection().query_row(
                "SELECT input_tokens, output_tokens, cache_read_tokens, cache_write_tokens FROM llm_calls",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
        assert_eq!((input, output, cache_read, cache_write), (100, 20, 40, 5));

        let (tool_name, status): (String, String) =
            db.connection()
                .query_row("SELECT tool_name, status FROM tool_calls", [], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?;
        assert_eq!(tool_name, "Bash");
        assert_eq!(status, "success");

        let turns: i64 = db
            .connection()
            .query_row("SELECT COUNT(*) FROM turns", [], |row| row.get(0))?;
        assert_eq!(turns, 1);

        let _ = fs::remove_dir_all(root);
        Ok(())
    }
}
