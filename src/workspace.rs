#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    config::{Identity, SourcePaths, SourceSettings, home_dir},
    db::{Database, now_ns, stable_hash},
    importers::{self, ImportIdentity, ImportReport, SupersedeCollectedEvents},
    projection::{
        event::NormalizedEvent,
        projector::{ProjectionCache, Projector},
    },
    rollup,
};

const COLLECT_WATERMARK_OVERLAP_NS: i64 = 10 * 60 * 1_000_000_000;

#[derive(Debug, Serialize)]
pub struct WorkspaceReport {
    pub status: &'static str,
    pub workspace_dir: PathBuf,
    pub catalog_db: PathBuf,
    pub workspace_json: PathBuf,
    pub inbox_dir: PathBuf,
    pub profiles_dir: PathBuf,
    pub logs_dir: PathBuf,
}

#[derive(Debug, Serialize)]
pub struct CollectReport {
    pub status: &'static str,
    pub workspace_dir: PathBuf,
    pub inbox_file: Option<PathBuf>,
    pub events_collected: usize,
    pub sources: Vec<ImportReport>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DrainReport {
    pub status: &'static str,
    pub files_drained: usize,
    pub events_projected: usize,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct CollectorState {
    #[serde(default)]
    sources: HashMap<String, i64>,
    #[serde(default)]
    amp: importers::amp::AmpCollectorState,
}

#[derive(Debug, Deserialize, Serialize)]
struct PendingCollection {
    inbox_file: String,
    state: CollectorState,
}

#[derive(Debug, Deserialize, Serialize)]
struct WorkspaceProfile {
    device_id: String,
    device_label: String,
    profile_id: String,
    profile_label: String,
    macos_uid: Option<String>,
    macos_username: String,
    updated_at_ns: i64,
}

pub fn default_shared_workspace_dir() -> PathBuf {
    crate::config::default_shared_workspace_dir()
}

pub fn prepare(path: Option<PathBuf>) -> Result<WorkspaceReport> {
    let workspace_dir = path.unwrap_or_else(default_shared_workspace_dir);
    let inbox_dir = workspace_dir.join("inbox");
    let profiles_dir = workspace_dir.join("profiles");
    let collector_state_dir = workspace_dir.join("collector-state");
    let logs_dir = workspace_dir.join("logs");
    let workspace_json = workspace_dir.join("workspace.json");
    let catalog_db = workspace_dir.join("catalog.sqlite");

    fs::create_dir_all(&workspace_dir)
        .with_context(|| format!("create {}", workspace_dir.display()))?;
    fs::create_dir_all(&inbox_dir).with_context(|| format!("create {}", inbox_dir.display()))?;
    fs::create_dir_all(&profiles_dir)
        .with_context(|| format!("create {}", profiles_dir.display()))?;
    fs::create_dir_all(&collector_state_dir)
        .with_context(|| format!("create {}", collector_state_dir.display()))?;
    fs::create_dir_all(&logs_dir).with_context(|| format!("create {}", logs_dir.display()))?;

    chmod_best_effort(&workspace_dir, 0o1777)?;
    chmod_best_effort(&inbox_dir, 0o1777)?;
    chmod_best_effort(&profiles_dir, 0o1777)?;
    chmod_best_effort(&collector_state_dir, 0o777)?;
    chmod_best_effort(&logs_dir, 0o777)?;

    if !workspace_json.exists() {
        let workspace_id = format!(
            "workspace_{}",
            &stable_hash(&format!("{}:{}", workspace_dir.display(), now_ns()))[..16]
        );
        let raw = serde_json::to_string_pretty(&json!({
            "workspace_id": workspace_id,
            "created_at_ns": now_ns(),
            "version": 1
        }))?;
        fs::write(&workspace_json, format!("{raw}\n"))
            .with_context(|| format!("write {}", workspace_json.display()))?;
    }
    chmod_best_effort(&workspace_json, 0o666)?;

    Ok(WorkspaceReport {
        status: "ok",
        workspace_dir,
        catalog_db,
        workspace_json,
        inbox_dir,
        profiles_dir,
        logs_dir,
    })
}

pub fn seed_pricing_cache_from_default_local(db: &Database) -> Result<bool> {
    #[cfg(windows)]
    {
        if let Some(app_data) = std::env::var_os("LOCALAPPDATA")
            .filter(|value| !value.is_empty())
            .or_else(|| std::env::var_os("APPDATA").filter(|value| !value.is_empty()))
        {
            return seed_pricing_cache(
                db,
                &PathBuf::from(app_data)
                    .join("Shirabe")
                    .join("catalog.sqlite"),
            );
        }
    }

    let Some(home) = home_dir() else {
        return Ok(false);
    };
    seed_pricing_cache(db, &home.join(".shirabe").join("catalog.sqlite"))
}

pub fn seed_pricing_cache(db: &Database, source_catalog: &Path) -> Result<bool> {
    if !source_catalog.exists() {
        return Ok(false);
    }

    match (
        fs::canonicalize(source_catalog),
        fs::canonicalize(db.path()),
    ) {
        (Ok(source), Ok(target)) if source == target => return Ok(false),
        _ => {}
    }

    let source = Connection::open_with_flags(source_catalog, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open pricing seed sqlite {}", source_catalog.display()))?;
    if !sqlite_table_exists(&source, "pricing_sources")?
        || !sqlite_table_exists(&source, "model_prices")?
    {
        return Ok(false);
    }

    let source_count: i64 =
        source.query_row("SELECT COUNT(*) FROM model_prices", [], |row| row.get(0))?;
    if source_count == 0 {
        return Ok(false);
    }

    let target = db.connection();

    type PricingSourceRow = (String, String, i64, i64, String, Option<String>);
    type ModelPriceRow = (
        String,
        String,
        Option<String>,
        Option<f64>,
        Option<f64>,
        Option<f64>,
        Option<f64>,
        i64,
        Option<String>,
    );

    let pricing_sources = source
        .prepare(
            "SELECT pricing_source_id, source_url, fetched_at_ns, model_count, raw_json, metadata_json
             FROM pricing_sources",
        )?
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<PricingSourceRow>>>()?;
    let model_prices = source
        .prepare(
            "SELECT model_name, source_id, provider, input_cost_per_token, output_cost_per_token,
                    cache_read_input_token_cost, cache_creation_input_token_cost, updated_at_ns, metadata_json
             FROM model_prices",
        )?
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<ModelPriceRow>>>()?;

    let tx = db.begin_batch()?;
    let mut changed = false;
    for (pricing_source_id, source_url, fetched_at_ns, model_count, raw_json, metadata_json) in
        pricing_sources
    {
        changed |= target.execute(
            "INSERT INTO pricing_sources (
                pricing_source_id, source_url, fetched_at_ns, model_count, raw_json, metadata_json
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(pricing_source_id) DO UPDATE SET
                source_url = excluded.source_url,
                fetched_at_ns = excluded.fetched_at_ns,
                model_count = excluded.model_count,
                raw_json = excluded.raw_json,
                metadata_json = excluded.metadata_json
             WHERE excluded.fetched_at_ns > pricing_sources.fetched_at_ns",
            params![
                pricing_source_id,
                source_url,
                fetched_at_ns,
                model_count,
                raw_json,
                metadata_json
            ],
        )? > 0;
    }
    for (
        model_name,
        source_id,
        provider,
        input_cost_per_token,
        output_cost_per_token,
        cache_read_input_token_cost,
        cache_creation_input_token_cost,
        updated_at_ns,
        metadata_json,
    ) in model_prices
    {
        changed |= target.execute(
            "INSERT INTO model_prices (
                model_name, source_id, provider, input_cost_per_token, output_cost_per_token,
                cache_read_input_token_cost, cache_creation_input_token_cost, updated_at_ns, metadata_json
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(model_name) DO UPDATE SET
                source_id = excluded.source_id,
                provider = excluded.provider,
                input_cost_per_token = excluded.input_cost_per_token,
                output_cost_per_token = excluded.output_cost_per_token,
                cache_read_input_token_cost = excluded.cache_read_input_token_cost,
                cache_creation_input_token_cost = excluded.cache_creation_input_token_cost,
                updated_at_ns = excluded.updated_at_ns,
                metadata_json = excluded.metadata_json
             WHERE excluded.updated_at_ns > model_prices.updated_at_ns",
            params![
                model_name,
                source_id,
                provider,
                input_cost_per_token,
                output_cost_per_token,
                cache_read_input_token_cost,
                cache_creation_input_token_cost,
                updated_at_ns,
                metadata_json
            ],
        )? > 0;
    }
    tx.commit()?;
    Ok(changed)
}

pub fn collect_to_inbox(
    workspace_path: Option<PathBuf>,
    source_paths: &SourcePaths,
    source_settings: &SourceSettings,
    identity: &Identity,
) -> Result<CollectReport> {
    collect_to_inbox_with(
        workspace_path,
        source_paths,
        source_settings,
        identity,
        importers::amp::collect_cli_first_recent,
        |temp_file, inbox_file| {
            fs::rename(temp_file, inbox_file).with_context(|| {
                format!(
                    "move collected events {} -> {}",
                    temp_file.display(),
                    inbox_file.display()
                )
            })?;
            chmod_best_effort(inbox_file, 0o644)
        },
    )
}

fn collect_to_inbox_with<A, P>(
    workspace_path: Option<PathBuf>,
    source_paths: &SourcePaths,
    source_settings: &SourceSettings,
    identity: &Identity,
    amp_collector: A,
    publish: P,
) -> Result<CollectReport>
where
    A: FnMut(
        &[PathBuf],
        &mut importers::amp::AmpCollectorState,
        &ImportIdentity,
        &mut dyn Write,
    ) -> Result<ImportReport>,
    P: FnMut(&Path, &Path) -> Result<()>,
{
    collect_to_inbox_with_state_writer(
        workspace_path,
        source_paths,
        source_settings,
        identity,
        amp_collector,
        publish,
        write_collector_state,
    )
}

fn collect_to_inbox_with_state_writer<A, P, S>(
    workspace_path: Option<PathBuf>,
    source_paths: &SourcePaths,
    source_settings: &SourceSettings,
    identity: &Identity,
    mut amp_collector: A,
    mut publish: P,
    mut persist_state: S,
) -> Result<CollectReport>
where
    A: FnMut(
        &[PathBuf],
        &mut importers::amp::AmpCollectorState,
        &ImportIdentity,
        &mut dyn Write,
    ) -> Result<ImportReport>,
    P: FnMut(&Path, &Path) -> Result<()>,
    S: FnMut(&Path, &CollectorState) -> Result<()>,
{
    let workspace = prepare(workspace_path)?;
    write_workspace_profile(&workspace.workspace_dir, identity)?;
    let import_identity =
        ImportIdentity::new(identity.profile_id.clone(), identity.device_id.clone());
    let state_path = workspace
        .workspace_dir
        .join("collector-state")
        .join(format!(
            "{}.json",
            workspace_file_stem(&identity.profile_id)
        ));
    let pending_path = state_path.with_extension("pending.json");
    if pending_path.exists() {
        if let Some((pending, inbox_file)) =
            read_pending_collection(&pending_path, &workspace.inbox_dir)
        {
            let receipt = publication_receipt(&inbox_file);
            if is_regular_file(&inbox_file) || is_regular_file(&receipt) {
                persist_state(&state_path, &pending.state)?;
                fs::remove_file(&pending_path)
                    .with_context(|| format!("remove {}", pending_path.display()))?;
                if is_regular_file(&receipt) {
                    fs::remove_file(&receipt)
                        .with_context(|| format!("remove {}", receipt.display()))?;
                }
                return Ok(CollectReport {
                    status: "ok",
                    workspace_dir: workspace.workspace_dir,
                    inbox_file: Some(inbox_file),
                    events_collected: 0,
                    sources: Vec::new(),
                });
            }
            discard_pending_collection(&pending_path, "stale");
        }
    }
    let state = read_collector_state(&state_path)?;
    let mut staged_state = state.clone();
    let profile_file_stem = workspace_file_stem(&identity.profile_id);
    let temp_file = workspace.inbox_dir.join(format!(
        "{}-{}-{}.tmp",
        profile_file_stem,
        std::process::id(),
        now_ns()
    ));
    let inbox_file = temp_file.with_extension("jsonl");
    let file =
        fs::File::create(&temp_file).with_context(|| format!("create {}", temp_file.display()))?;
    let mut writer = BufWriter::new(file);

    let result = (|| -> Result<CollectReport> {
        let mut sources = Vec::new();
        if source_settings.is_enabled("codex") {
            sources.push(importers::codex::collect_recent(
                Some(source_paths.codex.clone()),
                &import_identity,
                &mut writer,
                collector_since(&state, "codex"),
            )?);
        }
        if source_settings.is_enabled("pi") {
            sources.push(importers::pi::collect_recent(
                Some(source_paths.pi.clone()),
                &import_identity,
                &mut writer,
                collector_since(&state, "pi"),
            )?);
        }
        if source_settings.is_enabled("claude") {
            sources.push(importers::claude::collect_recent(
                Some(source_paths.claude.clone()),
                &import_identity,
                &mut writer,
                collector_since(&state, "claude"),
            )?);
        }
        if source_settings.is_enabled("kanade") {
            sources.push(importers::kanade::collect_recent(
                Some(source_paths.kanade.clone()),
                &import_identity,
                &mut writer,
                collector_since(&state, "kanade"),
            )?);
        }
        if source_settings.is_enabled("amp") {
            sources.push(amp_collector(
                &source_paths.amp,
                &mut staged_state.amp,
                &import_identity,
                &mut writer,
            )?);
        }
        let events_collected = sources
            .iter()
            .map(|source| source.events_projected)
            .sum::<usize>();

        writer
            .flush()
            .with_context(|| format!("flush {}", temp_file.display()))?;
        drop(writer);
        let collected_at_ns = now_ns();
        for source in &sources {
            if source.files_seen > 0 {
                staged_state
                    .sources
                    .insert(source.source.clone(), collected_at_ns);
            }
        }
        if events_collected == 0 {
            persist_state(&state_path, &staged_state)?;
            return Ok(CollectReport {
                status: "ok",
                workspace_dir: workspace.workspace_dir,
                inbox_file: None,
                events_collected,
                sources,
            });
        }

        write_atomic_json(
            &pending_path,
            &PendingCollection {
                inbox_file: inbox_file
                    .file_name()
                    .and_then(|value| value.to_str())
                    .context("generated inbox path has no UTF-8 basename")?
                    .to_owned(),
                state: staged_state.clone(),
            },
        )?;
        if let Err(error) = publish(&temp_file, &inbox_file) {
            let _ = fs::remove_file(&temp_file);
            let _ = fs::remove_file(&inbox_file);
            let _ = fs::remove_file(&pending_path);
            return Err(error);
        }
        persist_state(&state_path, &staged_state)?;
        fs::remove_file(&pending_path)
            .with_context(|| format!("remove {}", pending_path.display()))?;

        Ok(CollectReport {
            status: "ok",
            workspace_dir: workspace.workspace_dir,
            inbox_file: Some(inbox_file),
            events_collected,
            sources,
        })
    })();
    if result.is_err() || temp_file.exists() {
        let _ = fs::remove_file(&temp_file);
    }
    result
}

pub fn drain_inbox(db: &Database, workspace_dir: &Path) -> Result<DrainReport> {
    register_workspace_profiles(db, workspace_dir)?;

    let inbox_dir = workspace_dir.join("inbox");
    if !inbox_dir.exists() {
        return Ok(DrainReport {
            status: "ok",
            files_drained: 0,
            events_projected: 0,
        });
    }

    let mut files = fs::read_dir(&inbox_dir)
        .with_context(|| format!("read {}", inbox_dir.display()))?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            entry.file_type().ok()?.is_file().then(|| entry.path())
        })
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("jsonl"))
        .collect::<Vec<_>>();
    files.sort();

