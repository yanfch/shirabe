use std::{
    collections::{HashMap, HashSet},
    env, fs,
    io::{BufRead, BufReader, BufWriter},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    config::{Identity, SourcePaths},
    db::{Database, now_ns, stable_hash},
    importers::{self, ImportIdentity, ImportReport},
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

#[derive(Debug, Default, Deserialize, Serialize)]
struct CollectorState {
    sources: HashMap<String, i64>,
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
    PathBuf::from("/Users/Shared/Shirabe")
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
    chmod_best_effort(&inbox_dir, 0o777)?;
    chmod_best_effort(&profiles_dir, 0o777)?;
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
    let Some(home) = env::var_os("HOME") else {
        return Ok(false);
    };
    seed_pricing_cache(db, &PathBuf::from(home).join(".shirabe/catalog.sqlite"))
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

    let (source_count, source_updated_at): (i64, i64) = source.query_row(
        "SELECT COUNT(*), COALESCE(MAX(updated_at_ns), 0) FROM model_prices",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if source_count == 0 {
        return Ok(false);
    }

    let target = db.connection();
    let (target_count, target_updated_at): (i64, i64) = target.query_row(
        "SELECT COUNT(*), COALESCE(MAX(updated_at_ns), 0) FROM model_prices",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if target_count >= source_count && target_updated_at >= source_updated_at {
        return Ok(false);
    }

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
    for (pricing_source_id, source_url, fetched_at_ns, model_count, raw_json, metadata_json) in
        pricing_sources
    {
        target.execute(
            "INSERT INTO pricing_sources (
                pricing_source_id, source_url, fetched_at_ns, model_count, raw_json, metadata_json
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(pricing_source_id) DO UPDATE SET
                source_url = excluded.source_url,
                fetched_at_ns = excluded.fetched_at_ns,
                model_count = excluded.model_count,
                raw_json = excluded.raw_json,
                metadata_json = excluded.metadata_json",
            params![
                pricing_source_id,
                source_url,
                fetched_at_ns,
                model_count,
                raw_json,
                metadata_json
            ],
        )?;
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
        target.execute(
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
                metadata_json = excluded.metadata_json",
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
        )?;
    }
    tx.commit()?;
    Ok(true)
}

pub fn collect_to_inbox(
    workspace_path: Option<PathBuf>,
    source_paths: &SourcePaths,
    identity: &Identity,
) -> Result<CollectReport> {
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
    let mut state = read_collector_state(&state_path)?;
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

    let sources = vec![
        importers::codex::collect_recent(
            Some(source_paths.codex.clone()),
            &import_identity,
            &mut writer,
            collector_since(&state, "codex"),
        )?,
        importers::pi::collect_recent(
            Some(source_paths.pi.clone()),
            &import_identity,
            &mut writer,
            collector_since(&state, "pi"),
        )?,
        importers::claude::collect_recent(
            Some(source_paths.claude.clone()),
            &import_identity,
            &mut writer,
            collector_since(&state, "claude"),
        )?,
        importers::kanade::collect_recent(
            Some(source_paths.kanade.clone()),
            &import_identity,
            &mut writer,
            collector_since(&state, "kanade"),
        )?,
    ];
    let events_collected = sources
        .iter()
        .map(|source| source.events_projected)
        .sum::<usize>();

    drop(writer);
    let collected_at_ns = now_ns();
    for source in &sources {
        if source.files_seen > 0 {
            state.sources.insert(source.source.clone(), collected_at_ns);
        }
    }
    write_collector_state(&state_path, &state)?;
    if events_collected == 0 {
        let _ = fs::remove_file(&temp_file);
        return Ok(CollectReport {
            status: "ok",
            workspace_dir: workspace.workspace_dir,
            inbox_file: None,
            events_collected,
            sources,
        });
    }

    fs::rename(&temp_file, &inbox_file).with_context(|| {
        format!(
            "move collected events {} -> {}",
            temp_file.display(),
            inbox_file.display()
        )
    })?;
    chmod_best_effort(&inbox_file, 0o666)?;

    Ok(CollectReport {
        status: "ok",
        workspace_dir: workspace.workspace_dir,
        inbox_file: Some(inbox_file),
        events_collected,
        sources,
    })
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
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("jsonl"))
        .collect::<Vec<_>>();
    files.sort();

    let mut files_drained = 0usize;
    let mut events_projected = 0usize;

    for path in files {
        let file = fs::File::open(&path).with_context(|| format!("open {}", path.display()))?;
        let reader = BufReader::new(file);
        let projector = Projector::new(db);
        let tx = db.begin_batch()?;
        let mut projection_cache = ProjectionCache::default();
        let mut touched_session_ids = HashSet::<String>::new();
        let mut touched_run_ids = HashSet::<String>::new();
        for line in reader.lines() {
            let line = line.with_context(|| format!("read {}", path.display()))?;
            if line.trim().is_empty() {
                continue;
            }
            let event: NormalizedEvent = serde_json::from_str(&line)
                .with_context(|| format!("parse collected event from {}", path.display()))?;
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
    fs::write(&path, format!("{raw}\n")).with_context(|| format!("write {}", path.display()))?;
    chmod_best_effort(&path, 0o666)
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
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let raw = serde_json::to_string_pretty(state)?;
    fs::write(path, format!("{raw}\n")).with_context(|| format!("write {}", path.display()))?;
    chmod_best_effort(path, 0o666)
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use anyhow::Result;
    use rusqlite::Connection;

    use crate::{
        config::{Identity, SourcePaths},
        db::Database,
    };

    use super::{collect_to_inbox, drain_inbox, prepare, seed_pricing_cache};

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

        let report_a = collect_to_inbox(Some(workspace_dir.clone()), &paths_a, &profile_a)?;
        let report_b = collect_to_inbox(Some(workspace_dir.clone()), &paths_b, &profile_b)?;
        assert!(report_a.events_collected > 0);
        assert!(report_b.events_collected > 0);

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

    fn test_identity(profile_id: &str) -> Identity {
        Identity {
            device_id: "device_1".to_string(),
            device_label: "Test Mac".to_string(),
            profile_id: profile_id.to_string(),
            profile_label: profile_id.to_string(),
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

        let report = collect_to_inbox(Some(workspace_dir.clone()), &paths, &identity)?;

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
