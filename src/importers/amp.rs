use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, Metadata, OpenOptions},
    io::{BufReader, Cursor, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::{
    db::{AmpVirtualImportState, Database, now_ns, stable_hash, system_time_ns},
    projection::event::{
        EntityHint, NormalizedEvent, Operation, OperationStatus, OperationType, TraceContext, Usage,
    },
    projection::projector::{ProjectionCache, Projector},
};

use super::{ImportIdentity, ImportReport, ImportTiming, write_collected_event};

const SOURCE_KIND: &str = "amp_local_thread_json";
const MAX_THREAD_JSON_BYTES: u64 = 64 * 1024 * 1024;
const MAX_LIST_BYTES: usize = 8 * 1024 * 1024;
const RECENT_NS: i64 = 15 * 60 * 1_000_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CliThread {
    pub id: String,
    pub message_count: usize,
}
struct CliPage {
    threads: Vec<CliThread>,
    candidate_rows: usize,
}

fn parse_cli_thread_page(bytes: &[u8]) -> Result<CliPage> {
    let text = std::str::from_utf8(bytes).context("Amp list output is not UTF-8")?;
    let lines: Vec<_> = text.lines().collect();
    let header = lines.iter().position(|line| {
        line.split_whitespace().collect::<Vec<_>>()
            == [
                "Title",
                "Last",
                "Updated",
                "Visibility",
                "Messages",
                "Thread",
                "ID",
            ]
    });
    let Some(header) = header else {
        if lines
            .iter()
            .all(|line| line.trim().is_empty() || is_terminal_control_line(line))
        {
            return Ok(CliPage {
                threads: Vec::new(),
                candidate_rows: 0,
            });
        }
        bail!("unrecognized Amp thread listing")
    };
    let separator = lines
        .get(header + 1)
        .filter(|line| is_table_separator(line));
    if separator.is_none() {
        bail!("unrecognized Amp thread listing")
    }
    let mut threads = Vec::new();
    for line in &lines[header + 2..] {
        if line.trim().is_empty() || is_terminal_control_line(line) {
            continue;
        }
        let words: Vec<_> = line.split_whitespace().collect();
        let id = words.last().filter(|id| valid_thread_id(id));
        let count = words.iter().rev().nth(1).and_then(|word| word.parse().ok());
        let (Some(id), Some(message_count)) = (id, count) else {
            bail!("malformed Amp thread listing row")
        };
        threads.push(CliThread {
            id: (*id).into(),
            message_count,
        });
    }
    Ok(CliPage {
        candidate_rows: threads.len(),
        threads,
    })
}

fn is_table_separator(line: &str) -> bool {
    let words: Vec<_> = line.split_whitespace().collect();
    words.len() >= 5
        && words
            .iter()
            .all(|word| word.chars().all(|c| matches!(c, '-' | '─')))
}

fn is_terminal_control_line(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty()
        && trimmed.chars().all(|c| {
            c.is_control()
                || matches!(c, '\u{1b}' | '[' | ']' | '?' | ';' | '0'..='9' | 'A'..='Z' | 'a'..='z')
        })
        && (trimmed.contains('\u{1b}') || trimmed == "[K")
}

pub(crate) trait AmpCommandRunner {
    fn list(&self, offset: usize) -> Result<Vec<u8>>;
    fn export(&self, thread_id: &str, identity: &ImportIdentity) -> Result<ParsedExport>;
}

#[derive(Debug)]
pub(crate) struct ParsedExport {
    parsed: ParsedThread,
    fingerprint: String,
    byte_count: u64,
}

struct CappedHashingReader<R> {
    inner: R,
    cap: u64,
    count: u64,
    hasher: Sha256,
    exceeded: Arc<AtomicBool>,
}

impl<R: Read> Read for CappedHashingReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.count == self.cap {
            let mut probe = [0];
            if self.inner.read(&mut probe)? != 0 {
                self.exceeded.store(true, Ordering::Relaxed);
                return Err(std::io::Error::other("Amp export output limit exceeded"));
            }
            return Ok(0);
        }
        let remaining = (self.cap - self.count).min(buffer.len() as u64) as usize;
        let read = self.inner.read(&mut buffer[..remaining])?;
        self.hasher.update(&buffer[..read]);
        self.count += read as u64;
        Ok(read)
    }
}

fn parse_export_reader<R: Read>(
    reader: R,
    cap: u64,
    identity: &ImportIdentity,
) -> Result<ParsedExport> {
    let exceeded = Arc::new(AtomicBool::new(false));
    let mut reader = CappedHashingReader {
        inner: reader,
        cap,
        count: 0,
        hasher: Sha256::new(),
        exceeded: exceeded.clone(),
    };
    let parsed = parse_thread_reader(&mut reader, "amp_cli_thread_export", identity);
    // A schema error may stop serde before the cap. Continue consuming without
    // retaining bytes so cap+1 always wins over malformed-content reporting.
    let _ = std::io::copy(&mut reader, &mut std::io::sink());
    if exceeded.load(Ordering::Relaxed) {
        bail!("Amp export output limit exceeded")
    }
    let parsed = parsed.context("invalid Amp export")?;
    Ok(ParsedExport {
        parsed,
        fingerprint: hex::encode(reader.hasher.finalize()),
        byte_count: reader.count,
    })
}