    let mut files_drained = 0usize;
    let mut events_projected = 0usize;

    for path in files {
        let file = fs::File::open(&path).with_context(|| format!("open {}", path.display()))?;
        let reader = BufReader::new(&file);
        let projector = Projector::new(db);
        let tx = db.begin_batch()?;
        let mut projection_cache = ProjectionCache::default();
        let mut touched_session_ids = HashSet::<String>::new();
        let mut touched_run_ids = HashSet::<String>::new();
        let mut events = Vec::<NormalizedEvent>::new();
        let mut supersessions = Vec::<SupersedeCollectedEvents>::new();
        for line in reader.lines() {
            let line = line.with_context(|| format!("read {}", path.display()))?;
            if line.trim().is_empty() {
                continue;
            }
            let value: serde_json::Value = serde_json::from_str(&line)
                .with_context(|| format!("parse collected record from {}", path.display()))?;
            if value.get("record_type").and_then(|value| value.as_str())
                == Some("supersede_source_events_v1")
            {
                supersessions.push(serde_json::from_value(value).with_context(|| {
                    format!("parse collected supersession from {}", path.display())
                })?);
            } else {
                events.push(
                    serde_json::from_value(value).with_context(|| {
                        format!("parse collected event from {}", path.display())
                    })?,
                );
            }
        }
        for supersession in supersessions {
            validate_supersession_provenance(&file, workspace_dir, &supersession.profile_id)?;
            validate_supersession(&supersession, &events)?;
            for (run_id, session_id) in db.retire_source_events(
                &supersession.profile_id,
                &supersession.source,
                &supersession.source_event_ids,
            )? {
                touched_run_ids.insert(run_id);
                if let Some(session_id) = session_id {
                    touched_session_ids.insert(session_id);
                }
            }
        }
        for event in events {
            let projection = projector.project_with_cache(&event, &mut projection_cache)?;
            if let Some(session_id) = projection.session_id {
                touched_session_ids.insert(session_id);
            }
            touched_run_ids.insert(projection.run_id);
            events_projected += 1;
        }
        projector.refresh_run_summaries(touched_run_ids.iter().map(String::as_str))?;
        db.refresh_observed_llm_latency_for_runs(touched_run_ids.iter().map(String::as_str))?;
        projector.refresh_session_summaries(touched_session_ids.iter().map(String::as_str))?;
        tx.commit()?;
        if has_pending_collection_for(workspace_dir, &path)? {
            let receipt = publication_receipt(&path);
            let receipt_file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&receipt)
                .with_context(|| format!("create {}", receipt.display()))?;
            receipt_file
                .sync_all()
                .with_context(|| format!("sync {}", receipt.display()))?;
            fs::File::open(&inbox_dir)
                .and_then(|directory| directory.sync_all())
                .with_context(|| format!("sync {}", inbox_dir.display()))?;
        }
        fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        files_drained += 1;
    }

    if events_projected > 0 {
        let _ = rollup::refresh_all(db)?;
    }

    Ok(DrainReport {
        status: "ok",
        files_drained,
        events_projected,
    })
}

