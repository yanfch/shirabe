use std::{
    collections::{HashMap, HashSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::Instant,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer};
use walkdir::WalkDir;

use crate::{
    db::{Database, stable_hash, system_time_ns},
    projection::event::{
        EntityHint, NormalizedEvent, Operation, OperationStatus, OperationType, TraceContext, Usage,
    },
    projection::projector::{ProjectionCache, Projector},
};

use super::{ImportIdentity, ImportReport, ImportTiming, write_collected_event};

const SOURCE_KIND: &str = "amp_local_thread_json";

pub fn import_local_with_identity(
    db: &Database,
    path: PathBuf,
    identity: &ImportIdentity,
) -> Result<ImportReport> {
    import_local_recent_with_identity(db, &[path], i64::MIN, identity)
}

pub fn import_local_recent_with_identity(
    db: &Database,
    roots: &[PathBuf],
    modified_since_ns: i64,
    identity: &ImportIdentity,
) -> Result<ImportReport> {
    let total_started = Instant::now();
    let root_path = report_root(roots);
    // Amp file identities must remain stable when callers group the same files under
    // different root lists, so the catalog source is intentionally path-independent.
    let source_id = db.record_import_source_for_profile(
        "amp",
        SOURCE_KIND,
        Path::new("<amp-local>"),
        &identity.profile_id,
        &identity.device_id,
    )?;
    let scan_started = Instant::now();
    let tx = db.begin_batch()?;
    let files = recent_json_files(roots, modified_since_ns)?;
    let mut stats = ScanStats::default();
    for (path, size, modified_ns) in files {
        stats.files_seen += 1;
        stats.bytes = stats.bytes.saturating_add(size);
        let prepared = db.prepare_import_file(&source_id, &path, size, modified_ns)?;
        if !prepared.should_import {
            stats.files_skipped += 1;
            continue;
        }
        stats.files_imported += 1;
        import_file(db, &prepared.file_id, &path, identity, &mut stats)?;
    }
    tx.commit()?;
    let scan_elapsed_ms = scan_started.elapsed().as_millis();
    let written = fs::metadata(db.path()).map(|m| m.len()).unwrap_or_default();
    Ok(report(
        root_path,
        source_id,
        stats,
        written,
        scan_elapsed_ms,
        total_started,
    ))
}

pub fn collect_local_recent(
    roots: &[PathBuf],
    identity: &ImportIdentity,
    writer: &mut dyn Write,
    modified_since_ns: i64,
) -> Result<ImportReport> {
    let total_started = Instant::now();
    let root_path = report_root(roots);
    let scan_started = Instant::now();
    let mut stats = ScanStats::default();
    for (path, size, _) in recent_json_files(roots, modified_since_ns)? {
        stats.files_seen += 1;
        stats.files_imported += 1;
        stats.bytes = stats.bytes.saturating_add(size);
        let json = match fs::read_to_string(&path) {
            Ok(json) => json,
            Err(_) => {
                stats.warnings += 1;
                continue;
            }
        };
        match parse_thread(&json, SOURCE_KIND, identity) {
            Ok(parsed) => {
                stats.add_diagnostics(&parsed);
                for event in parsed.events {
                    write_collected_event(writer, &event)?;
                    stats.events += 1;
                }
            }
            Err(_) => stats.warnings += 1,
        }
    }
    let scan_elapsed_ms = scan_started.elapsed().as_millis();
    Ok(report(
        root_path,
        format!("{}:amp:collector", identity.profile_id),
        stats,
        0,
        scan_elapsed_ms,
        total_started,
    ))
}

#[derive(Default)]
struct ScanStats {
    files_seen: usize,
    files_imported: usize,
    files_skipped: usize,
    events: usize,
    bytes: u64,
    warnings: usize,
}

impl ScanStats {
    fn add_diagnostics(&mut self, parsed: &ParsedThread) {
        self.warnings = self
            .warnings
            .saturating_add(parsed.split_total_mismatch_count)
            .saturating_add(parsed.aggregate_only_count)
            .saturating_add(parsed.malformed_ledger_count)
            .saturating_add(parsed.ledger_total_mismatch_count);
    }
}

fn import_file(
    db: &Database,
    file_id: &str,
    path: &Path,
    identity: &ImportIdentity,
    stats: &mut ScanStats,
) -> Result<()> {
    let json = match fs::read_to_string(path) {
        Ok(json) => json,
        Err(_) => {
            stats.warnings += 1;
            db.finish_import_file(
                file_id,
                "partial",
                0,
                1,
                None,
                None,
                None,
                Some("Amp JSON file could not be read"),
            )?;
            return Ok(());
        }
    };
    let parsed = match parse_thread(&json, SOURCE_KIND, identity) {
        Ok(parsed) => parsed,
        Err(_) => {
            stats.warnings += 1;
            db.finish_import_file(
                file_id,
                "partial",
                0,
                1,
                Some(json.len() as u64),
                None,
                None,
                Some("Amp JSON schema could not be parsed"),
            )?;
            return Ok(());
        }
    };
    stats.add_diagnostics(&parsed);
    let projector = Projector::new(db);
    let mut cache = ProjectionCache::default();
    let mut run_ids = HashSet::new();
    let mut session_ids = HashSet::new();
    if parsed.used_ledger && parsed.malformed_ledger_count == 0 {
        for (run_id, session_id) in db.retire_source_events(
            &identity.profile_id,
            "amp",
            &parsed.superseded_source_event_ids,
        )? {
            run_ids.insert(run_id);
            if let Some(session_id) = session_id {
                session_ids.insert(session_id);
            }
        }
    }
    let first = parsed.events.iter().map(|e| e.occurred_at_ns).min();
    let last = parsed.events.iter().map(|e| e.occurred_at_ns).max();
    let event_count = parsed.events.len();
    for event in &parsed.events {
        let result = projector.project_with_cache(event, &mut cache)?;
        run_ids.insert(result.run_id);
        if let Some(session_id) = result.session_id {
            session_ids.insert(session_id);
        }
    }
    projector.refresh_run_summaries(run_ids.iter().map(String::as_str))?;
    db.refresh_observed_llm_latency_for_runs(run_ids.iter().map(String::as_str))?;
    projector.refresh_session_summaries(session_ids.iter().map(String::as_str))?;
    let file_warnings = parsed.split_total_mismatch_count
        + parsed.aggregate_only_count
        + parsed.malformed_ledger_count
        + parsed.ledger_total_mismatch_count;
    db.finish_import_file(
        file_id,
        if file_warnings == 0 {
            "imported"
        } else {
            "partial"
        },
        event_count,
        file_warnings,
        Some(json.len() as u64),
        first,
        last,
        None,
    )?;
    stats.events += event_count;
    Ok(())
}

fn recent_json_files(
    roots: &[PathBuf],
    modified_since_ns: i64,
) -> Result<Vec<(PathBuf, u64, i64)>> {
    let mut paths = Vec::new();
    for root in roots {
        let root_metadata = match fs::symlink_metadata(root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("stat {}", root.display())),
        };
        if root_metadata.file_type().is_symlink() {
            continue;
        }
        if root_metadata.is_file() {
            if root
                .extension()
                .and_then(|v| v.to_str())
                .is_some_and(|v| v.eq_ignore_ascii_case("json"))
            {
                paths.push(root.clone());
            }
            continue;
        }
        for entry in WalkDir::new(root).follow_links(false) {
            let entry = entry.with_context(|| format!("scan {}", root.display()))?;
            let metadata = fs::symlink_metadata(entry.path())
                .with_context(|| format!("stat {}", entry.path().display()))?;
            if metadata.file_type().is_file()
                && entry
                    .path()
                    .extension()
                    .and_then(|v| v.to_str())
                    .is_some_and(|v| v.eq_ignore_ascii_case("json"))
            {
                paths.push(entry.into_path());
            }
        }
    }
    paths.sort();
    paths.dedup();
    let mut files = Vec::new();
    for path in paths {
        let metadata =
            fs::symlink_metadata(&path).with_context(|| format!("stat {}", path.display()))?;
        if !metadata.file_type().is_file() {
            continue;
        }
        let modified_ns = metadata.modified().map(system_time_ns).unwrap_or_default();
        if modified_ns >= modified_since_ns {
            files.push((path, metadata.len(), modified_ns));
        }
    }
    Ok(files)
}