struct ProcessAmpRunner {
    executable: PathBuf,
    deadline: Duration,
    list_cap: usize,
    export_cap: usize,
}
impl ProcessAmpRunner {
    fn spawn(&self, kind: &'static str, args: &[&str]) -> Result<std::process::Child> {
        Command::new(&self.executable)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("start Amp {kind} command"))
    }

    fn run_list(&self, kind: &'static str, args: &[&str], cap: usize) -> Result<Vec<u8>> {
        let mut child = self.spawn(kind, args)?;
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut sink = std::io::sink();
            let mut input = stderr;
            let _ = std::io::copy(&mut input, &mut sink);
        });
        thread::spawn(move || {
            let mut input = stdout;
            let mut output = Vec::new();
            let result = (&mut input)
                .take((cap + 1) as u64)
                .read_to_end(&mut output)
                .map(|_| output);
            let _ = tx.send(result);
            let _ = std::io::copy(&mut input, &mut std::io::sink());
        });
        let started = Instant::now();
        let mut captured = None;
        loop {
            if captured.is_none() {
                if let Ok(result) = rx.try_recv() {
                    let output = result?;
                    if output.len() > cap {
                        let _ = child.kill();
                        let _ = child.wait();
                        bail!("Amp {kind} output limit exceeded")
                    }
                    captured = Some(output);
                }
            }
            if started.elapsed() >= self.deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!("Amp {kind} command timed out")
            }
            if let Some(status) = child.try_wait()? {
                let output = match captured {
                    Some(output) => output,
                    None => rx
                        .recv_timeout(Duration::from_secs(1))
                        .map_err(|_| anyhow::anyhow!("Amp {kind} output unavailable"))??,
                };
                if output.len() > cap {
                    bail!("Amp {kind} output limit exceeded")
                }
                if !status.success() {
                    bail!(
                        "Amp {kind} command exited with status {}",
                        status
                            .code()
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "signal".into())
                    )
                }
                return Ok(output);
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}
impl AmpCommandRunner for ProcessAmpRunner {
    fn list(&self, offset: usize) -> Result<Vec<u8>> {
        self.run_list(
            "list",
            &[
                "threads",
                "list",
                "--include-archived",
                "--limit",
                "100",
                "--offset",
                &offset.to_string(),
            ],
            self.list_cap,
        )
    }
    fn export(&self, id: &str, identity: &ImportIdentity) -> Result<ParsedExport> {
        if !valid_thread_id(id) {
            bail!("invalid Amp thread id")
        }
        let mut child = self.spawn("export", &["threads", "export", id])?;
        let stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        thread::spawn(move || {
            let _ = std::io::copy(&mut stderr, &mut std::io::sink());
        });
        let (tx, rx) = mpsc::channel();
        let cap = self.export_cap as u64;
        let identity = identity.clone();
        thread::spawn(move || {
            let _ = tx.send(parse_export_reader(stdout, cap, &identity));
        });
        let started = Instant::now();
        let mut parsed = None;
        loop {
            if parsed.is_none()
                && let Ok(result) = rx.try_recv()
            {
                parsed = Some(result);
            }
            if started.elapsed() >= self.deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!("Amp export command timed out for {id}")
            }
            if let Some(status) = child.try_wait()? {
                let result = match parsed {
                    Some(result) => result,
                    None => rx
                        .recv_timeout(Duration::from_secs(1))
                        .map_err(|_| anyhow::anyhow!("Amp export output unavailable for {id}"))?,
                };
                if !status.success() {
                    bail!("Amp export command failed for {id}")
                }
                return result.map_err(|error| {
                    if error.to_string().contains("limit exceeded") {
                        anyhow::anyhow!("Amp export output limit exceeded for {id}")
                    } else {
                        anyhow::anyhow!("invalid Amp export for {id}")
                    }
                });
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

pub fn sync_cli_first_with_identity(
    db: &Database,
    fallback_roots: &[PathBuf],
    identity: &ImportIdentity,
) -> Result<ImportReport> {
    sync_cli_first_recent_with_identity(db, fallback_roots, i64::MIN, identity)
}

pub fn sync_cli_first_recent_with_identity(
    db: &Database,
    fallback_roots: &[PathBuf],
    modified_since_ns: i64,
    identity: &ImportIdentity,
) -> Result<ImportReport> {
    let runner = resolve_amp_cli().map(|executable| ProcessAmpRunner {
        executable,
        deadline: Duration::from_secs(30),
        list_cap: MAX_LIST_BYTES,
        export_cap: MAX_THREAD_JSON_BYTES as usize,
    });
    sync_cli_or_local_with_runner(
        db,
        runner
            .as_ref()
            .map(|runner| runner as &dyn AmpCommandRunner),
        fallback_roots,
        modified_since_ns,
        identity,
    )
}

fn sync_cli_or_local_with_runner(
    db: &Database,
    runner: Option<&dyn AmpCommandRunner>,
    fallback_roots: &[PathBuf],
    modified_since_ns: i64,
    identity: &ImportIdentity,
) -> Result<ImportReport> {
    let Some(runner) = runner else {
        return import_local_recent_with_identity(db, fallback_roots, modified_since_ns, identity);
    };
    let started = Instant::now();
    let threads = match discover_cli_threads(runner) {
        Ok(threads) => threads,
        Err(_) => {
            return import_local_recent_with_identity(
                db,
                fallback_roots,
                modified_since_ns,
                identity,
            );
        }
    };
    sync_discovered_threads(db, runner, identity, threads, started)
}

fn resolve_amp_cli() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("AMP_CLI").filter(|v| !v.is_empty()) {
        let path = PathBuf::from(value);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&paths) {
            let path = directory.join("amp");
            if path.is_file() {
                return Some(path);
            }
        }
    }
    crate::config::home_dir()
        .map(|home| home.join(".amp/bin/amp"))
        .filter(|path| path.is_file())
}

fn sync_with_runner(
    db: &Database,
    runner: &dyn AmpCommandRunner,
    identity: &ImportIdentity,
) -> Result<ImportReport> {
    let started = Instant::now();
    let threads = discover_cli_threads(runner)?;
    sync_discovered_threads(db, runner, identity, threads, started)
}

fn discover_cli_threads(runner: &dyn AmpCommandRunner) -> Result<Vec<CliThread>> {
    let mut offset = 0usize;
    let mut seen_pages = HashSet::new();
    let mut threads = Vec::new();
    loop {
        let bytes = runner.list(offset)?;
        let signature = stable_hash(&hex::encode(Sha256::digest(&bytes)));
        if !seen_pages.insert(signature) {
            bail!("Amp list pagination did not advance")
        }
        let page = parse_cli_thread_page(&bytes)?;
        if page.candidate_rows == 0 {
            break;
        }
        let next = offset
            .checked_add(page.candidate_rows)
            .context("Amp list offset overflow")?;
        if next <= offset {
            bail!("Amp list pagination did not advance")
        }
        threads.extend(page.threads);
        offset = next;
        if offset > 1_000_000 {
            bail!("Amp list pagination limit exceeded")
        }
        if offset % 100 != 0 || page.candidate_rows < 100 {
            break;
        }
    }
    Ok(threads)
}

fn sync_discovered_threads(
    db: &Database,
    runner: &dyn AmpCommandRunner,
    identity: &ImportIdentity,
    threads: Vec<CliThread>,
    started: Instant,
) -> Result<ImportReport> {
    let source_id = db.record_import_source_for_profile(
        "amp",
        "amp_cli_thread_export",
        Path::new("<amp-cli>"),
        &identity.profile_id,
        &identity.device_id,
    )?;
    let mut stats = ScanStats::default();
    for thread in threads {
        stats.files_seen += 1;
        let prior = db.amp_virtual_state(&source_id, &thread.id)?;
        let should_export = prior.as_ref().is_none_or(|state| {
            state.message_count != thread.message_count
                || !matches!(state.status.as_str(), "imported" | "partial")
                || state.updated_at_ns >= now_ns().saturating_sub(RECENT_NS)
        });
        if !should_export {
            stats.files_skipped += 1;
            continue;
        }
        let preserved = prior.unwrap_or_default();
        let file_id = db.prepare_amp_virtual(&source_id, &thread.id, thread.message_count)?;
        let exported = match runner.export(&thread.id, identity) {
            Ok(exported) => exported,
            Err(_) => {
                db.fail_amp_virtual(
                    &file_id,
                    &preserved,
                    &format!("Amp export command failed for {}", thread.id),
                )?;
                stats.warnings += 1;
                continue;
            }
        };
        stats.bytes = stats.bytes.saturating_add(exported.byte_count);
        let digest = exported.fingerprint;
        let parsed = exported.parsed;
        let event_count = parsed.events.len();
        let updated = parsed.updated_at_ns;
        let warnings = parsed_warning_count(&parsed);
        let tx = db.begin_batch()?;
        project_parsed_thread(db, &parsed, identity)?;
        let status = if warnings == 0 { "imported" } else { "partial" };
        let state = AmpVirtualImportState {
            status: status.into(),
            message_count: thread.message_count,
            updated_at_ns: updated,
            payload_fingerprint: Some(digest),
            event_count,
        };
        db.finish_amp_virtual(&file_id, status, &state)?;
        tx.commit()?;
        stats.files_imported += 1;
        stats.events += event_count;
        stats.warnings += warnings;
    }
    let written = fs::metadata(db.path()).map(|m| m.len()).unwrap_or_default();
    Ok(report(
        PathBuf::from("<amp-cli>"),
        source_id,
        stats,
        written,
        started.elapsed().as_millis(),
        started,
    ))
}

#[derive(Debug)]
struct ScannedFile {
    path: PathBuf,
    size: u64,
    modified_ns: i64,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

#[derive(Debug, PartialEq, Eq)]
struct DescriptorSnapshot {
    size: u64,
    modified_ns: i64,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

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
    for scanned in files {
        stats.files_seen += 1;
        stats.bytes = stats.bytes.saturating_add(scanned.size);
        let prepared =
            db.prepare_import_file(&source_id, &scanned.path, scanned.size, scanned.modified_ns)?;
        if !prepared.should_import {
            stats.files_skipped += 1;
            continue;
        }
        stats.files_imported += 1;
        import_file(db, &prepared.file_id, &scanned, identity, &mut stats)?;
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
    for scanned in recent_json_files(roots, modified_since_ns)? {
        stats.files_seen += 1;
        stats.files_imported += 1;
        stats.bytes = stats.bytes.saturating_add(scanned.size);
        let bytes = match read_scanned_file(&scanned, MAX_THREAD_JSON_BYTES) {
            Ok(bytes) => bytes,
            Err(_) => {
                stats.warnings += 1;
                continue;
            }
        };
        match parse_thread_reader(Cursor::new(bytes), SOURCE_KIND, identity) {
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
    scanned: &ScannedFile,
    identity: &ImportIdentity,
    stats: &mut ScanStats,
) -> Result<()> {
    import_file_with_limit(db, file_id, scanned, identity, stats, MAX_THREAD_JSON_BYTES)
}

fn import_file_with_limit(
    db: &Database,
    file_id: &str,
    scanned: &ScannedFile,
    identity: &ImportIdentity,
    stats: &mut ScanStats,
    limit: u64,
) -> Result<()> {
    let file_size = Some(scanned.size);
    let bytes = match read_scanned_file(scanned, limit) {
        Ok(bytes) => bytes,
        Err(_) => {
            stats.warnings += 1;
            let oversized = scanned.size > limit;
            db.finish_import_file(
                file_id,
                "failed",
                0,
                1,
                None,
                None,
                None,
                Some(if oversized {
                    "Amp JSON file exceeds size limit"
                } else {
                    "Amp JSON file could not be read"
                }),
            )?;
            return Ok(());
        }
    };
    let parsed = match parse_thread_reader(Cursor::new(bytes), SOURCE_KIND, identity) {
        Ok(parsed) => parsed,
        Err(_) => {
            stats.warnings += 1;
            db.finish_import_file(
                file_id,
                "failed",
                0,
                1,
                file_size,
                None,
                None,
                Some("Amp JSON schema could not be parsed"),
            )?;
            return Ok(());
        }
    };
    stats.add_diagnostics(&parsed);
    project_parsed_thread(db, &parsed, identity)?;
    let first = parsed.events.iter().map(|e| e.occurred_at_ns).min();
    let last = parsed.events.iter().map(|e| e.occurred_at_ns).max();
    let event_count = parsed.events.len();
    let file_warnings = parsed_warning_count(&parsed);
    db.finish_import_file(
        file_id,
        if file_warnings == 0 {
            "imported"
        } else {
            "partial"
        },
        event_count,
        file_warnings,
        file_size,
        first,
        last,
        None,
    )?;
    stats.events += event_count;
    Ok(())
}

fn parsed_warning_count(parsed: &ParsedThread) -> usize {
    parsed.split_total_mismatch_count
        + parsed.aggregate_only_count
        + parsed.malformed_ledger_count
        + parsed.ledger_total_mismatch_count
}

fn project_parsed_thread(
    db: &Database,
    parsed: &ParsedThread,
    identity: &ImportIdentity,
) -> Result<()> {
    let projector = Projector::new(db);
    let mut cache = ProjectionCache::default();
    let mut run_ids = HashSet::new();
    let mut session_ids = HashSet::new();
    let mut retire_ids: HashSet<String> = parsed
        .events
        .iter()
        .filter_map(|event| event.source_event_id.clone())
        .collect();
    if parsed.used_ledger && parsed.malformed_ledger_count == 0 {
        retire_ids.extend(parsed.superseded_source_event_ids.iter().cloned());
    }
    if !retire_ids.is_empty() {
        for (run_id, session_id) in db.retire_source_events(
            &identity.profile_id,
            "amp",
            &retire_ids.into_iter().collect::<Vec<_>>(),
        )? {
            run_ids.insert(run_id);
            if let Some(session_id) = session_id {
                session_ids.insert(session_id);
            }
        }
    }
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
    Ok(())
}

fn recent_json_files(roots: &[PathBuf], modified_since_ns: i64) -> Result<Vec<ScannedFile>> {
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
            files.push(scanned_file_from_metadata(path, metadata, modified_ns));
        }
    }
    Ok(files)
}

fn scanned_file(path: &Path) -> Result<ScannedFile> {
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!("Amp JSON path is not a regular file")
    }
    let modified_ns = metadata.modified().map(system_time_ns).unwrap_or_default();
    Ok(scanned_file_from_metadata(
        path.to_path_buf(),
        metadata,
        modified_ns,
    ))
}

fn scanned_file_from_metadata(path: PathBuf, metadata: Metadata, modified_ns: i64) -> ScannedFile {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;

    ScannedFile {
        path,
        size: metadata.len(),
        modified_ns,
        #[cfg(unix)]
        device: metadata.dev(),
        #[cfg(unix)]
        inode: metadata.ino(),
    }
}

fn descriptor_snapshot(file: &File) -> Result<DescriptorSnapshot> {
    let metadata = file.metadata()?;
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;

    Ok(DescriptorSnapshot {
        size: metadata.len(),
        modified_ns: metadata.modified().map(system_time_ns)?,
        #[cfg(unix)]
        device: metadata.dev(),
        #[cfg(unix)]
        inode: metadata.ino(),
    })
}

fn validate_descriptor_snapshot(
    scanned: &ScannedFile,
    opened: &DescriptorSnapshot,
    current: &DescriptorSnapshot,
) -> Result<()> {
    let matches_scan = opened.size == scanned.size && opened.modified_ns == scanned.modified_ns;
    #[cfg(unix)]
    let matches_scan =
        matches_scan && opened.device == scanned.device && opened.inode == scanned.inode;
    if !matches_scan || current != opened {
        bail!("Amp JSON changed after scan")
    }
    Ok(())
}

fn open_scanned_file(scanned: &ScannedFile) -> Result<(File, DescriptorSnapshot)> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(&scanned.path)
        .with_context(|| format!("open {}", scanned.path.display()))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        bail!("opened Amp JSON is not a regular file")
    }
    let opened = descriptor_snapshot(&file)?;
    validate_descriptor_snapshot(scanned, &opened, &opened)?;
    Ok((file, opened))
}