fn validate_supersession(
    supersession: &SupersedeCollectedEvents,
    events: &[NormalizedEvent],
) -> Result<()> {
    anyhow::ensure!(
        supersession.record_type == "supersede_source_events_v1"
            && supersession.source == "amp"
            && !supersession.profile_id.trim().is_empty()
            && !supersession.source_event_ids.is_empty(),
        "invalid collected supersession scope"
    );
    let ledger_thread_prefixes = events
        .iter()
        .filter(|event| event.profile_id == supersession.profile_id && event.source == "amp")
        .filter_map(|event| event.source_event_id.as_deref())
        .filter_map(|id| id.split_once(":ledger:").map(|(prefix, _)| prefix))
        .collect::<HashSet<_>>();
    anyhow::ensure!(
        supersession.source_event_ids.iter().all(|id| {
            id.split_once(":message:").is_some_and(|(prefix, suffix)| {
                !suffix.is_empty() && ledger_thread_prefixes.contains(prefix)
            })
        }),
        "collected supersession does not match an Amp ledger replacement"
    );
    Ok(())
}

#[cfg(unix)]
fn validate_supersession_provenance(
    inbox_file: &File,
    workspace_dir: &Path,
    profile_id: &str,
) -> Result<()> {
    let inbox_metadata = inbox_file
        .metadata()
        .context("read opened inbox metadata")?;
    anyhow::ensure!(
        inbox_metadata.file_type().is_file() && inbox_metadata.mode() & 0o022 == 0,
        "supersession inbox file does not have safe ownership permissions"
    );
    let owner_uid = inbox_metadata.uid();

    let profile_path = workspace_dir
        .join("profiles")
        .join(format!("{}.json", workspace_file_stem(profile_id)));
    let profile_file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&profile_path)
        .with_context(|| format!("open trusted profile {}", profile_path.display()))?;
    let profile_metadata = profile_file
        .metadata()
        .with_context(|| format!("read opened profile metadata {}", profile_path.display()))?;
    anyhow::ensure!(
        profile_metadata.file_type().is_file()
            && profile_metadata.mode() & 0o022 == 0
            && profile_metadata.uid() == owner_uid,
        "supersession profile manifest does not have matching safe ownership"
    );
    let profile: WorkspaceProfile = serde_json::from_reader(BufReader::new(profile_file))
        .with_context(|| format!("parse trusted profile {}", profile_path.display()))?;
    let declared_uid = profile
        .macos_uid
        .as_deref()
        .context("supersession profile manifest has no macos_uid")?
        .parse::<u32>()
        .context("supersession profile manifest has invalid macos_uid")?;
    anyhow::ensure!(
        profile.profile_id == profile_id && declared_uid == owner_uid,
        "supersession profile identity does not match file ownership"
    );
    Ok(())
}