fn report_root(roots: &[PathBuf]) -> PathBuf {
    roots.first().cloned().unwrap_or_default()
}

fn report(
    root_path: PathBuf,
    source_id: String,
    stats: ScanStats,
    written: u64,
    scan_ms: u128,
    total: Instant,
) -> ImportReport {
    ImportReport {
        source: "amp".into(),
        source_id,
        root_path,
        files_seen: stats.files_seen,
        files_imported: stats.files_imported,
        files_skipped: stats.files_skipped,
        events_projected: stats.events,
        source_bytes_scanned: stats.bytes,
        shirabe_bytes_written: written,
        timings: vec![
            ImportTiming {
                stage: "scan_project",
                elapsed_ms: scan_ms,
            },
            ImportTiming {
                stage: "total_importer",
                elapsed_ms: total.elapsed().as_millis(),
            },
        ],
        warnings: (stats.warnings > 0)
            .then(|| {
                format!(
                    "{} Amp files or schema diagnostics were skipped or degraded",
                    stats.warnings
                )
            })
            .into_iter()
            .collect(),
    }
}

#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct ParsedThread {
    pub(crate) thread_id: String,
    pub(crate) updated_at_ns: i64,
    pub(crate) events: Vec<NormalizedEvent>,
    pub(crate) superseded_source_event_ids: Vec<String>,
    pub(crate) split_total_mismatch_count: usize,
    pub(crate) used_ledger: bool,
    pub(crate) aggregate_only_count: usize,
    pub(crate) malformed_ledger_count: usize,
    pub(crate) ledger_total_mismatch_count: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Thread {
    #[serde(deserialize_with = "narrow")]
    id: Option<String>,
    #[serde(default, deserialize_with = "narrow")]
    updated_at: Option<String>,
    #[serde(default)]
    messages: Vec<Message>,
    #[serde(default)]
    usage_ledger: LedgerInput,
}

#[derive(Deserialize)]
struct Message {
    #[serde(default)]
    role: Option<String>,
    #[serde(default, rename = "messageId", deserialize_with = "narrow")]
    message_id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default, deserialize_with = "narrow")]
    timestamp: Option<String>,
    #[serde(default)]
    usage: Option<CurrentUsage>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CurrentUsage {
    #[serde(default)]
    model: Option<String>,
    #[serde(default, deserialize_with = "narrow")]
    timestamp: Option<String>,
    #[serde(default)]
    input_tokens: Option<i64>,
    #[serde(default)]
    output_tokens: Option<i64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<i64>,
    #[serde(default)]
    cache_read_input_tokens: Option<i64>,
    #[serde(default)]
    total_input_tokens: Option<i64>,
    #[serde(default)]
    total_tokens: Option<i64>,
    #[serde(default)]
    max_input_tokens: Option<i64>,
}

#[derive(Deserialize)]
struct Ledger {
    events: Vec<LenientLedgerEvent>,
}