fn read_scanned_file(scanned: &ScannedFile, limit: u64) -> Result<Vec<u8>> {
    let (mut file, opened) = open_scanned_file(scanned)?;
    let bytes = read_bounded(&mut file, limit)?;
    let after = descriptor_snapshot(&file)?;
    validate_descriptor_snapshot(scanned, &opened, &after)?;
    Ok(bytes)
}

fn read_bounded(file: &mut File, limit: u64) -> Result<Vec<u8>> {
    if file.metadata()?.len() > limit {
        bail!("Amp JSON exceeds size limit")
    }
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        bail!("Amp JSON grew beyond size limit")
    }
    Ok(bytes)
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
    parse_thread_reader(Cursor::new(json.as_bytes()), source_kind, identity)
}

pub(crate) fn parse_thread_reader<R: Read>(
    reader: R,
    source_kind: &str,
    identity: &ImportIdentity,
) -> Result<ParsedThread> {
    let thread: Thread = serde_json::from_reader(BufReader::new(reader))?;
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
    let mut candidate_message_ids = HashSet::new();
    let mut cache = HashMap::new();
    for (index, message) in thread.messages.iter().enumerate() {
        if message.role.as_deref() == Some("assistant") {
            if let Some(id) = message
                .message_id
                .as_deref()
                .filter(|id| !id.trim().is_empty())
            {
                candidate_message_ids.insert(format!("amp:{thread_id}:message:{id}"));
            } else if let (Some(model), Some(occurred)) = (
                message
                    .usage
                    .as_ref()
                    .and_then(|usage| usage.model.as_deref())
                    .or(message.model.as_deref())
                    .filter(|model| !model.is_empty()),
                message
                    .usage
                    .as_ref()
                    .and_then(|usage| usage.timestamp.as_deref())
                    .or(message.timestamp.as_deref())
                    .and_then(timestamp_ns),
            ) {
                let id = stable_hash(&format!("{thread_id}:{occurred}:{index}:{model}"));
                candidate_message_ids.insert(format!("amp:{thread_id}:message:{id}"));
            }
        }
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
    let used_ledger = malformed_ledger_count == 0 && !ledger_events.is_empty();
    let superseded_source_event_ids = if used_ledger {
        let mut ids = candidate_message_ids.into_iter().collect::<Vec<_>>();
        ids.sort();
        ids
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
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::{fs, io::Cursor, path::PathBuf, sync::Mutex, time::SystemTime};

    use crate::db::Database;

    use super::*;

    #[test]
    fn cli_table_parser_accepts_unicode_spaces_and_archived_rows() {
        let table = "Title  Last Updated  Visibility  Messages  Thread ID\n-----  ---- -------  ----------  --------  ------ --\n日本語 title with spaces  2 days ago  private  7 T-one\narchived café  1 year ago archived 0 T-two\n\u{1b}[0m\n";
        let page = parse_cli_thread_page(table.as_bytes()).unwrap();
        assert_eq!(page.candidate_rows, 2);
        assert_eq!(
            page.threads[0],
            CliThread {
                id: "T-one".into(),
                message_count: 7
            }
        );
        assert_eq!(page.threads[1].id, "T-two");
    }

    #[test]
    fn cli_table_parser_rejects_a_malformed_candidate() {
        let table = b"Title Last Updated Visibility Messages Thread ID\n----- ---- ------- ---------- -------- ------ --\ngood yesterday private 2 T-good\nbad yesterday private nope T-bad\n";
        assert!(parse_cli_thread_page(table).is_err());
    }

    #[test]
    fn cli_table_parser_rejects_nonempty_output_without_the_table() {
        assert!(parse_cli_thread_page(b"login required\n").is_err());
        assert!(
            parse_cli_thread_page(b"\x1b[0m\n")
                .unwrap()
                .threads
                .is_empty()
        );
    }

    fn list_table(rows: impl Iterator<Item = String>) -> Vec<u8> {
        let mut output = String::from(
            "Title Last Updated Visibility Messages Thread ID\n----- ---- ------- ---------- -------- ------ --\n",
        );
        output.extend(rows);
        output.into_bytes()
    }

    struct FakeRunner {
        offsets: Mutex<Vec<usize>>,
        fail: Option<String>,
    }
    impl AmpCommandRunner for FakeRunner {
        fn list(&self, offset: usize) -> Result<Vec<u8>> {
            self.offsets.lock().unwrap().push(offset);
            let range = if offset == 0 { 0..100 } else { 100..101 };
            Ok(list_table(range.map(|i| {
                format!("title {i} 1 day ago archived 0 T-{i}\n")
            })))
        }
        fn export(&self, id: &str, identity: &ImportIdentity) -> Result<ParsedExport> {
            if self.fail.as_deref() == Some(id) {
                bail!("private failure")
            }
            let bytes =
                format!(r#"{{"id":"{id}","updatedAt":"2020-01-01T00:00:00Z","messages":[]}}"#)
                    .into_bytes();
            parse_export_reader(Cursor::new(bytes), MAX_THREAD_JSON_BYTES, identity)
        }
    }

    #[test]
    fn cli_sync_pages_archived_threads_and_skips_stale_unchanged_threads() {
        let path = std::env::temp_dir().join(format!("shirabe-amp-cli-{}.db", now_ns()));
        let db = Database::open(&path).unwrap();
        db.migrate().unwrap();
        let runner = FakeRunner {
            offsets: Mutex::new(Vec::new()),
            fail: None,
        };
        let identity = ImportIdentity::new("p", "d");
        let first = sync_with_runner(&db, &runner, &identity).unwrap();
        assert_eq!(first.files_imported, 101);
        assert_eq!(*runner.offsets.lock().unwrap(), vec![0, 100]);
        runner.offsets.lock().unwrap().clear();
        let second = sync_with_runner(&db, &runner, &identity).unwrap();
        assert_eq!(second.files_skipped, 101);
        assert_eq!(second.files_imported, 0);
        drop(db);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn one_export_failure_does_not_block_other_threads() {
        let path = std::env::temp_dir().join(format!("shirabe-amp-failure-{}.db", now_ns()));
        let db = Database::open(&path).unwrap();
        db.migrate().unwrap();
        let runner = FakeRunner {
            offsets: Mutex::new(Vec::new()),
            fail: Some("T-0".into()),
        };
        let report = sync_with_runner(&db, &runner, &ImportIdentity::new("p", "d")).unwrap();
        assert_eq!(report.files_imported, 100);
        assert_eq!(report.warnings.len(), 1);
        let errors: String = db
            .connection()
            .query_row(
                "SELECT group_concat(COALESCE(error_message,''),' ') FROM import_files",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!errors.contains("private"));
        drop(db);
        let _ = fs::remove_file(path);
    }

    struct MutableRunner {
        message_count: Mutex<usize>,
        payload: Vec<u8>,
        exports: Mutex<usize>,
        fail: Mutex<bool>,
    }
    impl AmpCommandRunner for MutableRunner {
        fn list(&self, _: usize) -> Result<Vec<u8>> {
            let count = *self.message_count.lock().unwrap();
            Ok(list_table(std::iter::once(format!(
                "title now private {count} T-state\n"
            ))))
        }
        fn export(&self, _: &str, identity: &ImportIdentity) -> Result<ParsedExport> {
            *self.exports.lock().unwrap() += 1;
            if *self.fail.lock().unwrap() {
                bail!("PRIVATE_SENTINEL")
            }
            parse_export_reader(
                Cursor::new(self.payload.clone()),
                MAX_THREAD_JSON_BYTES,
                identity,
            )
        }
    }

    #[test]
    fn cli_refreshes_recent_unchanged_changed_count_and_manually_pending_threads() -> Result<()> {
        let root = temp_dir("amp-cli-refresh");
        fs::create_dir_all(&root)?;
        let db = test_db(&root)?;
        let runner = MutableRunner {
            message_count: Mutex::new(1),
            payload: br#"{"id":"T-state","updatedAt":"2099-01-01T00:00:00Z","messages":[]}"#
                .to_vec(),
            exports: Mutex::new(0),
            fail: Mutex::new(false),
        };
        let identity = ImportIdentity::new("p", "d");
        sync_with_runner(&db, &runner, &identity)?;
        sync_with_runner(&db, &runner, &identity)?;
        assert_eq!(
            *runner.exports.lock().unwrap(),
            2,
            "recent updatedAt refreshes even when unchanged"
        );

        *runner.message_count.lock().unwrap() = 2;
        sync_with_runner(&db, &runner, &identity)?;
        assert_eq!(
            *runner.exports.lock().unwrap(),
            3,
            "message count change refreshes"
        );

        let source_id: String = db.connection().query_row(
            "SELECT source_id FROM import_sources WHERE source_kind='amp_cli_thread_export'",
            [],
            |row| row.get(0),
        )?;
        let successful = db.amp_virtual_state(&source_id, "T-state")?.unwrap();
        db.prepare_amp_virtual(&source_id, "T-state", 2)?;
        let pending = db.amp_virtual_state(&source_id, "T-state")?.unwrap();
        assert_eq!(pending.status, "pending");
        assert_eq!(pending.payload_fingerprint, successful.payload_fingerprint);
        assert_eq!(pending.updated_at_ns, successful.updated_at_ns);
        assert_eq!(pending.event_count, successful.event_count);
        sync_with_runner(&db, &runner, &identity)?;
        assert_eq!(
            *runner.exports.lock().unwrap(),
            4,
            "crash-like pending records retry"
        );
        let recovered = db.amp_virtual_state(&source_id, "T-state")?.unwrap();
        assert_eq!(recovered.status, "imported");
        assert_eq!(
            recovered.payload_fingerprint,
            successful.payload_fingerprint
        );
        drop(db);
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn failed_cli_retry_preserves_successful_fingerprint_updated_at_and_event_count() -> Result<()>
    {
        let root = temp_dir("amp-cli-preserve");
        fs::create_dir_all(&root)?;
        let db = test_db(&root)?;
        let runner = MutableRunner {
            message_count: Mutex::new(1),
            payload: message_thread("T-state", "m1", 3, 0, 0, 0).into_bytes(),
            exports: Mutex::new(0),
            fail: Mutex::new(false),
        };
        let identity = ImportIdentity::new("p", "d");
        sync_with_runner(&db, &runner, &identity)?;
        let before: (String, i64, String, i64) = db.connection().query_row(
            "SELECT fingerprint, event_count, metadata_json, modified_ns FROM import_files",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        *runner.message_count.lock().unwrap() = 2;
        *runner.fail.lock().unwrap() = true;
        let report = sync_with_runner(&db, &runner, &identity)?;
        assert_eq!(report.warnings.len(), 1);
        let after: (String, i64, String, i64, String) = db.connection().query_row(
            "SELECT fingerprint, event_count, metadata_json, modified_ns, error_message FROM import_files",
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )?;
        assert_eq!(
            (&after.0, after.1, after.3),
            (&before.0, before.1, before.3)
        );
        let before_state: AmpVirtualImportState = serde_json::from_str(&before.2)?;
        let after_state: AmpVirtualImportState = serde_json::from_str(&after.2)?;
        assert_eq!(
            (
                after_state.payload_fingerprint,
                after_state.updated_at_ns,
                after_state.event_count
            ),
            (
                before_state.payload_fingerprint,
                before_state.updated_at_ns,
                before_state.event_count
            )
        );
        assert!(!after.4.contains("PRIVATE_SENTINEL"));

        *runner.fail.lock().unwrap() = false;
        *runner.message_count.lock().unwrap() = 3;
        let new_payload = message_thread("T-state", "m2", 7, 0, 0, 0).into_bytes();
        let new_fingerprint = hex::encode(Sha256::digest(&new_payload));
        // MutableRunner owns the fixture for its lifetime, so replace it through
        // the test's uniquely held value before the final retry.
        let runner = MutableRunner {
            message_count: Mutex::new(3),
            payload: new_payload,
            exports: Mutex::new(*runner.exports.lock().unwrap()),
            fail: Mutex::new(false),
        };
        let exports_before = *runner.exports.lock().unwrap();
        sync_with_runner(&db, &runner, &identity)?;
        assert_eq!(*runner.exports.lock().unwrap(), exports_before + 1);
        let recovered: (String, String, i64, String) = db.connection().query_row(
            "SELECT status, fingerprint, event_count, metadata_json FROM import_files",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        assert_eq!(recovered.0, "imported");
        assert_eq!(recovered.1, new_fingerprint);
        assert_eq!(recovered.2, 1);
        let recovered_state: AmpVirtualImportState = serde_json::from_str(&recovered.3)?;
        assert_eq!(recovered_state.payload_fingerprint, Some(new_fingerprint));
        assert_eq!(recovered_state.event_count, 1);
        drop(db);
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    struct MalformedSecondPageRunner {
        exports: Mutex<usize>,
    }
    impl AmpCommandRunner for MalformedSecondPageRunner {
        fn list(&self, offset: usize) -> Result<Vec<u8>> {
            if offset == 0 {
                Ok(list_table(
                    (0..100).map(|i| format!("title private 0 T-{i}\n")),
                ))
            } else {
                Ok(list_table(std::iter::once("malformed candidate\n".into())))
            }
        }
        fn export(&self, _: &str, _: &ImportIdentity) -> Result<ParsedExport> {
            *self.exports.lock().unwrap() += 1;
            bail!("must not export")
        }
    }

    #[test]
    fn malformed_later_page_falls_back_across_all_roots_without_partial_cli_state() -> Result<()> {
        let root = temp_dir("amp-cli-fallback");
        let roots = [root.join("one"), root.join("two")];
        fs::create_dir_all(&roots[0])?;
        fs::create_dir_all(&roots[1])?;
        fs::write(
            roots[0].join("one.json"),
            message_thread("T-local-one", "m1", 1, 0, 0, 0),
        )?;
        fs::write(
            roots[1].join("two.json"),
            message_thread("T-local-two", "m2", 2, 0, 0, 0),
        )?;
        let db = test_db(&root)?;
        let runner = MalformedSecondPageRunner {
            exports: Mutex::new(0),
        };
        let report = sync_cli_or_local_with_runner(
            &db,
            Some(&runner),
            &roots,
            i64::MIN,
            &ImportIdentity::new("p", "d"),
        )?;
        assert_eq!((report.files_imported, report.events_projected), (2, 2));
        assert_eq!(*runner.exports.lock().unwrap(), 0);
        let cli_sources: i64 = db.connection().query_row(
            "SELECT count(*) FROM import_sources WHERE source_kind='amp_cli_thread_export'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(cli_sources, 0);
        drop(db);
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn post_discovery_db_failure_does_not_start_local_fallback() -> Result<()> {
        let root = temp_dir("amp-cli-db-failure");
        let fallback = root.join("fallback");
        fs::create_dir_all(&fallback)?;
        fs::write(
            fallback.join("thread.json"),
            message_thread("T-local", "m-local", 9, 0, 0, 0),
        )?;
        let db = test_db(&root)?;
        db.connection().execute("DROP TABLE import_files", [])?;
        let runner = FakeRunner {
            offsets: Mutex::new(Vec::new()),
            fail: None,
        };

        let result = sync_cli_or_local_with_runner(
            &db,
            Some(&runner),
            &[fallback],
            i64::MIN,
            &ImportIdentity::new("p", "d"),
        );

        assert!(result.is_err());
        let local_sources: i64 = db.connection().query_row(
            "SELECT count(*) FROM import_sources WHERE source_kind=?1",
            [SOURCE_KIND],
            |row| row.get(0),
        )?;
        assert_eq!(local_sources, 0, "local fallback must not be attempted");
        drop(db);
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[cfg(unix)]
    fn executable_script(name: &str, body: &str) -> Result<PathBuf> {
        let path = std::env::temp_dir().join(format!("shirabe-{name}-{}", now_ns()));
        fs::write(&path, format!("#!/bin/sh\n{body}\n"))?;
        let mut permissions = fs::metadata(&path)?.permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&path, permissions)?;
        Ok(path)
    }

    #[cfg(unix)]
    #[test]
    fn process_runner_times_out_promptly_and_reaps_child() -> Result<()> {
        let script = executable_script("amp-timeout", "exec sleep 5")?;
        let runner = ProcessAmpRunner {
            executable: script.clone(),
            deadline: Duration::from_millis(50),
            list_cap: 128,
            export_cap: 128,
        };
        let started = Instant::now();
        let error = runner.list(0).unwrap_err().to_string();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(error.contains("list") && error.contains("timed out"));
        assert!(!error.contains("PRIVATE_SENTINEL"));
        fs::remove_file(script)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn process_runner_caps_stdout_and_drains_large_stderr_privately() -> Result<()> {
        let overflow = executable_script("amp-overflow", "printf 'PRIVATE_SENTINEL_OVERFLOW'")?;
        let runner = ProcessAmpRunner {
            executable: overflow.clone(),
            deadline: Duration::from_secs(1),
            list_cap: 4,
            export_cap: 4,
        };
        let identity = ImportIdentity::new("p", "d");
        let error = runner.export("T-safe", &identity).unwrap_err().to_string();
        assert!(error.contains("export") && error.contains("T-safe") && error.contains("limit"));
        assert!(!error.contains("PRIVATE_SENTINEL"));
        fs::remove_file(overflow)?;

        let flood = executable_script(
            "amp-stderr",
            "i=0; while [ $i -lt 20000 ]; do echo PRIVATE_SENTINEL >&2; i=$((i+1)); done; printf '{\"id\":\"T-safe\",\"updatedAt\":\"2020-01-01T00:00:00Z\",\"messages\":[]}'",
        )?;
        let runner = ProcessAmpRunner {
            executable: flood.clone(),
            deadline: Duration::from_secs(3),
            list_cap: 8,
            export_cap: 256,
        };
        let exported = runner.export("T-safe", &identity)?;
        let fixture = br#"{"id":"T-safe","updatedAt":"2020-01-01T00:00:00Z","messages":[]}"#;
        assert_eq!(exported.byte_count, fixture.len() as u64);
        assert_eq!(exported.fingerprint, hex::encode(Sha256::digest(fixture)));
        assert_eq!(exported.parsed.thread_id, "T-safe");
        fs::remove_file(flood)?;
        Ok(())
    }
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
    fn thread_parser_accepts_a_reader_and_skips_unknown_content() {
        let json = wrap(
            "",
            r#"[{"role":"assistant","messageId":"m","model":"x","timestamp":1,"unknown":{"private":"PRIVATE_SENTINEL"},"usage":{"inputTokens":2}}]"#,
        );
        let parsed = parse_thread_reader(
            Cursor::new(json.into_bytes()),
            "amp_thread_json",
            &ImportIdentity::new("p", "d"),
        )
        .unwrap();
        assert_eq!(parsed.events.len(), 1);
        assert!(!format!("{parsed:?}").contains("PRIVATE_SENTINEL"));
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
            vec!["amp:T-test-1:message:7", "amp:T-test-1:message:8"]
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
    fn malformed_ledger_element_makes_entire_ledger_unusable() {
        let private = "PRIVATE_ELEMENT_SENTINEL";
        let p = parse(&wrap(&format!(r#", "usageLedger":{{"events":[{{"timestamp":{{"private":"{private}"}},"tokens":{{"input":"bad"}}}}, {{"id":"usable","timestamp":1,"model":"x","tokens":{{"input":2}}}}]}}"#), r#"[{"role":"assistant","messageId":"current","model":"x","timestamp":1,"usage":{"inputTokens":7}}]"#)).unwrap();
        assert!(!p.used_ledger);
        assert_eq!(p.malformed_ledger_count, 1);
        assert_eq!(p.events.len(), 1);
        assert_eq!(
            p.events[0].source_event_id.as_deref(),
            Some("amp:T-test-1:message:current")
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
        assert_eq!(files[0].path, path);
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

    #[cfg(unix)]
    #[test]
    fn secure_open_rejects_a_symlink_even_after_scan() -> Result<()> {
        use std::os::unix::fs::symlink;

        let root = temp_dir("amp-local-secure-open");
        fs::create_dir_all(&root)?;
        let original = root.join("original.json");
        let replacement = root.join("replacement.json");
        fs::write(&original, "{}")?;
        fs::write(&replacement, "{}")?;
        let scanned = scanned_file(&original)?;
        fs::remove_file(&original)?;
        symlink(&replacement, &original)?;

        assert!(open_scanned_file(&scanned).is_err());
        Ok(())
    }

    #[test]
    fn secure_open_rejects_metadata_changed_after_scan() -> Result<()> {
        let root = temp_dir("amp-local-scan-metadata-change");
        fs::create_dir_all(&root)?;
        let path = root.join("thread.json");
        fs::write(&path, "{}")?;
        let mut scanned = scanned_file(&path)?;
        scanned.size += 1;

        assert!(open_scanned_file(&scanned).is_err());
        Ok(())
    }

    #[test]
    fn post_read_validation_rejects_descriptor_truncated_after_open() -> Result<()> {
        let root = temp_dir("amp-local-post-read-change");
        fs::create_dir_all(&root)?;
        let path = root.join("thread.json");
        fs::write(&path, "12345")?;
        let scanned = scanned_file(&path)?;
        let (file, opened) = open_scanned_file(&scanned)?;

        File::create(&path)?.set_len(0)?;
        let after = descriptor_snapshot(&file)?;

        assert!(validate_descriptor_snapshot(&scanned, &opened, &after).is_err());
        Ok(())
    }

    #[test]
    fn bounded_read_rejects_metadata_oversize_and_growth() -> Result<()> {
        let root = temp_dir("amp-local-size-limit");
        fs::create_dir_all(&root)?;
        let path = root.join("thread.json");
        fs::write(&path, "12345")?;
        let scanned = scanned_file(&path)?;
        let (mut file, _) = open_scanned_file(&scanned)?;
        assert!(read_bounded(&mut file, 4).is_err());

        fs::write(&path, "1234")?;
        let scanned = scanned_file(&path)?;
        let (mut file, _) = open_scanned_file(&scanned)?;
        fs::write(&path, "12345")?;
        assert!(read_bounded(&mut file, 4).is_err());
        Ok(())
    }

    #[test]
    fn finalized_message_exactly_replaces_all_mutable_usage_fields() -> Result<()> {
        let root = temp_dir("amp-local-update");
        fs::create_dir_all(&root)?;
        let path = root.join("thread.json");
        fs::write(&path, message_thread("T-one", "m1", 10, 5, 3, 2))?;
        let db = test_db(&root)?;
        let identity = ImportIdentity::new("profile", "device");
        import_local_with_identity(&db, path.clone(), &identity)?;

        fs::write(
            &path,
            r#"{"id":"T-one","updatedAt":"2026-01-02T00:00:00Z","messages":[{"role":"assistant","messageId":"m1","model":"ignored-padding","usage":{"model":"gpt-5","timestamp":1767312000000,"inputTokens":100,"outputTokens":50,"cacheCreationInputTokens":30,"cacheReadInputTokens":20}}]}"#,
        )?;
        import_local_with_identity(&db, path, &identity)?;

        let values: (i64, i64, i64, i64, String, i64, Option<i64>) = db.connection().query_row(
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
                None
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
        let retained: (String, i64, i64, i64, i64, i64, Option<i64>) = db.connection().query_row(
            "SELECT model, started_at_ns, input_tokens, output_tokens, cache_write_tokens, cache_read_tokens, model_context_window FROM llm_calls",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)),
        )?;
        assert_eq!(
            retained,
            (
                "claude-test".into(),
                1_767_225_600_000_000_000,
                10,
                5,
                3,
                2,
                Some(200000)
            )
        );

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
    fn usable_ledger_retires_all_in_payload_message_identities() -> Result<()> {
        let root = temp_dir("amp-local-reconcile-all-present");
        fs::create_dir_all(&root)?;
        let path = root.join("thread.json");
        fs::write(
            &path,
            r#"{"id":"T-one","messages":[{"role":"assistant","messageId":"m1","model":"claude-test","timestamp":1,"usage":{"inputTokens":10}},{"role":"assistant","messageId":"m2","model":"claude-test","timestamp":2,"usage":{"inputTokens":20}}]}"#,
        )?;
        let db = test_db(&root)?;
        let identity = ImportIdentity::new("profile", "device");
        import_local_with_identity(&db, path.clone(), &identity)?;
        assert_eq!(count(&db, "llm_calls")?, 2);

        fs::write(
            &path,
            r#"{"id":"T-one","messages":[{"role":"assistant","messageId":"m1","model":"claude-test","timestamp":1,"usage":{"inputTokens":0}},{"role":"assistant","messageId":"m2","usage":{"inputTokens":20}}],"usageLedger":{"events":[{"id":"ledger-1","timestamp":3,"model":"claude-ledger","tokens":{"input":30}}]}}"#,
        )?;
        import_local_with_identity(&db, path, &identity)?;

        assert_eq!(count(&db, "llm_calls")?, 1);
        let model: String =
            db.connection()
                .query_row("SELECT model FROM llm_calls", [], |row| row.get(0))?;
        assert_eq!(model, "claude-ledger");
        Ok(())
    }

    #[test]
    fn usable_ledger_does_not_retire_historical_message_absent_from_payload() -> Result<()> {
        let root = temp_dir("amp-local-reconcile-absent");
        fs::create_dir_all(&root)?;
        let path = root.join("thread.json");
        fs::write(
            &path,
            r#"{"id":"T-one","messages":[{"role":"assistant","messageId":"present","model":"claude-present","timestamp":1,"usage":{"inputTokens":10}},{"role":"assistant","messageId":"absent","model":"claude-absent","timestamp":2,"usage":{"inputTokens":20}}]}"#,
        )?;
        let db = test_db(&root)?;
        let identity = ImportIdentity::new("profile", "device");
        import_local_with_identity(&db, path.clone(), &identity)?;

        fs::write(
            &path,
            r#"{"id":"T-one","messages":[{"role":"assistant","messageId":"present","usage":{}}],"usageLedger":{"events":[{"id":"ledger-1","timestamp":3,"model":"claude-ledger","tokens":{"input":30}}]}}"#,
        )?;
        import_local_with_identity(&db, path, &identity)?;

        let mut models = db
            .connection()
            .prepare("SELECT model FROM llm_calls ORDER BY model")?;
        let models = models
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        assert_eq!(models, vec!["claude-absent", "claude-ledger"]);
        Ok(())
    }

    #[test]
    fn mixed_ledger_falls_back_to_current_message_without_duplicate_transition() -> Result<()> {
        let root = temp_dir("amp-local-mixed-ledger");
        fs::create_dir_all(&root)?;
        let path = root.join("thread.json");
        fs::write(&path, message_thread("T-one", "m1", 10, 5, 3, 2))?;
        let db = test_db(&root)?;
        let identity = ImportIdentity::new("profile", "device");
        import_local_with_identity(&db, path.clone(), &identity)?;

        fs::write(
            &path,
            r#"{"id":"T-one","messages":[{"role":"assistant","messageId":"m1","model":"claude-current","timestamp":1767225600000,"usage":{"inputTokens":20,"outputTokens":6}}],"usageLedger":{"events":[{"id":"ledger","timestamp":1767225600000,"model":"claude-ledger","tokens":{"input":99}}, {"tokens":"PRIVATE_SENTINEL"}]}}"#,
        )?;
        import_local_with_identity(&db, path.clone(), &identity)?;
        assert_eq!(count(&db, "llm_calls")?, 1);
        let current: (String, i64) =
            db.connection()
                .query_row("SELECT model, input_tokens FROM llm_calls", [], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?;
        assert_eq!(current, ("claude-current".into(), 20));

        fs::write(
            &path,
            r#"{"id":"T-one","messages":[],"usageLedger":{"events":[{"id":"ledger","timestamp":1767225600000,"model":"claude-ledger","tokens":{"input":99}}, {"tokens":"PRIVATE_SENTINEL"}]}}"#,
        )?;
        import_local_with_identity(&db, path, &identity)?;
        assert_eq!(count(&db, "llm_calls")?, 1);
        let unchanged: (String, i64) =
            db.connection()
                .query_row("SELECT model, input_tokens FROM llm_calls", [], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?;
        assert_eq!(unchanged, current);
        Ok(())
    }

    #[test]
    fn failed_amp_record_retries_with_unchanged_fingerprint() -> Result<()> {
        let root = temp_dir("amp-local-failed-retry");
        fs::create_dir_all(&root)?;
        let path = root.join("thread.json");
        fs::write(&path, message_thread("T-retry", "m1", 10, 5, 3, 2))?;
        let db = test_db(&root)?;
        let identity = ImportIdentity::new("profile", "device");
        let source_id = db.record_import_source_for_profile(
            "amp",
            SOURCE_KIND,
            Path::new("<amp-local>"),
            "profile",
            "device",
        )?;
        let scanned = scanned_file(&path)?;
        let prepared =
            db.prepare_import_file(&source_id, &path, scanned.size, scanned.modified_ns)?;
        db.finish_import_file(
            &prepared.file_id,
            "failed",
            0,
            1,
            None,
            None,
            None,
            Some("safe failure"),
        )?;

        let report = import_local_with_identity(&db, path, &identity)?;
        assert_eq!((report.files_imported, report.events_projected), (1, 1));
        assert_eq!(count(&db, "llm_calls")?, 1);
        Ok(())
    }

    #[test]
    fn oversized_import_is_failed_content_free_and_retryable() -> Result<()> {
        let root = temp_dir("amp-local-oversized");
        fs::create_dir_all(&root)?;
        let path = root.join("thread.json");
        fs::write(&path, message_thread("T-large", "m1", 10, 5, 3, 2))?;
        let db = test_db(&root)?;
        let identity = ImportIdentity::new("profile", "device");
        let scanned = scanned_file(&path)?;
        let source_id = db.record_import_source_for_profile(
            "amp",
            SOURCE_KIND,
            Path::new("<amp-local>"),
            "profile",
            "device",
        )?;
        let prepared =
            db.prepare_import_file(&source_id, &path, scanned.size, scanned.modified_ns)?;
        let mut stats = ScanStats::default();
        import_file_with_limit(&db, &prepared.file_id, &scanned, &identity, &mut stats, 8)?;
        let (status, error): (String, Option<String>) = db.connection().query_row(
            "SELECT status, error_message FROM import_files WHERE file_id = ?1",
            [&prepared.file_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(status, "failed");
        assert_eq!(error.as_deref(), Some("Amp JSON file exceeds size limit"));
        assert_eq!(count(&db, "llm_calls")?, 0);
        assert!(
            db.prepare_import_file(&source_id, &path, scanned.size, scanned.modified_ns)?
                .should_import
        );
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