#[cfg(not(unix))]
fn validate_supersession_provenance(
    _inbox_file: &File,
    _workspace_dir: &Path,
    _profile_id: &str,
) -> Result<()> {
    anyhow::bail!("destructive supersession requires OS-proven file ownership")
}

fn publication_receipt(inbox_file: &Path) -> PathBuf {
    inbox_file.with_extension("receipt")
}

fn is_regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
}

fn read_pending_collection(
    pending_path: &Path,
    inbox_dir: &Path,
) -> Option<(PendingCollection, PathBuf)> {
    if !is_regular_file(pending_path) {
        discard_pending_collection(pending_path, "not a regular file");
        return None;
    }
    let raw = match fs::read(pending_path) {
        Ok(raw) => raw,
        Err(_) => {
            discard_pending_collection(pending_path, "unreadable");
            return None;
        }
    };
    let pending: PendingCollection = match serde_json::from_slice(&raw) {
        Ok(pending) => pending,
        Err(_) => {
            discard_pending_collection(pending_path, "malformed");
            return None;
        }
    };
    let basename = Path::new(&pending.inbox_file);
    let valid_basename = basename.components().count() == 1
        && basename.file_name().is_some()
        && basename.extension().and_then(|value| value.to_str()) == Some("jsonl");
    if !valid_basename {
        discard_pending_collection(pending_path, "invalid inbox basename");
        return None;
    }
    let inbox_file = inbox_dir.join(basename);
    Some((pending, inbox_file))
}

fn discard_pending_collection(path: &Path, reason: &'static str) {
    tracing::warn!(pending_file = %path.display(), reason, "discarding unusable pending collection");
    let _ = fs::remove_file(path);
}

fn has_pending_collection_for(workspace_dir: &Path, inbox_file: &Path) -> Result<bool> {
    let state_dir = workspace_dir.join("collector-state");
    if !state_dir.exists() {
        return Ok(false);
    }
    for entry in
        fs::read_dir(&state_dir).with_context(|| format!("read {}", state_dir.display()))?
    {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json")
            || !path
                .file_stem()
                .and_then(|value| value.to_str())
                .is_some_and(|value| value.ends_with(".pending"))
        {
            continue;
        }
        let Some((_, pending_inbox_file)) =
            read_pending_collection(&path, &workspace_dir.join("inbox"))
        else {
            continue;
        };
        let receipt = publication_receipt(&pending_inbox_file);
        if pending_inbox_file == inbox_file {
            return Ok(true);
        }
        if !is_regular_file(&pending_inbox_file) && !is_regular_file(&receipt) {
            discard_pending_collection(&path, "stale");
        }
    }
    Ok(false)
}

fn write_workspace_profile(workspace_dir: &Path, identity: &Identity) -> Result<()> {
    let profiles_dir = workspace_dir.join("profiles");
    fs::create_dir_all(&profiles_dir)
        .with_context(|| format!("create {}", profiles_dir.display()))?;
    let path = profiles_dir.join(format!(
        "{}.json",
        workspace_file_stem(&identity.profile_id)
    ));
    let profile = WorkspaceProfile {
        device_id: identity.device_id.clone(),
        device_label: identity.device_label.clone(),
        profile_id: identity.profile_id.clone(),
        profile_label: identity.profile_label.clone(),
        macos_uid: identity.macos_uid.clone(),
        macos_username: identity.macos_username.clone(),
        updated_at_ns: now_ns(),
    };
    let raw = serde_json::to_string_pretty(&profile)?;
    write_profile_manifest(&path, format!("{raw}\n").as_bytes())
}

#[cfg(unix)]
fn write_profile_manifest(path: &Path, raw: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("open profile manifest {}", path.display()))?;
    file.write_all(raw)
        .with_context(|| format!("write {}", path.display()))?;
    file.set_permissions(fs::Permissions::from_mode(0o644))
        .with_context(|| format!("chmod {}", path.display()))
}

#[cfg(not(unix))]
fn write_profile_manifest(path: &Path, raw: &[u8]) -> Result<()> {
    fs::write(path, raw).with_context(|| format!("write {}", path.display()))
}

fn register_workspace_profiles(db: &Database, workspace_dir: &Path) -> Result<()> {
    let profiles_dir = workspace_dir.join("profiles");
    if !profiles_dir.exists() {
        return Ok(());
    }

    for entry in
        fs::read_dir(&profiles_dir).with_context(|| format!("read {}", profiles_dir.display()))?
    {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let raw = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let profile: WorkspaceProfile =
            serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
        db.register_identity(&Identity {
            device_id: profile.device_id,
            device_label: profile.device_label,
            profile_id: profile.profile_id,
            profile_label: profile.profile_label,
            macos_uid: profile.macos_uid,
            macos_username: profile.macos_username,
        })?;
    }

    Ok(())
}

fn collector_since(state: &CollectorState, source: &str) -> i64 {
    state
        .sources
        .get(source)
        .copied()
        .map(|value| value.saturating_sub(COLLECT_WATERMARK_OVERLAP_NS))
        .unwrap_or_default()
}

fn read_collector_state(path: &Path) -> Result<CollectorState> {
    if !path.exists() {
        return Ok(CollectorState::default());
    }
    let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))
}

fn write_collector_state(path: &Path, state: &CollectorState) -> Result<()> {
    write_atomic_json(path, state)?;
    chmod_best_effort(path, 0o666)
}