#[derive(Default, Deserialize)]
#[serde(untagged)]
enum LedgerInput {
    Ledger(Ledger),
    Null(()),
    Malformed(serde::de::IgnoredAny),
    #[default]
    Absent,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum LenientLedgerEvent {
    Event(LedgerEvent),
    Malformed(serde::de::IgnoredAny),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LedgerEvent {
    #[serde(default, deserialize_with = "narrow")]
    id: Option<String>,
    #[serde(default, deserialize_with = "narrow")]
    timestamp: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    tokens: Option<LedgerTokens>,
    #[serde(default, deserialize_with = "narrow")]
    to_message_id: Option<String>,
}

#[derive(Deserialize)]
struct LedgerTokens {
    #[serde(default)]
    input: Option<i64>,
    #[serde(default)]
    output: Option<i64>,
    #[serde(default)]
    total: Option<i64>,
}

fn narrow<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Option<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Scalar {
        String(String),
        Integer(u64),
        Other(serde::de::IgnoredAny),
    }
    Ok(match Scalar::deserialize(d)? {
        Scalar::String(value) => Some(value),
        Scalar::Integer(value) => Some(value.to_string()),
        Scalar::Other(_) => None,
    })
}

#[allow(dead_code)]
pub(crate) fn parse_thread(
    json: &str,
    source_kind: &str,
    identity: &ImportIdentity,
) -> Result<ParsedThread> {
    let thread: Thread = serde_json::from_str(json)?;
    let thread_id = thread.id.filter(|id| valid_thread_id(id));
    let Some(thread_id) = thread_id else {
        bail!("invalid Amp thread id")
    };
    let updated_at_ns = thread
        .updated_at
        .as_deref()
        .and_then(timestamp_ns)
        .unwrap_or_default();

    let mut diagnostics = Diagnostics::default();
    let mut message_events = Vec::new();
    let mut cache = HashMap::new();
    for (index, message) in thread.messages.iter().enumerate() {
        let Some(usage) = message
            .usage
            .as_ref()
            .filter(|_| message.role.as_deref() == Some("assistant"))
        else {
            continue;
        };
        if let Some(id) = message
            .message_id
            .as_deref()
            .filter(|id| !id.trim().is_empty())
        {
            let cache_write = usage.cache_creation_input_tokens.unwrap_or(0);
            let cache_read = usage.cache_read_input_tokens.unwrap_or(0);
            if cache_write >= 0 && cache_read >= 0 {
                cache.insert(id.to_string(), (cache_write, cache_read));
            }
        }
        let Some(model) = usage
            .model
            .as_deref()
            .or(message.model.as_deref())
            .filter(|v| !v.is_empty())
        else {
            continue;
        };
        let Some(occurred) = usage
            .timestamp
            .as_deref()
            .or(message.timestamp.as_deref())
            .and_then(timestamp_ns)
        else {
            continue;
        };
        let Some(mapped) = map_current(usage, &mut diagnostics) else {
            continue;
        };
        let explicit_id = message
            .message_id
            .as_deref()
            .filter(|id| !id.trim().is_empty());
        let identity_fallback = explicit_id.is_none();
        let id = message
            .message_id
            .as_deref()
            .filter(|id| !id.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| stable_hash(&format!("{thread_id}:{occurred}:{index}:{model}")));
        message_events.push(make_event(
            identity,
            source_kind,
            &thread_id,
            "message",
            &id,
            occurred,
            model,
            mapped,
            index,
            identity_fallback,
        ));
    }

    let mut ledger_events = Vec::new();
    let mut ledger_total_mismatch_count = 0_usize;
    let mut malformed_ledger_count =
        usize::from(matches!(thread.usage_ledger, LedgerInput::Malformed(_)));
    if let LedgerInput::Ledger(ledger) = thread.usage_ledger {
        for (index, item) in ledger.events.iter().enumerate() {
            let LenientLedgerEvent::Event(item) = item else {
                malformed_ledger_count += 1;
                continue;
            };
            let Some(tokens) = item.tokens.as_ref() else {
                continue;
            };
            let values = [
                tokens.input.unwrap_or(0),
                tokens.output.unwrap_or(0),
                tokens.total.unwrap_or(0),
            ];
            if values.iter().any(|v| *v < 0) {
                continue;
            }
            let Some(model) = item.model.as_deref().filter(|v| !v.is_empty()) else {
                continue;
            };
            let Some(occurred) = item.timestamp.as_deref().and_then(timestamp_ns) else {
                continue;
            };
            let output = tokens.output.unwrap_or(0);
            let input = if let Some(input) = tokens.input {
                if let Some(total) = tokens.total {
                    let Some(split_total) = input.checked_add(output) else {
                        continue;
                    };
                    if split_total != total {
                        ledger_total_mismatch_count = ledger_total_mismatch_count.saturating_add(1);
                    }
                }
                input
            } else if let Some(total) = tokens.total {
                let Some(input) = total.checked_sub(output).filter(|input| *input >= 0) else {
                    continue;
                };
                input
            } else {
                0
            };
            let (cache_write, cache_read) = item
                .to_message_id
                .as_ref()
                .and_then(|id| cache.get(id))
                .copied()
                .unwrap_or_default();
            if input == 0 && output == 0 && cache_write == 0 && cache_read == 0 {
                continue;
            }
            let explicit_id = item.id.as_deref().filter(|id| !id.trim().is_empty());
            let identity_fallback = explicit_id.is_none();
            let id = item
                .id
                .as_deref()
                .filter(|id| !id.trim().is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| stable_hash(&format!("{thread_id}:{occurred}:{index}:{model}")));
            ledger_events.push(make_event(
                identity,
                source_kind,
                &thread_id,
                "ledger",
                &id,
                occurred,
                model,
                Mapped {
                    input,
                    output,
                    cache_write,
                    cache_read,
                    context: None,
                    confidence: 0.9,
                    aggregate_fallback: false,
                },
                index,
                identity_fallback,
            ));
        }
    }
    let used_ledger = !ledger_events.is_empty();
    let superseded_source_event_ids = if used_ledger {
        message_events
            .iter()
            .filter_map(|e| e.source_event_id.clone())
            .collect()
    } else {
        Vec::new()
    };
    Ok(ParsedThread {
        thread_id,
        updated_at_ns,
        events: if used_ledger {
            ledger_events
        } else {
            message_events
        },
        superseded_source_event_ids,
        split_total_mismatch_count: diagnostics.mismatches,
        aggregate_only_count: diagnostics.aggregates,
        malformed_ledger_count,
        ledger_total_mismatch_count,
        used_ledger,
    })
}

#[derive(Default)]
struct Diagnostics {
    mismatches: usize,
    aggregates: usize,
}
struct Mapped {
    input: i64,
    output: i64,
    cache_write: i64,
    cache_read: i64,
    context: Option<i64>,
    confidence: f64,
    aggregate_fallback: bool,
}

fn map_current(value: &CurrentUsage, diagnostics: &mut Diagnostics) -> Option<Mapped> {
    let supplied = [
        value.input_tokens,
        value.output_tokens,
        value.cache_creation_input_tokens,
        value.cache_read_input_tokens,
        value.total_input_tokens,
        value.total_tokens,
        value.max_input_tokens,
    ];
    if supplied.iter().flatten().any(|v| *v < 0) {
        return None;
    }
    let split_present = value.input_tokens.is_some()
        || value.cache_creation_input_tokens.is_some()
        || value.cache_read_input_tokens.is_some();
    let (input, cache_write, cache_read, confidence, aggregate_fallback) = if split_present {
        let values = (
            value.input_tokens.unwrap_or(0),
            value.cache_creation_input_tokens.unwrap_or(0),
            value.cache_read_input_tokens.unwrap_or(0),
        );
        let split_total = values.0.checked_add(values.1)?.checked_add(values.2)?;
        if value
            .total_input_tokens
            .is_some_and(|total| total != split_total)
        {
            diagnostics.mismatches += 1;
        }
        (values.0, values.1, values.2, 0.9, false)
    } else {
        let aggregate = value.total_input_tokens.or(value.total_tokens).unwrap_or(0);
        if aggregate > 0 {
            diagnostics.aggregates += 1;
        }
        (aggregate, 0, 0, 0.6, aggregate > 0)
    };
    let output = value.output_tokens.unwrap_or(0);
    (input != 0 || output != 0 || cache_write != 0 || cache_read != 0).then_some(Mapped {
        input,
        output,
        cache_write,
        cache_read,
        context: value.max_input_tokens,
        confidence,
        aggregate_fallback,
    })
}

fn make_event(
    identity: &ImportIdentity,
    source_kind: &str,
    thread: &str,
    schema: &str,
    id: &str,
    occurred: i64,
    model: &str,
    mapped: Mapped,
    index: usize,
    identity_fallback: bool,
) -> NormalizedEvent {
    let external_id = format!("amp:{thread}:{schema}:{id}");
    NormalizedEvent {
        profile_id: identity.profile_id.clone(),
        device_id: identity.device_id.clone(),
        source: "amp".into(),
        source_kind: source_kind.into(),
        source_event_id: Some(external_id.clone()),
        observed_at_ns: occurred,
        occurred_at_ns: occurred,
        source_ref: None,
        cwd: None,
        project_id: None,
        trace: TraceContext::default(),
        session: Some(EntityHint {
            external_id: thread.into(),
            kind: "amp_thread".into(),
            title: None,
        }),
        run: EntityHint {
            external_id: thread.into(),
            kind: "amp_thread".into(),
            title: None,
        },
        turn: None,
        operation: Operation {
            operation_type: OperationType::LlmCall,
            name: "llm_call".into(),
            status: OperationStatus::Success,
            error_type: None,
            started_at_ns: occurred,
            ended_at_ns: Some(occurred),
            duration_ns: None,
            order_index: Some(index as i64),
            metadata: match (identity_fallback, mapped.aggregate_fallback) {
                (false, false) => None,
                (true, false) => Some(serde_json::json!({ "identity_fallback": true })),
                (false, true) => Some(serde_json::json!({ "aggregate_fallback": true })),
                (true, true) => Some(serde_json::json!({
                    "identity_fallback": true,
                    "aggregate_fallback": true
                })),
            },
        },
        usage: Usage {
            provider: provider(model).map(str::to_string),
            model: Some(model.into()),
            input_tokens: mapped.input,
            output_tokens: mapped.output,
            uncached_input_tokens: mapped.input,
            cache_read_tokens: mapped.cache_read,
            cache_write_tokens: mapped.cache_write,
            pricing_status: Some("estimated_from_model_prices".into()),
            cost_confidence: Some("estimated".into()),
            model_context_window: mapped.context,
            ..Usage::default()
        },
        skill_events: Vec::new(),
        confidence: if identity_fallback {
            mapped.confidence * 0.8
        } else {
            mapped.confidence
        },
    }
}

fn provider(model: &str) -> Option<&'static str> {
    let lower = model.to_ascii_lowercase();
    if lower.starts_with("claude") {
        Some("anthropic")
    } else if lower.starts_with("gpt")
        || lower.starts_with('o')
            && lower[1..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit())
    {
        Some("openai")
    } else {
        None
    }
}
fn valid_thread_id(id: &str) -> bool {
    id.strip_prefix("T-").is_some_and(|suffix| {
        !suffix.is_empty()
            && suffix
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
    })
}
fn timestamp_ns(value: &str) -> Option<i64> {
    value
        .parse::<u64>()
        .ok()
        .and_then(|v| i64::try_from(v).ok())
        .and_then(|v| v.checked_mul(1_000_000))
        .or_else(|| parse_rfc3339_ns(value))
}
fn parse_rfc3339_ns(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes.len() > 30
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || *bytes.last()? != b'Z'
    {
        return None;
    }
    let parse_digits = |range: std::ops::Range<usize>| -> Option<i64> {
        let digits = bytes.get(range)?;
        digits.iter().all(u8::is_ascii_digit).then(|| {
            digits
                .iter()
                .fold(0_i64, |value, digit| value * 10 + i64::from(digit - b'0'))
        })
    };
    let year = parse_digits(0..4)?;
    let month = parse_digits(5..7)?;
    let day = parse_digits(8..10)?;
    let hour = parse_digits(11..13)?;
    let minute = parse_digits(14..16)?;
    let second = parse_digits(17..19)?;
    if year == 0 || !(1..=12).contains(&month) || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if day == 0 || day > month_days[(month - 1) as usize] {
        return None;
    }
    let nanos = if bytes.len() == 20 {
        0
    } else {
        if bytes[19] != b'.' {
            return None;
        }
        let fraction = &bytes[20..bytes.len() - 1];
        if fraction.is_empty() || fraction.len() > 9 || !fraction.iter().all(u8::is_ascii_digit) {
            return None;
        }
        fraction
            .iter()
            .fold(0_i64, |value, digit| value * 10 + i64::from(digit - b'0'))
            .checked_mul(10_i64.pow((9 - fraction.len()) as u32))?
    };
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mp = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let days = era * 146097 + (yoe * 365 + yoe / 4 - yoe / 100 + doy) - 719468;
    days.checked_mul(86_400_000_000_000)?
        .checked_add(hour.checked_mul(3_600_000_000_000)?)?
        .checked_add(minute.checked_mul(60_000_000_000)?)?
        .checked_add(second.checked_mul(1_000_000_000)?)?
        .checked_add(nanos)
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf, time::SystemTime};

    use crate::db::Database;

    use super::*;
    fn parse(json: &str) -> Result<ParsedThread> {
        parse_thread(json, "amp_thread_json", &ImportIdentity::new("p", "d"))
    }
    fn wrap(extra: &str, messages: &str) -> String {
        format!(
            r#"{{"id":"T-test-1","updatedAt":"2026-01-01T00:00:03Z","messages":{messages}{extra}}}"#
        )
    }

    #[test]
    fn current_exact_buckets_and_private_data_is_absent() {
        let private = "PRIVATE_SENTINEL";
        let json = wrap(
            "",
            &format!(
                r#"[{{"role":"assistant","messageId":"m-1","content":"{private}","thinking":{{"x":"{private}"}},"model":"fallback","usage":{{"model":"claude-sonnet","timestamp":1767225602000,"inputTokens":10,"outputTokens":5,"cacheCreationInputTokens":3,"cacheReadInputTokens":2,"totalInputTokens":15,"maxInputTokens":200000}}}}]"#
            ),
        );
        let parsed = parse(&json).unwrap();
        let event = &parsed.events[0];
        assert_eq!(
            (
                event.usage.input_tokens,
                event.usage.uncached_input_tokens,
                event.usage.output_tokens,
                event.usage.cache_write_tokens,
                event.usage.cache_read_tokens
            ),
            (10, 10, 5, 3, 2)
        );
        assert_eq!(event.usage.provider.as_deref(), Some("anthropic"));
        assert_eq!(event.usage.model_context_window, Some(200000));
        assert_eq!(
            event.source_event_id.as_deref(),
            Some("amp:T-test-1:message:m-1")
        );
        assert!(!serde_json::to_string(event).unwrap().contains(private));
        assert!(!format!("{parsed:?}").contains(private));
    }
    #[test]
    fn empty_and_malformed_ledger_fall_back() {
        for ledger in [
            r#","usageLedger":{"events":[]}"#,
            r#","usageLedger":"bad""#,
            r#","usageLedger":{"events":"bad"}"#,
        ] {
            let p=parse(&wrap(ledger,r#"[{"role":"assistant","messageId":1,"model":"gpt-4","timestamp":"2026-01-01T00:00:00Z","usage":{"inputTokens":1}}]"#)).unwrap();
            assert_eq!(p.events.len(), 1);
            assert!(!p.used_ledger);
        }
    }
    #[test]
    fn malformed_whole_ledger_is_diagnosed_without_retaining_private_data() {
        let private = "PRIVATE_LEDGER_SENTINEL";
        let p = parse(&wrap(
            &format!(r#","usageLedger":{{"private":"{private}"}}"#),
            r#"[{"role":"assistant","messageId":"m","model":"x","timestamp":1,"usage":{"inputTokens":1}}]"#,
        ))
        .unwrap();
        assert_eq!(p.malformed_ledger_count, 1);
        assert!(!p.used_ledger);
        assert_eq!(p.events.len(), 1);
        assert!(!format!("{p:?}").contains(private));
    }
    #[test]
    fn ledger_joins_cache_suppresses_and_canonicalizes_ids() {
        let p=parse(&wrap(r#", "usageLedger":{"events":[{"id":9,"timestamp":"1767225603000","model":"claude-x","tokens":{"input":12,"output":4,"total":16},"toMessageId":"7"}]}"#,r#"[{"role":"assistant","messageId":7,"usage":{"cacheCreationInputTokens":1,"cacheReadInputTokens":2}},{"role":"assistant","messageId":8,"model":"claude-x","timestamp":1767225602000,"usage":{"inputTokens":10}}]"#)).unwrap();
        assert!(p.used_ledger);
        assert_eq!(p.events.len(), 1);
        assert_eq!(
            p.events[0].source_event_id.as_deref(),
            Some("amp:T-test-1:ledger:9")
        );
        assert_eq!(
            (
                p.events[0].usage.cache_write_tokens,
                p.events[0].usage.cache_read_tokens
            ),
            (1, 2)
        );
        assert_eq!(
            p.superseded_source_event_ids,
            vec!["amp:T-test-1:message:8"]
        );
    }
    #[test]
    fn mismatch_and_aggregate_confidence() {
        let p=parse(&wrap("",r#"[{"role":"assistant","model":"x","timestamp":1,"usage":{"inputTokens":2,"cacheReadInputTokens":3,"totalInputTokens":99}},{"role":"assistant","model":"x","timestamp":2,"usage":{"totalTokens":8}}]"#)).unwrap();
        assert_eq!(p.split_total_mismatch_count, 1);
        assert_eq!(p.aggregate_only_count, 1);
        assert_eq!(p.events[0].usage.input_tokens, 2);
        assert_eq!(
            p.events[0].operation.metadata,
            Some(serde_json::json!({"identity_fallback": true}))
        );
        assert_eq!(
            p.events[1].operation.metadata,
            Some(serde_json::json!({
                "aggregate_fallback": true,
                "identity_fallback": true
            }))
        );
        assert!(p.events[1].confidence < p.events[0].confidence);
    }
    #[test]
    fn aggregate_fallback_with_explicit_identity_is_bounded_metadata() {
        let p = parse(&wrap(
            "",
            r#"[{"role":"assistant","messageId":"m","model":"x","timestamp":1,"usage":{"totalInputTokens":8}}]"#,
        ))
        .unwrap();
        assert_eq!(
            p.events[0].operation.metadata,
            Some(serde_json::json!({"aggregate_fallback": true}))
        );
        assert_eq!(p.events[0].confidence, 0.6);
    }
    #[test]
    fn skips_zero_negative_and_non_assistant() {
        let p=parse(&wrap("",r#"[{"role":"assistant","model":"x","timestamp":1,"usage":{"inputTokens":0}},{"role":"assistant","model":"x","timestamp":1,"usage":{"inputTokens":-1}},{"role":"user","model":"x","timestamp":1,"usage":{"inputTokens":1}}]"#)).unwrap();
        assert!(p.events.is_empty());
    }
    #[test]
    fn missing_id_fallback_is_deterministic() {
        let json = wrap(
            "",
            r#"[{"role":"assistant","model":"x","timestamp":1,"usage":{"inputTokens":1}}]"#,
        );
        let fallback = parse(&json).unwrap();
        let explicit = parse(&wrap("", r#"[{"role":"assistant","messageId":"m","model":"x","timestamp":1,"usage":{"inputTokens":1}}]"#)).unwrap();
        assert_eq!(
            fallback.events[0].source_event_id,
            parse(&json).unwrap().events[0].source_event_id
        );
        assert_eq!(
            fallback.events[0].operation.metadata,
            Some(serde_json::json!({"identity_fallback": true}))
        );
        assert!(fallback.events[0].confidence < explicit.events[0].confidence);
        assert_eq!(explicit.events[0].operation.metadata, None);
    }
    #[test]
    fn numeric_and_string_top_level_message_ids_are_identical() {
        let numeric = parse(&wrap("", r#"[{"role":"assistant","messageId":7,"model":"x","timestamp":1,"usage":{"inputTokens":1}}]"#)).unwrap();
        let string = parse(&wrap("", r#"[{"role":"assistant","messageId":"7","model":"x","timestamp":1,"usage":{"inputTokens":1}}]"#)).unwrap();
        assert_eq!(
            numeric.events[0].source_event_id,
            string.events[0].source_event_id
        );
        assert_eq!(
            numeric.events[0].source_event_id.as_deref(),
            Some("amp:T-test-1:message:7")
        );
    }
    #[test]
    fn missing_ledger_id_fallback_is_deterministic() {
        let json = wrap(
            r#", "usageLedger":{"events":[{"timestamp":2,"model":"x","tokens":{"input":1}}]}"#,
            "[]",
        );
        let fallback = parse(&json).unwrap();
        let explicit = parse(&wrap(r#", "usageLedger":{"events":[{"id":"l","timestamp":2,"model":"x","tokens":{"input":1}}]}"#, "[]")).unwrap();
        assert_eq!(
            fallback.events[0].source_event_id,
            parse(&json).unwrap().events[0].source_event_id
        );
        assert_eq!(
            fallback.events[0].operation.metadata,
            Some(serde_json::json!({"identity_fallback": true}))
        );
        assert!(fallback.events[0].confidence < explicit.events[0].confidence);
        assert_eq!(explicit.events[0].operation.metadata, None);
    }
    #[test]
    fn ledger_skips_negative_and_all_zero_records() {
        let p = parse(&wrap(r#", "usageLedger":{"events":[{"id":"negative","timestamp":1,"model":"x","tokens":{"input":-1}},{"id":"zero","timestamp":1,"model":"x","tokens":{"input":0,"output":0,"total":0}},{"id":"usable","timestamp":1,"model":"x","tokens":{"input":2}}]}"#, "[]")).unwrap();
        assert_eq!(p.events.len(), 1);
        assert_eq!(
            p.events[0].source_event_id.as_deref(),
            Some("amp:T-test-1:ledger:usable")
        );
    }
    #[test]
    fn ledger_rejects_total_less_than_output() {
        let p = parse(&wrap(
            r#", "usageLedger":{"events":[{"id":"bad","timestamp":1,"model":"x","tokens":{"output":5,"total":4}}]}"#,
            "[]",
        ))
        .unwrap();
        assert!(p.events.is_empty());
        assert!(!p.used_ledger);
    }
    #[test]
    fn ledger_trusts_explicit_split_and_diagnoses_mismatched_total() {
        let p = parse(&wrap(
            r#", "usageLedger":{"events":[{"id":"split","timestamp":1,"model":"x","tokens":{"input":2,"output":3,"total":99}}]}"#,
            "[]",
        ))
        .unwrap();
        assert_eq!(p.events[0].usage.input_tokens, 2);
        assert_eq!(p.events[0].usage.output_tokens, 3);
        assert_eq!(p.ledger_total_mismatch_count, 1);
    }
    #[test]
    fn ledger_consistent_total_has_no_mismatch() {
        let p = parse(&wrap(
            r#", "usageLedger":{"events":[{"id":"split","timestamp":1,"model":"x","tokens":{"input":2,"output":3,"total":5}}]}"#,
            "[]",
        ))
        .unwrap();
        assert_eq!(p.events.len(), 1);
        assert_eq!(p.ledger_total_mismatch_count, 0);
    }
    #[test]
    fn ledger_split_addition_overflow_rejects_without_panicking() {
        let ledger = format!(
            r#", "usageLedger":{{"events":[{{"id":"overflow","timestamp":1,"model":"x","tokens":{{"input":{},"output":1,"total":{}}}}}]}}"#,
            i64::MAX,
            i64::MAX
        );
        let p = parse(&wrap(&ledger, "[]")).unwrap();
        assert!(p.events.is_empty());
        assert_eq!(p.ledger_total_mismatch_count, 0);
    }
    #[test]
    fn malformed_ledger_element_does_not_hide_usable_events() {
        let private = "PRIVATE_ELEMENT_SENTINEL";
        let p = parse(&wrap(&format!(r#", "usageLedger":{{"events":[{{"timestamp":{{"private":"{private}"}},"tokens":{{"input":"bad"}}}}, {{"id":"usable","timestamp":1,"model":"x","tokens":{{"input":2}}}}]}}"#), "[]")).unwrap();
        assert!(p.used_ledger);
        assert_eq!(p.malformed_ledger_count, 1);
        assert_eq!(p.events.len(), 1);
        assert_eq!(
            p.events[0].source_event_id.as_deref(),
            Some("amp:T-test-1:ledger:usable")
        );
        assert!(!format!("{p:?}").contains(private));
    }
    #[test]
    fn rejects_invalid_thread_id_without_leaking_nested_private_values() {
        assert!(parse(r#"{"id":"bad/private","messages":[]}"#).is_err());
        assert!(parse(r#"{"id":"T-","messages":[]}"#).is_err());
        let private = "PRIVATE_SENTINEL";
        let json = format!(
            r#"{{"id":"T-safe","updatedAt":{{"x":"{private}"}},"messages":[{{"role":"assistant","messageId":{{"x":"{private}"}},"model":"x","timestamp":1,"usage":{{"inputTokens":1}}}}],"usageLedger":{{"events":[{{"id":1,"timestamp":2,"model":"x","tokens":{{"input":1}},"toMessageId":{{"x":"{private}"}}}}]}}}}"#
        );
        let result = parse(&json);
        assert!(!format!("{result:?}").contains(private));
        assert_eq!(result.unwrap().events.len(), 1);
    }

    #[test]
    fn numeric_timestamp_milliseconds_are_checked() {
        let max_millis = i64::MAX / 1_000_000;
        assert_eq!(
            timestamp_ns(&max_millis.to_string()),
            max_millis.checked_mul(1_000_000)
        );
        assert_eq!(timestamp_ns(&(max_millis + 1).to_string()), None);
        assert_eq!(timestamp_ns("18446744073709551615"), None);
    }

    #[test]
    fn rfc3339_timestamp_accepts_exact_valid_values() {
        assert_eq!(timestamp_ns("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            timestamp_ns("2000-02-29T23:59:59.1Z"),
            Some(951_868_799_100_000_000)
        );
        assert_eq!(
            timestamp_ns("1970-01-01T00:00:00.123456789Z"),
            Some(123_456_789)
        );
    }

    #[test]
    fn rfc3339_timestamp_rejects_invalid_values() {
        for value in [
            "2023-02-29T00:00:00Z",
            "2024-02-30T00:00:00Z",
            "2024-04-31T00:00:00Z",
            "2024-00-01T00:00:00Z",
            "2024-01-01T24:00:00Z",
            "2024-01-01T00:60:00Z",
            "2024-01-01T00:00:60Z",
            "2024-01-01T00:00:00.Z",
            "2024-01-01T00:00:00.1234567890Z",
            "2024-01-01T00:00:00.aZ",
            "2024-01-01T00:00:00Zextra",
            "2024-01-01T00:00:00:00Z",
            "10000-01-01T00:00:00Z",
            "0000-01-01T00:00:00Z",
        ] {
            assert_eq!(timestamp_ns(value), None, "accepted {value}");
        }
    }

    #[test]
    fn split_token_sum_overflow_rejects_record_without_panicking() {
        let messages = format!(
            r#"[{{"role":"assistant","model":"x","timestamp":1,"usage":{{"inputTokens":{},"cacheReadInputTokens":1,"totalInputTokens":{}}}}}]"#,
            i64::MAX,
            i64::MAX
        );
        let p = parse(&wrap("", &messages)).unwrap();
        assert!(p.events.is_empty());
    }

    #[test]
    fn blank_explicit_ids_use_distinct_deterministic_fallbacks() {
        let messages = r#"[{"role":"assistant","messageId":"","model":"x","timestamp":1,"usage":{"inputTokens":1}},{"role":"assistant","messageId":"  ","model":"x","timestamp":1,"usage":{"inputTokens":1}}]"#;
        let first = parse(&wrap("", messages)).unwrap();
        let second = parse(&wrap("", messages)).unwrap();
        assert_eq!(
            first.events[0].source_event_id,
            second.events[0].source_event_id
        );
        assert_ne!(
            first.events[0].source_event_id,
            first.events[1].source_event_id
        );
        assert!(first.events.iter().all(|event| event.operation.metadata
            == Some(serde_json::json!({"identity_fallback": true}))));
        assert!(
            first
                .events
                .iter()
                .all(|event| (event.confidence - 0.72).abs() < f64::EPSILON)
        );

        let ledger = r#", "usageLedger":{"events":[{"id":" ","timestamp":1,"model":"x","tokens":{"input":1}},{"id":"","timestamp":1,"model":"x","tokens":{"input":1}}]}"#;
        let parsed = parse(&wrap(ledger, "[]")).unwrap();
        assert_ne!(
            parsed.events[0].source_event_id,
            parsed.events[1].source_event_id
        );
        assert!(parsed.events.iter().all(|event| event.operation.metadata
            == Some(serde_json::json!({"identity_fallback": true}))));
    }

    #[test]
    fn local_import_scans_multiple_roots_and_is_idempotent() -> Result<()> {
        let root = temp_dir("amp-local-multiple");
        let first = root.join("one");
        let second = root.join("two");
        fs::create_dir_all(&first)?;
        fs::create_dir_all(&second)?;
        fs::write(
            first.join("a.json"),
            message_thread("T-one", "m1", 10, 5, 3, 2),
        )?;
        fs::write(
            second.join("b.json"),
            message_thread("T-two", "m2", 20, 7, 4, 1),
        )?;
        let db = test_db(&root)?;
        let identity = ImportIdentity::new("profile", "device");

        let report = import_local_recent_with_identity(&db, &[first, second], 0, &identity)?;
        assert_eq!(
            (
                report.files_seen,
                report.files_imported,
                report.events_projected
            ),
            (2, 2, 2)
        );
        assert_eq!(count(&db, "sessions")?, 2);
        assert_eq!(count(&db, "llm_calls")?, 2);
        let totals: (i64, i64, i64, i64) = db.connection().query_row(
            "SELECT SUM(input_tokens), SUM(output_tokens), SUM(cache_write_tokens), SUM(cache_read_tokens) FROM llm_calls",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        assert_eq!(totals, (30, 12, 7, 3));

        let repeated = import_local_recent_with_identity(&db, &[root], 0, &identity)?;
        assert_eq!(
            (
                repeated.files_imported,
                repeated.files_skipped,
                repeated.events_projected
            ),
            (0, 2, 0)
        );
        assert_eq!(count(&db, "llm_calls")?, 2);
        Ok(())
    }

    #[test]
    fn local_scan_includes_an_ordinary_json_file_root() -> Result<()> {
        let root = temp_dir("amp-local-ordinary-file");
        fs::create_dir_all(&root)?;
        let path = root.join("thread.json");
        fs::write(&path, "{}")?;

        let files = recent_json_files(std::slice::from_ref(&path), i64::MIN)?;

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, path);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn local_scan_excludes_a_direct_json_file_symlink_root() -> Result<()> {
        use std::os::unix::fs::symlink;

        let root = temp_dir("amp-local-file-symlink");
        fs::create_dir_all(&root)?;
        let target = root.join("target.json");
        let link = root.join("thread.json");
        fs::write(&target, "{}")?;
        symlink(&target, &link)?;

        let files = recent_json_files(&[link], i64::MIN)?;

        assert!(files.is_empty());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn local_scan_excludes_json_file_symlinks_inside_a_directory() -> Result<()> {
        use std::os::unix::fs::symlink;

        let root = temp_dir("amp-local-contained-symlink");
        fs::create_dir_all(&root)?;
        let target = root.join("target.txt");
        let link = root.join("thread.json");
        fs::write(&target, "{}")?;
        symlink(&target, &link)?;

        let files = recent_json_files(std::slice::from_ref(&root), i64::MIN)?;

        assert!(files.is_empty());
        Ok(())
    }

    #[test]
    fn finalized_message_updates_all_mutable_usage_fields() -> Result<()> {
        let root = temp_dir("amp-local-update");
        fs::create_dir_all(&root)?;
        let path = root.join("thread.json");
        fs::write(&path, message_thread("T-one", "m1", 10, 5, 3, 2))?;
        let db = test_db(&root)?;
        let identity = ImportIdentity::new("profile", "device");
        import_local_with_identity(&db, path.clone(), &identity)?;

        fs::write(
            &path,
            r#"{"id":"T-one","updatedAt":"2026-01-02T00:00:00Z","messages":[{"role":"assistant","messageId":"m1","model":"ignored-padding","usage":{"model":"gpt-5","timestamp":1767312000000,"inputTokens":100,"outputTokens":50,"cacheCreationInputTokens":30,"cacheReadInputTokens":20,"maxInputTokens":400000}}]}"#,
        )?;
        import_local_with_identity(&db, path, &identity)?;

        let values: (i64, i64, i64, i64, String, i64, i64) = db.connection().query_row(
            "SELECT input_tokens, output_tokens, cache_write_tokens, cache_read_tokens, model, started_at_ns, model_context_window FROM llm_calls",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)),
        )?;
        assert_eq!(
            values,
            (
                100,
                50,
                30,
                20,
                "gpt-5".into(),
                1_767_312_000_000_000_000,
                400000
            )
        );
        assert_eq!(count(&db, "llm_calls")?, 1);
        Ok(())
    }

    #[test]
    fn usable_ledger_retires_superseded_message_but_parse_failure_does_not() -> Result<()> {
        let root = temp_dir("amp-local-reconcile");
        fs::create_dir_all(&root)?;
        let path = root.join("thread.json");
        fs::write(&path, message_thread("T-one", "m1", 10, 5, 3, 2))?;
        let db = test_db(&root)?;
        let identity = ImportIdentity::new("profile", "device");
        import_local_with_identity(&db, path.clone(), &identity)?;

        fs::write(&path, "{ malformed and a different size")?;
        let failed = import_local_with_identity(&db, path.clone(), &identity)?;
        assert_eq!(failed.events_projected, 0);
        assert_eq!(count(&db, "llm_calls")?, 1);

        fs::write(
            &path,
            r#"{"id":"T-one","updatedAt":"2026-01-02T00:00:00Z","messages":[{"role":"assistant","messageId":"m1","model":"claude-old","usage":{"timestamp":1767225600000,"inputTokens":10,"outputTokens":5,"cacheCreationInputTokens":3,"cacheReadInputTokens":2}}],"usageLedger":{"events":[{"id":"ledger-1","toMessageId":"m1","timestamp":1767312000000,"model":"claude-new","tokens":{"input":40,"output":9,"total":49}}]}}"#,
        )?;
        import_local_with_identity(&db, path, &identity)?;
        assert_eq!(count(&db, "llm_calls")?, 1);
        assert_eq!(count(&db, "run_steps")?, 1);
        let values: (String, i64, i64, i64, i64) = db.connection().query_row(
            "SELECT model, input_tokens, output_tokens, cache_write_tokens, cache_read_tokens FROM llm_calls",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )?;
        assert_eq!(values, ("claude-new".into(), 40, 9, 3, 2));
        Ok(())
    }

    #[test]
    fn missing_root_and_private_collector_are_safe() -> Result<()> {
        let root = temp_dir("amp-local-collector");
        let missing = root.join("missing");
        let db = test_db(&root)?;
        let identity = ImportIdentity::new("profile", "device");
        let empty = import_local_recent_with_identity(&db, &[missing], 0, &identity)?;
        assert_eq!((empty.files_seen, empty.events_projected), (0, 0));

        fs::create_dir_all(&root)?;
        fs::write(
            root.join("thread.json"),
            message_thread("T-private", "m-private", 10, 5, 3, 2),
        )?;
        let mut output = Vec::new();
        let report = collect_local_recent(&[root], &identity, &mut output, 0)?;
        assert_eq!(report.events_projected, 1);
        let json = String::from_utf8(output)?;
        assert!(json.contains("input_tokens"));
        assert!(!json.contains("PRIVATE_SENTINEL"));
        Ok(())
    }

    fn message_thread(
        thread: &str,
        message: &str,
        input: i64,
        output: i64,
        write: i64,
        read: i64,
    ) -> String {
        format!(
            r#"{{"id":"{thread}","updatedAt":"2026-01-01T00:00:00Z","messages":[{{"role":"assistant","messageId":"{message}","model":"claude-test","content":"PRIVATE_SENTINEL","usage":{{"timestamp":1767225600000,"inputTokens":{input},"outputTokens":{output},"cacheCreationInputTokens":{write},"cacheReadInputTokens":{read},"maxInputTokens":200000}}}}]}}"#
        )
    }

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "shirabe-{name}-{}-{:?}",
            std::process::id(),
            SystemTime::now()
        ))
    }

    fn test_db(root: &std::path::Path) -> Result<Database> {
        fs::create_dir_all(root)?;
        let db = Database::open(&root.join("catalog.sqlite"))?;
        db.migrate()?;
        Ok(db)
    }

    fn count(db: &Database, table: &str) -> Result<i64> {
        Ok(db
            .connection()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })?)
    }
}