fn write_atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let raw = serde_json::to_string_pretty(value)?;
    let parent = path
        .parent()
        .context("collector state path has no parent")?;
    let stem = path.file_name().and_then(|v| v.to_str()).unwrap_or("state");
    let mut attempt = 0u32;
    let (temp_path, mut file) = loop {
        let candidate = parent.join(format!(
            ".{stem}.{}-{}-{attempt}.tmp",
            std::process::id(),
            now_ns()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => break (candidate, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => attempt += 1,
            Err(error) => {
                return Err(error).with_context(|| format!("create {}", candidate.display()));
            }
        }
    };
    let result = (|| -> Result<()> {
        file.write_all(format!("{raw}\n").as_bytes())
            .with_context(|| format!("write {}", temp_path.display()))?;
        file.flush()
            .with_context(|| format!("flush {}", temp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("sync {}", temp_path.display()))?;
        drop(file);
        fs::rename(&temp_path, path)
            .with_context(|| format!("rename {} -> {}", temp_path.display(), path.display()))?;
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("sync {}", parent.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn workspace_file_stem(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        format!("profile_{}", &stable_hash(value)[..16])
    } else {
        sanitized
    }
}

fn sqlite_table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

#[cfg(unix)]
fn chmod_best_effort(path: &Path, mode: u32) -> Result<()> {
    let mut permissions = fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .permissions();
    permissions.set_mode(mode);
    match fs::set_permissions(path, permissions) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Ok(()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "set permissions on {}. Run workspace repair from an admin account if this path is owned by another user",
                path.display()
            )
        }),
    }
}

#[cfg(not(unix))]
fn chmod_best_effort(path: &Path, _mode: u32) -> Result<()> {
    fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{io::Write, path::PathBuf};

    use anyhow::Result;
    use rusqlite::Connection;

    use crate::{
        config::{Identity, SourcePaths, SourceSettings},
        db::Database,
    };

    use super::{
        collect_to_inbox, collect_to_inbox_with, drain_inbox, prepare, seed_pricing_cache,
    };

    fn injected_amp_report(writer: &mut dyn Write) -> Result<crate::importers::ImportReport> {
        writer.write_all(b"{}\n")?;
        Ok(crate::importers::ImportReport {
            source: "amp".into(),
            source_id: "test:amp".into(),
            root_path: PathBuf::from("<test>"),
            files_seen: 1,
            files_imported: 1,
            files_skipped: 0,
            events_projected: 1,
            source_bytes_scanned: 3,
            shirabe_bytes_written: 0,
            mode: Some("cli"),
            threads_enumerated: Some(1),
            threads_exported: Some(1),
            unchanged_threads_skipped: Some(0),
            timings: Vec::new(),
            warnings: Vec::new(),
        })
    }

    fn empty_amp_report() -> crate::importers::ImportReport {
        crate::importers::ImportReport {
            source: "amp".into(),
            source_id: "test:amp".into(),
            root_path: PathBuf::from("<test>"),
            files_seen: 0,
            files_imported: 0,
            files_skipped: 0,
            events_projected: 0,
            source_bytes_scanned: 0,
            shirabe_bytes_written: 0,
            mode: Some("cli"),
            threads_enumerated: Some(0),
            threads_exported: Some(0),
            unchanged_threads_skipped: Some(0),
            timings: Vec::new(),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn collector_state_without_amp_field_remains_compatible() -> Result<()> {
        let state: super::CollectorState = serde_json::from_str(r#"{"sources":{"codex":123}}"#)?;
        assert_eq!(state.sources.get("codex"), Some(&123));
        assert!(state.amp.threads.is_empty());
        Ok(())
    }

    #[test]
    fn disabled_amp_does_not_invoke_collector_or_change_amp_state_or_watermark() -> Result<()> {
        let root = std::env::temp_dir().join(format!("shirabe-disabled-amp-{}", super::now_ns()));
        let workspace = prepare(Some(root.clone()))?;
        let identity = test_identity("disabled_amp");
        let state_path = workspace
            .workspace_dir
            .join("collector-state/disabled_amp.json");
        let initial = r#"{"sources":{"codex":11},"amp":{"threads":{"T-old":{"message_count":1,"updated_at_ns":2,"fingerprint":"old","event_count":3,"status":"imported"}}}}"#;
        std::fs::write(&state_path, initial)?;
        let settings = SourceSettings::from_values(vec!["amp".into(), "codex".into()], None);
        let paths = test_source_paths(&root.join("missing"));
        let report = collect_to_inbox_with(
            Some(root.clone()),
            &paths,
            &settings,
            &identity,
            |_, _, _, _| panic!("disabled Amp collector was invoked"),
            |from, to| {
                std::fs::rename(from, to)?;
                Ok(())
            },
        )?;
        assert!(report.sources.iter().all(|source| source.source != "amp"));
        let state: super::CollectorState = serde_json::from_slice(&std::fs::read(&state_path)?)?;
        assert_eq!(state.sources.len(), 1);
        assert!(!state.sources.contains_key("amp"));
        assert_eq!(state.amp.threads.len(), 1);
        assert_eq!(
            state.amp.threads["T-old"].fingerprint.as_deref(),
            Some("old")
        );
        let _ = std::fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn publish_failure_does_not_advance_amp_state_and_next_collection_retries() -> Result<()> {
        let root =
            std::env::temp_dir().join(format!("shirabe-publish-failure-{}", super::now_ns()));
        let workspace = prepare(Some(root.clone()))?;
        let identity = test_identity("publish_failure");
        let state_path = workspace
            .workspace_dir
            .join("collector-state/publish_failure.json");
        let initial = super::CollectorState::default();
        super::write_collector_state(&state_path, &initial)?;
        let before = std::fs::read(&state_path)?;
        let settings = SourceSettings::from_values(Vec::new(), None);
        let paths = test_source_paths(&root.join("missing"));
        let mut invocations = 0;
        let failed = collect_to_inbox_with(
            Some(root.clone()),
            &paths,
            &settings,
            &identity,
            |_, state, _, writer| {
                invocations += 1;
                state.threads.insert(
                    "T-retry".into(),
                    crate::importers::amp::AmpCollectorThreadState {
                        message_count: 1,
                        updated_at_ns: 2,
                        fingerprint: Some("new".into()),
                        event_count: 1,
                        status: "imported".into(),
                    },
                );
                injected_amp_report(writer)
            },
            |_, _| anyhow::bail!("forced publish failure"),
        );
        assert!(failed.is_err());
        assert_eq!(invocations, 1);
        assert_eq!(std::fs::read(&state_path)?, before);
        assert!(std::fs::read_dir(&workspace.inbox_dir)?.next().is_none());

        let succeeded = collect_to_inbox_with(
            Some(root.clone()),
            &paths,
            &settings,
            &identity,
            |_, state, _, writer| {
                invocations += 1;
                assert!(!state.threads.contains_key("T-retry"));
                state.threads.insert(
                    "T-retry".into(),
                    crate::importers::amp::AmpCollectorThreadState {
                        message_count: 1,
                        updated_at_ns: 2,
                        fingerprint: Some("new".into()),
                        event_count: 1,
                        status: "imported".into(),
                    },
                );
                injected_amp_report(writer)
            },
            |from, to| {
                std::fs::rename(from, to)?;
                Ok(())
            },
        )?;
        assert_eq!(invocations, 2);
        assert_eq!(succeeded.events_collected, 1);
        let state: super::CollectorState = serde_json::from_slice(&std::fs::read(&state_path)?)?;
        assert_eq!(state.amp.threads["T-retry"].status, "imported");
        let _ = std::fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn published_inbox_recovers_state_without_replaying_collection() -> Result<()> {
        let root = std::env::temp_dir().join(format!("shirabe-state-recovery-{}", super::now_ns()));
        let workspace = prepare(Some(root.clone()))?;
        let identity = test_identity("state_recovery");
        let settings = SourceSettings::from_values(Vec::new(), None);
        let paths = test_source_paths(&root.join("missing"));
        let mut collections = 0;
        let mut fail_state = true;

        let first = super::collect_to_inbox_with_state_writer(
            Some(root.clone()),
            &paths,
            &settings,
            &identity,
            |_, state, _, writer| {
                collections += 1;
                state.threads.insert(
                    "T-once".into(),
                    crate::importers::amp::AmpCollectorThreadState {
                        message_count: 1,
                        updated_at_ns: 2,
                        fingerprint: Some("once".into()),
                        event_count: 1,
                        status: "imported".into(),
                    },
                );
                injected_amp_report(writer)
            },
            |from, to| {
                std::fs::rename(from, to)?;
                Ok(())
            },
            |path, state| {
                if fail_state
                    && path.file_name().and_then(|v| v.to_str()) == Some("state_recovery.json")
                {
                    fail_state = false;
                    anyhow::bail!("forced state persistence failure");
                }
                super::write_collector_state(path, state)
            },
        );
        assert!(first.is_err());
        assert_eq!(
            std::fs::read_dir(&workspace.inbox_dir)?
                .filter_map(Result::ok)
                .filter(|entry| entry.path().extension().and_then(|v| v.to_str()) == Some("jsonl"))
                .count(),
            1
        );

        let second = super::collect_to_inbox_with_state_writer(
            Some(root.clone()),
            &paths,
            &settings,
            &identity,
            |_, _, _, _| {
                collections += 1;
                anyhow::bail!("must not replay collection")
            },
            |_, _| anyhow::bail!("must not publish again"),
            |path, state| super::write_collector_state(path, state),
        )?;
        assert_eq!(collections, 1);
        assert_eq!(second.events_collected, 0);
        let state: super::CollectorState = serde_json::from_slice(&std::fs::read(
            workspace
                .workspace_dir
                .join("collector-state/state_recovery.json"),
        )?)?;
        assert!(state.amp.threads.contains_key("T-once"));
        let _ = std::fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn malicious_pending_path_cannot_inject_state_or_remove_external_file() -> Result<()> {
        let root =
            std::env::temp_dir().join(format!("shirabe-malicious-pending-{}", super::now_ns()));
        let workspace = prepare(Some(root.clone()))?;
        let identity = test_identity("malicious");
        let external = root.with_extension("jsonl");
        std::fs::write(&external, b"do not touch")?;
        let pending_path = workspace
            .workspace_dir
            .join("collector-state/malicious.pending.json");
        std::fs::write(
            &pending_path,
            serde_json::to_vec(&serde_json::json!({
                "inbox_file": external,
                "state": {"sources": {"injected": 42}}
            }))?,
        )?;

        let report = collect_to_inbox_with(
            Some(root.clone()),
            &test_source_paths(&root.join("missing")),
            &SourceSettings::from_values(Vec::new(), None),
            &identity,
            |_, _, _, _| Ok(empty_amp_report()),
            |_, _| unreachable!(),
        )?;

        assert_eq!(report.events_collected, 0);
        assert_eq!(std::fs::read(&external)?, b"do not touch");
        assert!(!pending_path.exists());
        let state: super::CollectorState = serde_json::from_slice(&std::fs::read(
            workspace
                .workspace_dir
                .join("collector-state/malicious.json"),
        )?)?;
        assert!(!state.sources.contains_key("injected"));
        let _ = std::fs::remove_file(external);
        let _ = std::fs::remove_dir_all(root);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_pending_state_cannot_be_used_for_recovery() -> Result<()> {
        let root =
            std::env::temp_dir().join(format!("shirabe-pending-symlink-{}", super::now_ns()));
        let workspace = prepare(Some(root.clone()))?;
        let inbox_file = workspace.inbox_dir.join("published.jsonl");
        std::fs::write(&inbox_file, b"published")?;
        let external_pending = root.join("external-pending.json");
        let external_contents =
            br#"{"inbox_file":"published.jsonl","state":{"sources":{"injected":42}}}"#;
        std::fs::write(&external_pending, external_contents)?;
        let pending_path = workspace
            .workspace_dir
            .join("collector-state/symlink.pending.json");
        std::os::unix::fs::symlink(&external_pending, &pending_path)?;

        collect_to_inbox_with(
            Some(root.clone()),
            &test_source_paths(&root.join("missing")),
            &SourceSettings::from_values(Vec::new(), None),
            &test_identity("symlink"),
            |_, _, _, _| Ok(empty_amp_report()),
            |_, _| unreachable!(),
        )?;

        let state: super::CollectorState = serde_json::from_slice(&std::fs::read(
            workspace.workspace_dir.join("collector-state/symlink.json"),
        )?)?;
        assert!(!state.sources.contains_key("injected"));
        assert_eq!(std::fs::read(&external_pending)?, external_contents);
        let _ = std::fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn malformed_and_stale_pending_files_do_not_block_future_collection() -> Result<()> {
        for (profile, pending) in [
            ("malformed", b"not json".as_slice()),
            (
                "stale",
                br#"{"inbox_file":"missing.jsonl","state":{"sources":{"stale":1}}}"#.as_slice(),
            ),
        ] {
            let root =
                std::env::temp_dir().join(format!("shirabe-{profile}-pending-{}", super::now_ns()));
            let workspace = prepare(Some(root.clone()))?;
            let pending_path = workspace
                .workspace_dir
                .join(format!("collector-state/{profile}.pending.json"));
            std::fs::write(&pending_path, pending)?;
            let mut collected = false;
            collect_to_inbox_with(
                Some(root.clone()),
                &test_source_paths(&root.join("missing")),
                &SourceSettings::from_values(Vec::new(), None),
                &test_identity(profile),
                |_, _, _, _| {
                    collected = true;
                    Ok(empty_amp_report())
                },
                |_, _| unreachable!(),
            )?;
            assert!(collected);
            assert!(!pending_path.exists());
            let _ = std::fs::remove_dir_all(root);
        }
        Ok(())
    }

    #[test]
    fn malformed_pending_does_not_block_unrelated_inbox_drain() -> Result<()> {
        let root =
            std::env::temp_dir().join(format!("shirabe-drain-malformed-{}", super::now_ns()));
        let workspace = prepare(Some(root.clone()))?;
        std::fs::write(
            workspace
                .workspace_dir
                .join("collector-state/bad.pending.json"),
            b"{broken",
        )?;
        let legacy_inbox = workspace.inbox_dir.join("unrelated.jsonl");
        std::fs::write(&legacy_inbox, b"")?;
        #[cfg(unix)]
        std::fs::set_permissions(
            &legacy_inbox,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o666),
        )?;
        let db = Database::open(&workspace.catalog_db)?;
        db.migrate()?;

        let report = drain_inbox(&db, &workspace.workspace_dir)?;

        assert_eq!(report.files_drained, 1);
        assert!(!workspace.inbox_dir.join("unrelated.jsonl").exists());
        let _ = std::fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn collector_error_removes_temporary_inbox_file() -> Result<()> {
        let root =
            std::env::temp_dir().join(format!("shirabe-collector-cleanup-{}", super::now_ns()));
        let workspace = prepare(Some(root.clone()))?;
        let result = collect_to_inbox_with(
            Some(root.clone()),
            &test_source_paths(&root.join("missing")),
            &SourceSettings::from_values(Vec::new(), None),
            &test_identity("cleanup"),
            |_, _, _, writer| {
                writer.write_all(b"partial")?;
                anyhow::bail!("collector failed")
            },
            |_, _| unreachable!(),
        );
        assert!(result.is_err());
        assert!(std::fs::read_dir(&workspace.inbox_dir)?.next().is_none());
        let _ = std::fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn collector_inbox_drains_two_profiles_without_db_writes_from_collectors() -> Result<()> {
        let root =
            std::env::temp_dir().join(format!("shirabe-workspace-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let workspace_dir = root.join("workspace");
        let profile_a_source = root.join("profile-a-codex");
        let profile_b_source = root.join("profile-b-codex");
        std::fs::create_dir_all(&profile_a_source)?;
        std::fs::create_dir_all(&profile_b_source)?;
        write_codex_session(&profile_a_source.join("session.jsonl"), 100)?;
        write_codex_session(&profile_b_source.join("session.jsonl"), 200)?;

        let workspace = prepare(Some(workspace_dir.clone()))?;
        let db = Database::open(&workspace.catalog_db)?;
        db.migrate()?;

        let profile_a = test_identity("profile_a");
        let profile_b = test_identity("profile_b");
        let paths_a = test_source_paths(&profile_a_source);
        let paths_b = test_source_paths(&profile_b_source);

        let settings = SourceSettings::from_values(vec!["amp".into()], None);
        let report_a =
            collect_to_inbox(Some(workspace_dir.clone()), &paths_a, &settings, &profile_a)?;
        let report_b =
            collect_to_inbox(Some(workspace_dir.clone()), &paths_b, &settings, &profile_b)?;
        assert!(report_a.events_collected > 0);
        assert!(report_b.events_collected > 0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            assert_eq!(
                std::fs::metadata(&workspace.inbox_dir)?
                    .permissions()
                    .mode()
                    & 0o7777,
                0o1777
            );
            assert_eq!(
                std::fs::metadata(&workspace.profiles_dir)?
                    .permissions()
                    .mode()
                    & 0o7777,
                0o1777
            );
            for inbox_file in [
                report_a.inbox_file.as_ref().expect("profile A inbox"),
                report_b.inbox_file.as_ref().expect("profile B inbox"),
            ] {
                assert_eq!(
                    std::fs::metadata(inbox_file)?.permissions().mode() & 0o777,
                    0o644
                );
            }
            for profile_id in ["profile_a", "profile_b"] {
                let manifest = workspace.profiles_dir.join(format!("{profile_id}.json"));
                assert_eq!(
                    std::fs::metadata(manifest)?.permissions().mode() & 0o777,
                    0o644
                );
            }
        }

        let pre_drain_conn = Connection::open(&workspace.catalog_db)?;
        let pre_drain_runs: i64 =
            pre_drain_conn.query_row("SELECT COUNT(*) FROM runs", [], |row| row.get(0))?;
        assert_eq!(pre_drain_runs, 0);

        let drain = drain_inbox(&db, &workspace_dir)?;
        assert_eq!(drain.files_drained, 2);
        assert!(drain.events_projected >= 2);

        let conn = Connection::open(&workspace.catalog_db)?;
        let totals: (i64, i64, i64) = conn.query_row(
            "SELECT
                (SELECT COUNT(*) FROM sessions),
                (SELECT COUNT(*) FROM runs),
                (SELECT COALESCE(SUM(input_tokens), 0) FROM llm_calls)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(totals, (2, 2, 300));

        let profile_a_tokens: i64 = conn.query_row(
            "SELECT COALESCE(SUM(input_tokens), 0)
             FROM usage_source_rollups
             WHERE bucket = 'month' AND profile_id = 'profile_a'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(profile_a_tokens, 100);

        let profile_b_metadata: (String, String) = conn.query_row(
            "SELECT device_id, profile_label FROM profiles WHERE profile_id = 'profile_b'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(
            profile_b_metadata,
            ("device_1".to_string(), "profile_b".to_string())
        );

        let _ = std::fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn shared_workspace_ledger_replaces_previously_drained_amp_message_usage() -> Result<()> {
        let root = std::env::temp_dir().join(format!("shirabe-amp-replace-{}", super::now_ns()));
        let workspace = prepare(Some(root.clone()))?;
        let source = root.join("amp-thread.json");
        let identity = test_identity("amp_replace");
        let settings = SourceSettings::from_values(Vec::new(), None);
        let paths = test_source_paths(&root.join("missing"));
        let db = Database::open(&workspace.catalog_db)?;
        db.migrate()?;
        std::fs::write(
            &source,
            r#"{"id":"T-one","messages":[{"role":"assistant","messageId":"m1","model":"message-model","timestamp":1,"usage":{"inputTokens":10}}]}"#,
        )?;
        collect_to_inbox_with(
            Some(root.clone()),
            &paths,
            &settings,
            &identity,
            |_, _, import_identity, writer| {
                crate::importers::amp::collect_local_recent(
                    std::slice::from_ref(&source),
                    import_identity,
                    writer,
                    i64::MIN,
                )
            },
            |from, to| {
                std::fs::rename(from, to)?;
                Ok(())
            },
        )?;
        drain_inbox(&db, &workspace.workspace_dir)?;

        std::fs::write(
            &source,
            r#"{"id":"T-one","messages":[{"role":"assistant","messageId":"m1","usage":{}}],"usageLedger":{"events":[{"id":"l1","timestamp":2,"model":"ledger-model","tokens":{"input":30}}]}}"#,
        )?;
        collect_to_inbox_with(
            Some(root.clone()),
            &paths,
            &settings,
            &identity,
            |_, _, import_identity, writer| {
                crate::importers::amp::collect_local_recent(
                    std::slice::from_ref(&source),
                    import_identity,
                    writer,
                    i64::MIN,
                )
            },
            |from, to| {
                std::fs::rename(from, to)?;
                Ok(())
            },
        )?;
        drain_inbox(&db, &workspace.workspace_dir)?;

        let values: (i64, i64, String) = db.connection().query_row(
            "SELECT COUNT(*), COALESCE(SUM(input_tokens), 0), MIN(model) FROM llm_calls",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(values, (1, 30, "ledger-model".into()));
        let _ = std::fs::remove_dir_all(root);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn world_writable_forged_supersession_cannot_delete_profile_usage() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!(
            "shirabe-forged-amp-supersession-{}",
            super::now_ns()
        ));
        let workspace = prepare(Some(root.clone()))?;
        let source = root.join("amp-thread.json");
        let identity = test_identity("victim");
        let settings = SourceSettings::from_values(Vec::new(), None);
        let paths = test_source_paths(&root.join("missing"));
        let db = Database::open(&workspace.catalog_db)?;
        db.migrate()?;
        std::fs::write(
            &source,
            r#"{"id":"T-one","messages":[{"role":"assistant","messageId":"m1","model":"message-model","timestamp":1,"usage":{"inputTokens":10}}]}"#,
        )?;
        collect_to_inbox_with(
            Some(root.clone()),
            &paths,
            &settings,
            &identity,
            |_, _, import_identity, writer| {
                crate::importers::amp::collect_local_recent(
                    std::slice::from_ref(&source),
                    import_identity,
                    writer,
                    i64::MIN,
                )
            },
            |from, to| {
                std::fs::rename(from, to)?;
                Ok(())
            },
        )?;
        drain_inbox(&db, &workspace.workspace_dir)?;

        std::fs::write(
            &source,
            r#"{"id":"T-one","messages":[{"role":"assistant","messageId":"m1","usage":{}}],"usageLedger":{"events":[{"id":"l1","timestamp":2,"model":"ledger-model","tokens":{"input":30}}]}}"#,
        )?;
        let forged = collect_to_inbox_with(
            Some(root.clone()),
            &paths,
            &settings,
            &identity,
            |_, _, import_identity, writer| {
                crate::importers::amp::collect_local_recent(
                    std::slice::from_ref(&source),
                    import_identity,
                    writer,
                    i64::MIN,
                )
            },
            |from, to| {
                std::fs::rename(from, to)?;
                Ok(())
            },
        )?
        .inbox_file
        .expect("replacement inbox file");
        std::fs::set_permissions(&forged, std::fs::Permissions::from_mode(0o666))?;

        assert!(drain_inbox(&db, &workspace.workspace_dir).is_err());
        let unchanged: (i64, i64, String) = db.connection().query_row(
            "SELECT COUNT(*), COALESCE(SUM(input_tokens), 0), MIN(model) FROM llm_calls",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(unchanged, (1, 10, "message-model".into()));

        std::fs::set_permissions(&forged, std::fs::Permissions::from_mode(0o644))?;
        drain_inbox(&db, &workspace.workspace_dir)?;
        let replaced: (i64, i64, String) = db.connection().query_row(
            "SELECT COUNT(*), COALESCE(SUM(input_tokens), 0), MIN(model) FROM llm_calls",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(replaced, (1, 30, "ledger-model".into()));
        let _ = std::fs::remove_dir_all(root);
        Ok(())
    }

    fn test_identity(profile_id: &str) -> Identity {
        Identity {
            device_id: "device_1".to_string(),
            device_label: "Test Mac".to_string(),
            profile_id: profile_id.to_string(),
            profile_label: profile_id.to_string(),
            #[cfg(unix)]
            macos_uid: Some(unsafe { libc::geteuid() }.to_string()),
            #[cfg(not(unix))]
            macos_uid: None,
            macos_username: profile_id.to_string(),
        }
    }

    fn test_source_paths(codex: &std::path::Path) -> SourcePaths {
        SourcePaths {
            codex: codex.to_path_buf(),
            pi: PathBuf::from("/tmp/shirabe-missing-pi"),
            claude: PathBuf::from("/tmp/shirabe-missing-claude"),
            kanade: PathBuf::from("/tmp/shirabe-missing-kanade"),
            amp: vec![PathBuf::from("/tmp/shirabe-missing-amp")],
        }
    }

    #[test]
    fn shared_workspace_seeds_pricing_cache_from_local_catalog() -> Result<()> {
        let root =
            std::env::temp_dir().join(format!("shirabe-pricing-seed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root)?;
        let source_path = root.join("local.sqlite");
        let target_path = root.join("shared.sqlite");
        let source_db = Database::open(&source_path)?;
        source_db.migrate()?;
        source_db.connection().execute(
            "INSERT INTO pricing_sources (
                pricing_source_id, source_url, fetched_at_ns, model_count, raw_json, metadata_json
             )
             VALUES ('source_1', 'https://example.test/prices.json', 10, 1, '{}', NULL)",
            [],
        )?;
        source_db.connection().execute(
            "INSERT INTO model_prices (
                model_name, source_id, provider, input_cost_per_token, output_cost_per_token,
                cache_read_input_token_cost, cache_creation_input_token_cost, updated_at_ns, metadata_json
             )
             VALUES ('gpt-test', 'source_1', 'openai', 0.001, 0.002, 0.0001, NULL, 10, NULL)",
            [],
        )?;

        let target_db = Database::open(&target_path)?;
        target_db.migrate()?;
        assert!(seed_pricing_cache(&target_db, &source_path)?);
        assert!(!seed_pricing_cache(&target_db, &source_path)?);

        let count: i64 = target_db.connection().query_row(
            "SELECT COUNT(*) FROM model_prices WHERE model_name = 'gpt-test'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(count, 1);

        target_db.connection().execute(
            "UPDATE pricing_sources SET fetched_at_ns = 20 WHERE pricing_source_id = 'source_1'",
            [],
        )?;
        target_db.connection().execute(
            "UPDATE model_prices
             SET input_cost_per_token = 0.009, updated_at_ns = 20
             WHERE model_name = 'gpt-test'",
            [],
        )?;
        source_db.connection().execute(
            "INSERT INTO model_prices (
                model_name, source_id, provider, input_cost_per_token, output_cost_per_token,
                cache_read_input_token_cost, cache_creation_input_token_cost, updated_at_ns, metadata_json
             )
             VALUES ('gpt-new', 'source_1', 'openai', 0.003, 0.004, NULL, NULL, 10, NULL)",
            [],
        )?;

        assert!(seed_pricing_cache(&target_db, &source_path)?);
        let (fetched_at_ns, test_price, test_updated_at, new_count): (i64, f64, i64, i64) =
            target_db.connection().query_row(
                "SELECT ps.fetched_at_ns, mp.input_cost_per_token, mp.updated_at_ns,
                        (SELECT COUNT(*) FROM model_prices WHERE model_name = 'gpt-new')
                 FROM pricing_sources ps
                 JOIN model_prices mp ON mp.model_name = 'gpt-test'
                 WHERE ps.pricing_source_id = 'source_1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
        assert_eq!(fetched_at_ns, 20);
        assert_eq!(test_price, 0.009);
        assert_eq!(test_updated_at, 20);
        assert_eq!(new_count, 1);
        assert!(!seed_pricing_cache(&target_db, &source_path)?);

        let _ = std::fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn collector_missing_sources_do_not_advance_watermarks() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "shirabe-workspace-missing-source-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let workspace_dir = root.join("workspace");
        let identity = test_identity("missing_profile");
        let paths = test_source_paths(&root.join("missing-codex"));

        let settings = SourceSettings::from_values(vec!["amp".into()], None);
        let report = collect_to_inbox(Some(workspace_dir.clone()), &paths, &settings, &identity)?;

        assert_eq!(report.events_collected, 0);
        assert!(report.inbox_file.is_none());

        let state_path = workspace_dir
            .join("collector-state")
            .join("missing_profile.json");
        let raw = std::fs::read_to_string(state_path)?;
        let state: serde_json::Value = serde_json::from_str(&raw)?;
        assert!(
            state
                .get("sources")
                .and_then(serde_json::Value::as_object)
                .is_some_and(serde_json::Map::is_empty)
        );

        let _ = std::fs::remove_dir_all(root);
        Ok(())
    }

    fn write_codex_session(path: &std::path::Path, input_tokens: i64) -> Result<()> {
        let event_msg = format!(
            r#"{{"timestamp":"2026-01-01T00:00:01.000Z","type":"event_msg","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":{input_tokens},"cached_input_tokens":0,"output_tokens":10,"reasoning_output_tokens":0,"total_tokens":110}},"model_context_window":1000}}}}}}"#
        );
        let content = format!(
            "{}\n{}\n",
            r#"{"timestamp":"2026-01-01T00:00:00.000Z","type":"session_meta","payload":{"id":"same-session","cwd":"/tmp/project","model":"gpt-test","model_provider":"openai"}}"#,
            event_msg
        );
        std::fs::write(path, content)?;
        Ok(())
    }
}
