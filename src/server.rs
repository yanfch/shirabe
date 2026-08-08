#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::Write,
    net::SocketAddr,
    path::{Path as FsPath, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use tokio::{net::TcpListener, time};
use tower_http::{services::ServeDir, trace::TraceLayer};

use crate::{
    config::{Identity, SourcePaths, SourceSettings, is_shared_workspace_dir},
    db::Database,
    importers::{self, ImportIdentity},
    pricing, rollup, workspace,
};

const AUTO_SYNC_INTERVAL: Duration = Duration::from_secs(5 * 60);
const SYNC_WATERMARK_OVERLAP_NS: i64 = 10 * 60 * 1_000_000_000;
const FALLBACK_RECENT_SYNC_WINDOW_NS: i64 = 36 * 60 * 60 * 1_000_000_000;

#[derive(Debug)]
struct AppState {
    db_path: PathBuf,
    ui_dir: PathBuf,
    source_paths: SourcePaths,
    source_settings: SourceSettings,
    identity: Identity,
    sync: Mutex<SyncStatus>,
}

pub async fn serve(
    db_path: PathBuf,
    bind: String,
    ui_dir: PathBuf,
    source_paths: SourcePaths,
    source_settings: SourceSettings,
    identity: Identity,
) -> Result<()> {
    let _server_lock = acquire_server_lock_if_shared(&db_path)?;
    let addr: SocketAddr = bind
        .parse()
        .with_context(|| format!("parse bind address {bind}"))?;
    for source in source_settings.unknown_sources() {
        tracing::warn!(source, "ignoring unknown disabled source");
    }
    let state = Arc::new(AppState {
        db_path,
        ui_dir,
        source_paths,
        source_settings,
        identity,
        sync: Mutex::new(SyncStatus::default()),
    });
    spawn_auto_sync(state.clone());

    let app = Router::new()
        .route("/api/health", get(health))
        .route("/api/overview", get(overview))
        .route("/api/usage", get(usage))
        .route("/api/menubar", get(menubar))
        .route("/api/workspace", get(workspace_status))
        .route("/api/workspace/repair", post(repair_workspace))
        .route("/api/sync", post(start_sync))
        .route("/api/sync/status", get(sync_status))
        .route("/api/sessions", get(sessions))
        .route("/api/sessions/{id}", get(session_detail))
        .route("/api/runs/{id}", get(run_detail))
        .fallback_service(ServeDir::new(state.ui_dir.clone()))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = TcpListener::bind(addr).await?;
    tracing::info!("serving http://{}", listener.local_addr()?);
    axum::serve(listener, app).await?;

    Ok(())
}

struct ServerLock {
    _path: PathBuf,
    _file: File,
}

impl Drop for ServerLock {
    fn drop(&mut self) {
        let _ = unlock_server_file(&self._file);
    }
}

fn acquire_server_lock_if_shared(db_path: &FsPath) -> Result<Option<ServerLock>> {
    let Some(workspace_dir) = db_path.parent().filter(|path| is_shared_workspace(path)) else {
        return Ok(None);
    };

    fs::create_dir_all(workspace_dir)
        .with_context(|| format!("create {}", workspace_dir.display()))?;
    let lock_path = workspace_dir.join("server.lock");
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .or_else(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(&lock_path)
            } else {
                Err(error)
            }
        })
        .with_context(|| format!("open {}", lock_path.display()))?;
    try_lock_server_file(&file, &lock_path)?;
    file.set_len(0)
        .with_context(|| format!("truncate {}", lock_path.display()))?;
    writeln!(file, "{}", std::process::id())?;
    set_shared_file_mode(&lock_path)?;
    Ok(Some(ServerLock {
        _path: lock_path,
        _file: file,
    }))
}

#[cfg(unix)]
fn set_shared_file_mode(path: &FsPath) -> Result<()> {
    let mut permissions = fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .permissions();
    permissions.set_mode(0o666);
    match fs::set_permissions(path, permissions) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Ok(()),
        Err(error) => Err(error).with_context(|| format!("set permissions on {}", path.display())),
    }
}

#[cfg(not(unix))]
fn set_shared_file_mode(path: &FsPath) -> Result<()> {
    fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn try_lock_server_file(file: &File, lock_path: &FsPath) -> Result<()> {
    unsafe extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }
    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;

    if unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) } == 0 {
        return Ok(());
    }

    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        anyhow::bail!(
            "shared Shirabe server is already running; lock file {} is locked",
            lock_path.display()
        );
    }
    Err(error).with_context(|| format!("lock {}", lock_path.display()))
}

#[cfg(not(unix))]
fn try_lock_server_file(_file: &File, _lock_path: &FsPath) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn unlock_server_file(file: &File) -> Result<()> {
    unsafe extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }
    const LOCK_UN: i32 = 8;

    if unsafe { flock(file.as_raw_fd(), LOCK_UN) } == 0 {
        return Ok(());
    }

    Err(std::io::Error::last_os_error()).context("unlock shared server lock")
}

#[cfg(not(unix))]
fn unlock_server_file(_file: &File) -> Result<()> {
    Ok(())
}

async fn health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(HealthResponse {
        status: "ok",
        database: state.db_path.display().to_string(),
        ui_dir: state.ui_dir.display().to_string(),
    })
}

async fn overview(State(state): State<Arc<AppState>>) -> Response {
    match load_overview(&state.db_path) {
        Ok(response) => Json(response).into_response(),
        Err(error) => api_error(error),
    }
}

async fn usage(State(state): State<Arc<AppState>>, Query(query): Query<UsageQuery>) -> Response {
    let filters = UsageFilters::from_query(query);
    match load_usage(&state.db_path, filters) {
        Ok(response) => Json(response).into_response(),
        Err(error) => api_error(error),
    }
}

async fn menubar(
    State(state): State<Arc<AppState>>,
    Query(query): Query<MenubarQuery>,
) -> Response {
    match load_menubar(
        &state.db_path,
        query,
        current_sync_status(&state),
        &state.identity,
    ) {
        Ok(response) => Json(response).into_response(),
        Err(error) => api_error(error),
    }
}

async fn sync_status(State(state): State<Arc<AppState>>) -> Response {
    Json(current_sync_status(&state)).into_response()
}

async fn workspace_status(State(state): State<Arc<AppState>>) -> Response {
    match load_workspace_status(&state.db_path, &state.identity, false) {
        Ok(response) => Json(response).into_response(),
        Err(error) => api_error(error),
    }
}

async fn repair_workspace(State(state): State<Arc<AppState>>) -> Response {
    match repair_workspace_inner(&state) {
        Ok(response) => Json(response).into_response(),
        Err(error) => api_error(error),
    }
}

async fn start_sync(State(state): State<Arc<AppState>>) -> Response {
    match try_start_sync_job(state.clone()) {
        Some(response) => (StatusCode::ACCEPTED, Json(response)).into_response(),
        None => {
            let sync = current_sync_status(&state);
            (StatusCode::ACCEPTED, Json(sync)).into_response()
        }
    }
}

fn try_start_sync_job(state: Arc<AppState>) -> Option<SyncStatus> {
    let response = {
        let mut sync = state.sync.lock().expect("sync status lock poisoned");
        if sync.state == "running" {
            return None;
        }

        *sync = SyncStatus::running();
        sync.clone()
    };

    let job_state = state.clone();
    tokio::task::spawn_blocking(move || run_sync_job(job_state));

    Some(response)
}

fn spawn_auto_sync(state: Arc<AppState>) {
    tokio::spawn(async move {
        loop {
            let _ = try_start_sync_job(state.clone());
            time::sleep(AUTO_SYNC_INTERVAL).await;
        }
    });
}

async fn sessions(State(state): State<Arc<AppState>>) -> Response {
    match load_sessions(&state.db_path) {
        Ok(response) => Json(response).into_response(),
        Err(error) => api_error(error),
    }
}

async fn session_detail(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match load_session_detail(&state.db_path, &id) {
        Ok(Some(response)) => Json(response).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                status: "error",
                message: format!("session not found: {id}"),
            }),
        )
            .into_response(),
        Err(error) => api_error(error),
    }
}

async fn run_detail(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match load_run_detail(&state.db_path, &id) {
        Ok(Some(response)) => Json(response).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                status: "error",
                message: format!("run not found: {id}"),
            }),
        )
            .into_response(),
        Err(error) => api_error(error),
    }
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
    database: String,
    ui_dir: String,
}

#[derive(Debug, Serialize)]
struct WorkspaceStatusResponse {
    status: &'static str,
    mode: &'static str,
    workspace_dir: String,
    catalog_db: String,
    shared_workspace_dir: String,
    shared_workspace_exists: bool,
    current_profile_id: String,
    current_profile_label: String,
    current_device_id: String,
    current_device_label: String,
    profile_count: i64,
    writable: bool,
    repair_available: bool,
    restart_required: bool,
    issue: Option<String>,
}

#[derive(Debug, Serialize)]
struct OverviewResponse {
    status: &'static str,
    totals: OverviewTotals,
    imports: Vec<ImportSourceSummary>,
    top_signals: Vec<SignalSummary>,
    source_usage: Vec<SourceUsageSummary>,
    recent_sessions: Vec<SessionSummary>,
    recent_runs: Vec<RecentRun>,
    tool_summaries: Vec<ToolSummary>,
    model_summaries: Vec<ModelSummary>,
}

#[derive(Debug, Serialize)]
struct UsageResponse {
    status: &'static str,
    filters: UsageFilters,
    usage_grain: String,
    profile_options: Vec<ProfileOption>,
    source_options: Vec<String>,
    model_options: Vec<String>,
    summary: UsageSummary,
    latency: LatencySummary,
    latency_panel: LatencyPanel,
    latency_buckets: Vec<LatencyBucketSummary>,
    buckets: Vec<UsageBucketSummary>,
    source_usage: Vec<SourceUsageSummary>,
    recent_sessions: Vec<SessionSummary>,
    recent_runs: Vec<RecentRun>,
    tool_summaries: Vec<ToolSummary>,
    tool_failures: Vec<ToolFailureSummary>,
    skill_summaries: Vec<SkillSummary>,
    model_summaries: Vec<ModelSummary>,
}

#[derive(Debug, Serialize)]
struct MenubarResponse {
    status: &'static str,
    range: String,
    profile_options: Vec<ProfileOption>,
    current_profile_id: String,
    summary: UsageSummary,
    latency: LatencySummary,
    latency_panel: LatencyPanel,
    trend: Vec<UsageBucketSummary>,
    source_usage: Vec<SourceUsageSummary>,
    recent_runs: Vec<RecentRun>,
    latest_run: Option<RecentRun>,
    last_updated_at_ns: Option<i64>,
    sync: SyncStatus,
}

#[derive(Debug, Clone, Serialize)]
struct SyncStatus {
    status: &'static str,
    state: String,
    phase: String,
    started_at_ns: Option<i64>,
    finished_at_ns: Option<i64>,
    duration_ms: Option<i64>,
    sources: Vec<SyncSourceReport>,
    rollup: Option<SyncRollupReport>,
    error: Option<String>,
}

impl SyncStatus {
    fn running() -> Self {
        Self {
            status: "ok",
            state: "running".to_string(),
            phase: "queued".to_string(),
            started_at_ns: Some(crate::db::now_ns()),
            finished_at_ns: None,
            duration_ms: None,
            sources: Vec::new(),
            rollup: None,
            error: None,
        }
    }
}

impl Default for SyncStatus {
    fn default() -> Self {
        Self {
            status: "ok",
            state: "idle".to_string(),
            phase: "idle".to_string(),
            started_at_ns: None,
            finished_at_ns: None,
            duration_ms: None,
            sources: Vec::new(),
            rollup: None,
            error: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct SyncSourceReport {
    source: String,
    status: String,
    files_seen: usize,
    files_imported: usize,
    files_skipped: usize,
    events_projected: usize,
    source_bytes_scanned: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    threads_enumerated: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    threads_exported: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unchanged_threads_skipped: Option<usize>,
    elapsed_ms: i64,
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct SyncRollupReport {
    status: String,
    source_rollups: i64,
    model_rollups: i64,
    tool_rollups: i64,
    tool_model_rollups: i64,
    elapsed_ms: i64,
}

#[derive(Debug, Serialize)]
struct SessionListResponse {
    status: &'static str,
    sessions: Vec<SessionSummary>,
}

#[derive(Debug, Serialize)]
struct SessionDetailResponse {
    status: &'static str,
    session: SessionSummary,
    runs: Vec<RecentRun>,
    turns: Vec<TurnSummary>,
    timeline: Vec<SessionTimelineEvent>,
    llm_calls: Vec<LlmCall>,
    tool_calls: Vec<ToolCall>,
    skill_events: Vec<SkillEventRecord>,
    tool_summaries: Vec<ToolSummary>,
    skill_summaries: Vec<SkillSummary>,
    model_summaries: Vec<ModelSummary>,
}

#[derive(Debug, Deserialize)]
struct UsageQuery {
    preset: Option<String>,
    range: Option<String>,
    from: Option<String>,
    to: Option<String>,
    grain: Option<String>,
    profile_id: Option<String>,
    device_id: Option<String>,
    source: Option<String>,
    model: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MenubarQuery {
    range: Option<String>,
    profile_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct UsageFilters {
    preset: String,
    range: String,
    from: Option<String>,
    to: Option<String>,
    grain: String,
    profile_id: Option<String>,
    device_id: Option<String>,
    source: Option<String>,
    model: Option<String>,
}

#[derive(Debug, Clone)]
struct SqlDateRange {
    from_expr: String,
    to_expr: String,
}

impl SqlDateRange {
    fn bucket_key_condition(&self, alias: &str, bucket: &str) -> String {
        if bucket == "month" {
            format!(
                "{alias}.bucket_key >= strftime('%Y-%m', {}) AND {alias}.bucket_key <= strftime('%Y-%m', {}, '-1 day')",
                self.from_expr, self.to_expr
            )
        } else {
            format!(
                "{alias}.bucket_key >= {} AND {alias}.bucket_key < {}",
                self.from_expr, self.to_expr
            )
        }
    }
}

#[derive(Debug, Serialize)]
struct OverviewTotals {
    import_sources: i64,
    import_files: i64,
    imported_files: i64,
    partial_files: i64,
    failed_files: i64,
    sessions: i64,
    runs: i64,
    turns: i64,
    run_steps: i64,
    llm_calls: i64,
    tool_calls: i64,
    failed_tool_calls: i64,
    signals: i64,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    total_cost_usd: f64,
}

#[derive(Debug, Serialize)]
struct UsageSummary {
    sessions: i64,
    runs: i64,
    turns: i64,
    llm_calls: i64,
    tool_calls: i64,
    failed_tool_calls: i64,
    input_tokens: i64,
    output_tokens: i64,
    total_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    cache_hit_rate: Option<f64>,
    tool_failure_rate: Option<f64>,
    total_cost_usd: f64,
}

#[derive(Debug, Clone, Serialize)]
struct UsageBucketSummary {
    bucket_key: String,
    date: String,
    bucket_count: i64,
    sessions: i64,
    runs: i64,
    turns: i64,
    llm_calls: i64,
    tool_calls: i64,
    failed_tool_calls: i64,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    total_cost_usd: f64,
}

#[derive(Debug, Clone, Serialize)]
struct LatencySummary {
    llm_calls: i64,
    observed_calls: i64,
    good_calls: i64,
    subsecond_calls: i64,
    outlier_calls: i64,
    missing_calls: i64,
    p50_response_delay_ns: Option<i64>,
    p90_response_delay_ns: Option<i64>,
    avg_observed_output_tps: Option<f64>,
    p50_observed_output_tps: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
struct LatencyBucketSummary {
    bucket_key: String,
    date: String,
    bucket_count: i64,
    good_calls: i64,
    p50_response_delay_ns: Option<i64>,
    p90_response_delay_ns: Option<i64>,
    avg_observed_output_tps: Option<f64>,
    p50_observed_output_tps: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
struct LatencyPanel {
    mode: String,
    status: String,
    baseline: LatencyBaseline,
    current: Option<LatencyCurrent>,
    slow_hours: Vec<LatencyHour>,
    best_windows: Vec<LatencyWindow>,
    slow_windows: Vec<LatencyWindow>,
    provider_patterns: Vec<ProviderLatencyPattern>,
}

#[derive(Debug, Clone, Serialize)]
struct LatencyBaseline {
    p50_response_delay_ns: Option<i64>,
    good_calls: i64,
    active_good_hours: i64,
}

#[derive(Debug, Clone, Serialize)]
struct LatencyCurrent {
    hour: String,
    p50_response_delay_ns: Option<i64>,
    ratio_to_baseline: Option<f64>,
    good_calls: i64,
    slowest_provider: Option<LatencyProviderNow>,
}

#[derive(Debug, Clone, Serialize)]
struct LatencyProviderNow {
    provider: String,
    p50_response_delay_ns: i64,
    ratio_to_baseline: Option<f64>,
    good_calls: i64,
}

#[derive(Debug, Clone, Serialize)]
struct LatencyHour {
    hour: String,
    p50_response_delay_ns: i64,
    ratio_to_baseline: Option<f64>,
    good_calls: i64,
    is_current: bool,
}

#[derive(Debug, Clone, Serialize)]
struct LatencyWindow {
    label: String,
    p50_response_delay_ns: i64,
    good_calls: i64,
}

#[derive(Debug, Clone, Serialize)]
struct ProviderLatencyPattern {
    provider: String,
    good_calls: i64,
    best_hour: Option<String>,
    best_p50_response_delay_ns: Option<i64>,
    slow_hour: Option<String>,
    slow_p50_response_delay_ns: Option<i64>,
}

const MAX_USAGE_BUCKETS: usize = 72;

#[derive(Debug, Serialize)]
struct ImportSourceSummary {
    source_id: String,
    source: String,
    source_kind: String,
    root_path: String,
    last_scan_ns: Option<i64>,
    parser_version: Option<String>,
    file_count: i64,
    imported_files: i64,
    partial_files: i64,
    failed_files: i64,
    source_bytes: i64,
    event_count: i64,
    warning_count: i64,
}

#[derive(Debug, Serialize)]
struct RecentRun {
    run_id: String,
    source: String,
    kind: String,
    title: Option<String>,
    status: String,
    session_id: Option<String>,
    started_at_ns: i64,
    ended_at_ns: Option<i64>,
    duration_ns: Option<i64>,
    llm_call_count: i64,
    tool_call_count: i64,
    failed_tool_count: i64,
    signal_count: i64,
    primary_signal: Option<String>,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    total_cost_usd: f64,
}

#[derive(Debug, Serialize)]
struct SourceUsageSummary {
    source: String,
    sessions: i64,
    runs: i64,
    turns: i64,
    llm_calls: i64,
    tool_calls: i64,
    failed_tool_calls: i64,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    total_cost_usd: f64,
}

#[derive(Debug, Serialize)]
struct SessionSummary {
    session_id: String,
    source: String,
    kind: String,
    title: Option<String>,
    first_seen_ns: i64,
    last_seen_ns: i64,
    runs: i64,
    turns: i64,
    llm_calls: i64,
    tool_calls: i64,
    failed_tool_calls: i64,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    total_cost_usd: f64,
    models: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ToolSummary {
    tool_name: String,
    calls: i64,
    success_calls: i64,
    failed_calls: i64,
    failure_rate: f64,
    last_called_at_ns: i64,
}

#[derive(Debug, Serialize)]
struct ToolFailureSummary {
    source: String,
    tool_name: String,
    calls: i64,
    success_calls: i64,
    failed_calls: i64,
    failure_rate: f64,
}

#[derive(Debug, Serialize)]
struct ModelSummary {
    model: String,
    calls: i64,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    total_cost_usd: f64,
}

#[derive(Debug, Serialize)]
struct SkillSummary {
    source: String,
    skill_name: String,
    loaded_count: i64,
    invoked_count: i64,
    attributed_count: i64,
    sessions: i64,
    runs: i64,
    last_used_at_ns: i64,
    confidence: f64,
}

#[derive(Debug, Serialize)]
struct TurnSummary {
    turn_id: String,
    run_id: String,
    session_id: Option<String>,
    source: String,
    turn_index: Option<i64>,
    role: Option<String>,
    status: Option<String>,
    started_at_ns: i64,
    ended_at_ns: Option<i64>,
    duration_ns: Option<i64>,
    llm_calls: i64,
    tool_calls: i64,
    failed_tool_calls: i64,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    total_cost_usd: f64,
    models: Vec<String>,
}

#[derive(Debug, Serialize)]
struct SkillEventRecord {
    skill_event_id: String,
    source: String,
    skill_name: String,
    event_type: String,
    confidence: f64,
    session_id: Option<String>,
    run_id: String,
    turn_id: Option<String>,
    source_event_id: Option<String>,
    source_ref: Option<String>,
    occurred_at_ns: i64,
    metadata_json: Option<String>,
}

#[derive(Debug, Serialize)]
struct SessionTimelineEvent {
    event_id: String,
    event_type: String,
    run_id: String,
    turn_id: Option<String>,
    label: String,
    status: String,
    error_type: Option<String>,
    started_at_ns: i64,
    ended_at_ns: Option<i64>,
    duration_ns: Option<i64>,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    total_cost_usd: f64,
    model: Option<String>,
    tool_name: Option<String>,
    skill_name: Option<String>,
    skill_event_type: Option<String>,
}

#[derive(Debug, Serialize)]
struct RunDetailResponse {
    status: &'static str,
    run: RecentRun,
    signals: Vec<RunSignal>,
    timeline: Vec<RunStep>,
    llm_calls: Vec<LlmCall>,
    tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Serialize)]
struct SignalSummary {
    signal_type: String,
    severity: String,
    count: i64,
}

#[derive(Debug, Serialize)]
struct RunSignal {
    signal_id: String,
    signal_type: String,
    severity: String,
    title: String,
    evidence_json: Option<String>,
    suggestion: Option<String>,
    created_at_ns: i64,
}

#[derive(Debug, Serialize)]
struct RunStep {
    step_id: String,
    step_type: String,
    name: String,
    status: String,
    error_type: Option<String>,
    source_event_id: Option<String>,
    source_ref: Option<String>,
    started_at_ns: i64,
    ended_at_ns: Option<i64>,
    duration_ns: Option<i64>,
    order_index: Option<i64>,
    llm_call_id: Option<String>,
    tool_call_id: Option<String>,
    input_tokens: i64,
    output_tokens: i64,
    cost_usd: f64,
    metadata_json: Option<String>,
}

#[derive(Debug, Serialize)]
struct LlmCall {
    llm_call_id: String,
    provider: Option<String>,
    model: Option<String>,
    operation: Option<String>,
    status: String,
    error_type: Option<String>,
    input_tokens: i64,
    output_tokens: i64,
    reasoning_tokens: i64,
    uncached_input_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    cache_ratio: Option<f64>,
    model_context_window: Option<i64>,
    context_window_percent: Option<f64>,
    total_cost_usd: f64,
    pricing_status: Option<String>,
    cost_confidence: Option<String>,
    started_at_ns: i64,
    ended_at_ns: Option<i64>,
    duration_ns: Option<i64>,
    metadata_json: Option<String>,
}

#[derive(Debug, Serialize)]
struct ToolCall {
    tool_call_id: String,
    tool_name: String,
    status: String,
    error_type: Option<String>,
    output_bytes: Option<i64>,
    started_at_ns: i64,
    ended_at_ns: Option<i64>,
    duration_ns: Option<i64>,
    metadata_json: Option<String>,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    status: &'static str,
    message: String,
}

#[derive(Debug, Clone, Serialize)]
struct ProfileOption {
    profile_id: String,
    profile_label: String,
    device_id: String,
    device_label: Option<String>,
    macos_username: Option<String>,
    is_current: bool,
}

const LLM_COST_SQL: &str = "CASE
    WHEN l.total_cost_usd > 0 THEN l.total_cost_usd
    ELSE
        ((CASE
            WHEN l.uncached_input_tokens > 0 THEN l.uncached_input_tokens
            ELSE MAX(l.input_tokens - l.cache_read_tokens - l.cache_write_tokens, 0)
        END) * COALESCE(mp.input_cost_per_token, 0)) +
        (l.output_tokens * COALESCE(mp.output_cost_per_token, 0)) +
        (l.cache_read_tokens * COALESCE(mp.cache_read_input_token_cost, 0)) +
        (l.cache_write_tokens * COALESCE(mp.cache_creation_input_token_cost, 0))
    END";

const MODEL_ROLLUP_COST_SQL: &str = "CASE
    WHEN m.total_cost_usd > 0 THEN m.total_cost_usd
    ELSE
        ((CASE
            WHEN m.uncached_input_tokens > 0 THEN m.uncached_input_tokens
            ELSE MAX(m.input_tokens - m.cache_read_tokens - m.cache_write_tokens, 0)
        END) * COALESCE(mp.input_cost_per_token, 0)) +
        (m.output_tokens * COALESCE(mp.output_cost_per_token, 0)) +
        (m.cache_read_tokens * COALESCE(mp.cache_read_input_token_cost, 0)) +
        (m.cache_write_tokens * COALESCE(mp.cache_creation_input_token_cost, 0))
    END";

impl UsageFilters {
    fn all() -> Self {
        Self {
            preset: "all".to_string(),
            range: "all".to_string(),
            from: None,
            to: None,
            grain: "auto".to_string(),
            profile_id: None,
            device_id: None,
            source: None,
            model: None,
        }
    }

    fn usage_grain(&self) -> &'static str {
        match self.grain.as_str() {
            "day" => "day",
            "month" => "month",
            _ => self.auto_grain(),
        }
    }

    fn auto_grain(&self) -> &'static str {
        if self.preset == "all" {
            return "month";
        }
        if self.preset == "custom"
            && let Some(days) = self.custom_span_days()
            && days > 90
        {
            return "month";
        }
        "day"
    }

    fn custom_span_days(&self) -> Option<i64> {
        let from = self.from.as_deref()?;
        let to = self.to.as_deref()?;
        Some(days_since_epoch(to)? - days_since_epoch(from)?)
    }

    fn date_range_sql(&self) -> Option<SqlDateRange> {
        match self.preset.as_str() {
            "today" => Some(SqlDateRange {
                from_expr: "date('now', 'localtime', 'start of day')".to_string(),
                to_expr: "date('now', 'localtime', 'start of day', '+1 day')".to_string(),
            }),
            "7d" => Some(SqlDateRange {
                from_expr: "date('now', 'localtime', 'start of day', '-6 days')".to_string(),
                to_expr: "date('now', 'localtime', 'start of day', '+1 day')".to_string(),
            }),
            "30d" => Some(SqlDateRange {
                from_expr: "date('now', 'localtime', 'start of day', '-29 days')".to_string(),
                to_expr: "date('now', 'localtime', 'start of day', '+1 day')".to_string(),
            }),
            "this_month" => Some(SqlDateRange {
                from_expr: "date('now', 'localtime', 'start of month')".to_string(),
                to_expr: "date('now', 'localtime', 'start of month', '+1 month')".to_string(),
            }),
            "last_month" => Some(SqlDateRange {
                from_expr: "date('now', 'localtime', 'start of month', '-1 month')".to_string(),
                to_expr: "date('now', 'localtime', 'start of month')".to_string(),
            }),
            "custom" => Some(SqlDateRange {
                from_expr: sql_literal(self.from.as_deref()?),
                to_expr: sql_literal(self.to.as_deref()?),
            }),
            _ => None,
        }
    }

    fn stats_rollup_grain(&self) -> &'static str {
        if self.date_range_sql().is_some() {
            "day"
        } else {
            "month"
        }
    }

    fn chart_rollup_grain(&self) -> &'static str {
        if self.usage_grain() == "month" && self.date_range_sql().is_some() {
            "day"
        } else {
            self.usage_grain()
        }
    }

    fn chart_bucket_key_expr(&self, alias: &str, rollup_grain: &str) -> String {
        if self.usage_grain() == "month" && rollup_grain == "day" {
            format!("substr({alias}.bucket_key, 1, 7)")
        } else {
            format!("{alias}.bucket_key")
        }
    }

    fn rollup_key_where_for(&self, alias: &str, bucket: &str) -> String {
        let mut conditions = vec![format!("{alias}.bucket = {}", sql_literal(bucket))];
        if let Some(range) = self.date_range_sql() {
            conditions.push(range.bucket_key_condition(alias, bucket));
        }
        join_conditions(conditions)
    }

    fn rollup_source_where(&self, alias: &str) -> String {
        self.rollup_source_where_for(alias, self.stats_rollup_grain())
    }

    fn rollup_source_where_for(&self, alias: &str, bucket: &str) -> String {
        let mut conditions = vec![self.rollup_key_where_for(alias, bucket)];
        if let Some(profile) = self.profile_condition(alias) {
            conditions.push(profile);
        }
        if let Some(device) = self.device_condition(alias) {
            conditions.push(device);
        }
        if let Some(source) = &self.source {
            conditions.push(format!("{alias}.source = {}", sql_literal(source)));
        }
        join_conditions(conditions)
    }

    fn rollup_model_where(&self, alias: &str) -> String {
        self.rollup_model_where_for(alias, self.stats_rollup_grain())
    }

    fn rollup_model_where_for(&self, alias: &str, bucket: &str) -> String {
        let mut conditions = vec![self.rollup_source_where_for(alias, bucket)];
        if let Some(model) = &self.model {
            conditions.push(format!("{alias}.model = {}", sql_literal(model)));
        }
        join_conditions(conditions)
    }

    fn from_query(query: UsageQuery) -> Self {
        let from = normalize_date_value(query.from);
        let to = normalize_date_value(query.to);
        let has_valid_custom_range = from
            .as_deref()
            .zip(to.as_deref())
            .is_some_and(|(from, to)| from < to);
        let requested_preset = query.preset.as_deref().or(query.range.as_deref());
        let preset = if has_valid_custom_range && requested_preset == Some("custom") {
            "custom"
        } else {
            match requested_preset {
                Some("today") => "today",
                Some("7d") => "7d",
                Some("30d") => "30d",
                Some("this_month") | Some("mtd") => "this_month",
                Some("last_month") => "last_month",
                Some("all") => "all",
                _ if has_valid_custom_range => "custom",
                _ => "30d",
            }
        }
        .to_string();
        let grain = match query.grain.as_deref() {
            Some("day") => "day",
            Some("month") => "month",
            _ => "auto",
        }
        .to_string();
        let (from, to) = if preset == "custom" {
            (from, to)
        } else {
            (None, None)
        };

        Self {
            preset: preset.clone(),
            range: preset,
            from,
            to,
            grain,
            profile_id: normalize_filter_value(query.profile_id),
            device_id: normalize_filter_value(query.device_id),
            source: normalize_filter_value(query.source),
            model: normalize_model_filter_value(query.model),
        }
    }

    fn session_where(&self, alias: &str) -> String {
        let mut conditions = vec![self.date_predicate(&format!("{alias}.last_seen_ns"))];
        if let Some(source) = self.source_condition(alias) {
            conditions.push(source);
        }
        if let Some(profile) = self.profile_condition(alias) {
            conditions.push(profile);
        }
        if let Some(device) = self.device_condition(alias) {
            conditions.push(device);
        }
        if let Some(model) = self.model_exists_for_session(alias) {
            conditions.push(model);
        }
        join_conditions(conditions)
    }

    fn run_where(&self, alias: &str) -> String {
        let mut conditions = vec![self.date_predicate(&format!("{alias}.started_at_ns"))];
        if let Some(source) = self.source_condition(alias) {
            conditions.push(source);
        }
        if let Some(profile) = self.profile_condition(alias) {
            conditions.push(profile);
        }
        if let Some(device) = self.device_condition(alias) {
            conditions.push(device);
        }
        if let Some(model) = self.model_exists_for_run(alias) {
            conditions.push(model);
        }
        join_conditions(conditions)
    }

    fn turn_where(&self, alias: &str) -> String {
        let mut conditions = vec![self.date_predicate(&format!("{alias}.started_at_ns"))];
        if let Some(source) = self.source_condition(alias) {
            conditions.push(source);
        }
        if let Some(profile) = self.profile_condition(alias) {
            conditions.push(profile);
        }
        if let Some(device) = self.device_condition(alias) {
            conditions.push(device);
        }
        if let Some(model) = self.model_exists_for_turn(alias) {
            conditions.push(model);
        }
        join_conditions(conditions)
    }

    fn llm_where(&self, alias: &str) -> String {
        let mut conditions = vec![self.date_predicate(&format!("{alias}.started_at_ns"))];
        if let Some(source) = self.source_condition(alias) {
            conditions.push(source);
        }
        if let Some(profile) = self.profile_condition(alias) {
            conditions.push(profile);
        }
        if let Some(device) = self.device_condition(alias) {
            conditions.push(device);
        }
        if let Some(model) = self.model_condition(alias) {
            conditions.push(model);
        }
        join_conditions(conditions)
    }

    fn tool_where(&self, alias: &str) -> String {
        let mut conditions = vec![self.date_predicate(&format!("{alias}.started_at_ns"))];
        if let Some(source) = self.source_condition(alias) {
            conditions.push(source);
        }
        if let Some(profile) = self.profile_condition(alias) {
            conditions.push(profile);
        }
        if let Some(device) = self.device_condition(alias) {
            conditions.push(device);
        }
        if let Some(model) = self.model_exists_for_tool(alias) {
            conditions.push(model);
        }
        join_conditions(conditions)
    }

    fn skill_where(&self, alias: &str) -> String {
        let mut conditions = vec![self.date_predicate(&format!("{alias}.occurred_at_ns"))];
        if let Some(source) = self.source_condition(alias) {
            conditions.push(source);
        }
        if let Some(profile) = self.profile_condition(alias) {
            conditions.push(profile);
        }
        if let Some(device) = self.device_condition(alias) {
            conditions.push(device);
        }
        if let Some(model) = self.model_exists_for_skill(alias) {
            conditions.push(model);
        }
        join_conditions(conditions)
    }

    fn date_predicate(&self, timestamp_expr: &str) -> String {
        if let Some(range) = self.date_range_sql() {
            format!(
                "{timestamp_expr} >= {} AND {timestamp_expr} < {}",
                date_expr_start_ns_sql(&range.from_expr),
                date_expr_start_ns_sql(&range.to_expr)
            )
        } else {
            "1 = 1".to_string()
        }
    }

    fn source_condition(&self, alias: &str) -> Option<String> {
        self.source
            .as_deref()
            .map(|source| format!("{alias}.source = {}", sql_literal(source)))
    }

    fn profile_condition(&self, alias: &str) -> Option<String> {
        self.profile_id
            .as_deref()
            .map(|profile_id| format!("{alias}.profile_id = {}", sql_literal(profile_id)))
    }

    fn device_condition(&self, alias: &str) -> Option<String> {
        self.device_id
            .as_deref()
            .map(|device_id| format!("{alias}.device_id = {}", sql_literal(device_id)))
    }

    fn model_condition(&self, alias: &str) -> Option<String> {
        self.model.as_deref().map(|model| {
            if model == "unknown" {
                format!("({alias}.model IS NULL OR {alias}.model = '')")
            } else {
                format!("{alias}.model = {}", sql_literal(model))
            }
        })
    }

    fn model_exists_for_session(&self, alias: &str) -> Option<String> {
        self.model_condition("lm").map(|condition| {
            format!(
                "EXISTS (SELECT 1 FROM llm_calls lm WHERE lm.session_id = {alias}.session_id AND {condition})"
            )
        })
    }

    fn model_exists_for_run(&self, alias: &str) -> Option<String> {
        self.model_condition("lm").map(|condition| {
            format!(
                "EXISTS (SELECT 1 FROM llm_calls lm WHERE lm.run_id = {alias}.run_id AND {condition})"
            )
        })
    }

    fn model_exists_for_turn(&self, alias: &str) -> Option<String> {
        self.model_condition("lm").map(|condition| {
            format!(
                "EXISTS (SELECT 1 FROM llm_calls lm WHERE lm.run_id = {alias}.run_id AND {condition})"
            )
        })
    }

    fn model_exists_for_tool(&self, alias: &str) -> Option<String> {
        self.model_condition("lm").map(|condition| {
            format!(
                "EXISTS (SELECT 1 FROM llm_calls lm WHERE lm.run_id = {alias}.run_id AND {condition})"
            )
        })
    }

    fn model_exists_for_skill(&self, alias: &str) -> Option<String> {
        self.model_condition("lm").map(|condition| {
            format!(
                "EXISTS (SELECT 1 FROM llm_calls lm WHERE lm.run_id = {alias}.run_id AND {condition})"
            )
        })
    }
}

impl UsageSummary {
    fn from_totals(totals: &OverviewTotals) -> Self {
        Self {
            sessions: totals.sessions,
            runs: totals.runs,
            turns: totals.turns,
            llm_calls: totals.llm_calls,
            tool_calls: totals.tool_calls,
            failed_tool_calls: totals.failed_tool_calls,
            input_tokens: totals.input_tokens,
            output_tokens: totals.output_tokens,
            total_tokens: totals.input_tokens.saturating_add(totals.output_tokens),
            cache_read_tokens: totals.cache_read_tokens,
            cache_write_tokens: totals.cache_write_tokens,
            cache_hit_rate: optional_ratio(totals.cache_read_tokens, totals.input_tokens),
            tool_failure_rate: optional_ratio(totals.failed_tool_calls, totals.tool_calls),
            total_cost_usd: totals.total_cost_usd,
        }
    }
}

fn load_overview(db_path: &PathBuf) -> Result<OverviewResponse> {
    let conn =
        Connection::open(db_path).with_context(|| format!("open sqlite {}", db_path.display()))?;

    Ok(OverviewResponse {
        status: "ok",
        totals: load_totals(&conn)?,
        imports: load_import_summaries(&conn)?,
        top_signals: load_top_signals(&conn)?,
        source_usage: load_source_usage(&conn)?,
        recent_sessions: load_recent_sessions(&conn)?,
        recent_runs: load_recent_runs(&conn)?,
        tool_summaries: load_tool_summaries(&conn)?,
        model_summaries: load_model_summaries(&conn)?,
    })
}

fn load_usage(db_path: &PathBuf, filters: UsageFilters) -> Result<UsageResponse> {
    let mut conn =
        Connection::open(db_path).with_context(|| format!("open sqlite {}", db_path.display()))?;
    let tx = conn.transaction()?;
    let totals = load_usage_totals(&tx, &filters)?;
    let buckets = if filters.preset == "today" {
        load_today_hourly_usage_buckets(&tx, &filters)?
    } else {
        compact_usage_buckets(load_usage_buckets(&tx, &filters)?)
    };
    let summary = UsageSummary::from_totals(&totals);
    let latency_observations = load_latency_observations(&tx, &filters)?;
    let latency = latency_summary_from_observations(&latency_observations);
    let latency_panel = latency_panel_from_observations(&tx, &filters, &latency_observations)?;
    let latency_buckets = latency_buckets_from_observations(&filters, &latency_observations);

    let response = UsageResponse {
        status: "ok",
        filters: filters.clone(),
        usage_grain: filters.usage_grain().to_string(),
        profile_options: load_profile_options(&tx, filters.profile_id.as_deref())?,
        source_options: load_source_options(&tx)?,
        model_options: load_model_options(&tx)?,
        summary,
        latency,
        latency_panel,
        latency_buckets,
        buckets,
        source_usage: load_source_usage_scoped(&tx, &filters)?,
        recent_sessions: load_recent_sessions_scoped(&tx, &filters)?,
        recent_runs: load_recent_runs_scoped(&tx, &filters)?,
        tool_summaries: load_tool_summaries_scoped(&tx, &filters)?,
        tool_failures: load_tool_failures_scoped(&tx, &filters)?,
        skill_summaries: load_skill_summaries_scoped(&tx, &filters)?,
        model_summaries: load_model_summaries_scoped(&tx, &filters)?,
    };
    tx.commit()?;
    Ok(response)
}

fn load_menubar(
    db_path: &PathBuf,
    query: MenubarQuery,
    sync: SyncStatus,
    identity: &Identity,
) -> Result<MenubarResponse> {
    let mut conn =
        Connection::open(db_path).with_context(|| format!("open sqlite {}", db_path.display()))?;
    let tx = conn.transaction()?;
    let range = normalize_menubar_range(query.range.as_deref());
    let selected_profile_id =
        normalize_filter_value(query.profile_id).or_else(|| Some(identity.profile_id.clone()));
    let filters = menubar_filters(range, selected_profile_id);
    let summary = UsageSummary::from_totals(&load_usage_totals(&tx, &filters)?);
    let latency_observations = load_latency_observations(&tx, &filters)?;
    let latency = latency_summary_from_observations(&latency_observations);
    let latency_panel = latency_panel_from_observations(&tx, &filters, &latency_observations)?;
    let mut recent_runs = load_recent_runs_scoped(&tx, &filters)?;
    recent_runs.truncate(6);
    let latest_run = if recent_runs.is_empty() {
        load_recent_runs_scoped(&tx, &UsageFilters::all())?
            .into_iter()
            .next()
    } else {
        None
    };

    let last_updated_at_ns = sync
        .finished_at_ns
        .or_else(|| latest_import_scan_ns(&tx).ok().flatten());

    let response = MenubarResponse {
        status: "ok",
        range: range.to_string(),
        profile_options: load_profile_options(&tx, filters.profile_id.as_deref())?,
        current_profile_id: filters
            .profile_id
            .clone()
            .unwrap_or_else(|| "all".to_string()),
        summary,
        latency,
        latency_panel,
        trend: if range == "today" {
            load_today_hourly_usage_buckets(&tx, &filters)?
        } else {
            load_usage_buckets(&tx, &filters)?
        },
        source_usage: load_source_usage_scoped(&tx, &filters)?,
        recent_runs,
        latest_run,
        last_updated_at_ns,
        sync,
    };
    tx.commit()?;
    Ok(response)
}

fn normalize_menubar_range(value: Option<&str>) -> &'static str {
    match value {
        Some("7d") => "7d",
        Some("30d") => "30d",
        _ => "today",
    }
}

fn menubar_filters(range: &str, profile_id: Option<String>) -> UsageFilters {
    UsageFilters {
        preset: range.to_string(),
        range: range.to_string(),
        from: None,
        to: None,
        grain: "day".to_string(),
        profile_id,
        device_id: None,
        source: None,
        model: None,
    }
}

fn load_sessions(db_path: &PathBuf) -> Result<SessionListResponse> {
    let conn =
        Connection::open(db_path).with_context(|| format!("open sqlite {}", db_path.display()))?;

    Ok(SessionListResponse {
        status: "ok",
        sessions: load_recent_sessions_scoped_limit(&conn, &UsageFilters::all(), 80)?,
    })
}

fn load_workspace_status(
    db_path: &FsPath,
    identity: &Identity,
    restart_required: bool,
) -> Result<WorkspaceStatusResponse> {
    let default_shared_dir = workspace::default_shared_workspace_dir();
    let shared_dir = shared_workspace_dir(db_path).map(PathBuf::from);
    let mode = if shared_dir.is_some() {
        "shared"
    } else {
        "local"
    };
    let workspace_dir = shared_dir
        .clone()
        .or_else(|| db_path.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    let shared_workspace_exists = default_shared_dir.join("workspace.json").exists();
    let profile_count = load_profile_count(db_path).unwrap_or_default();
    let (writable, mut issue) = if let Some(shared_dir) = shared_dir.as_deref() {
        check_shared_workspace_writable(shared_dir)
    } else {
        (true, None)
    };

    if mode == "local" && shared_workspace_exists && issue.is_none() {
        issue = Some("Shared workspace is ready. Restart Shirabe to use it.".to_string());
    }

    if restart_required {
        issue = Some(format!(
            "Shared workspace is ready at {}. Restart Shirabe to use it.",
            default_shared_dir.display()
        ));
    }

    Ok(WorkspaceStatusResponse {
        status: "ok",
        mode,
        workspace_dir: workspace_dir.display().to_string(),
        catalog_db: db_path.display().to_string(),
        shared_workspace_dir: default_shared_dir.display().to_string(),
        shared_workspace_exists,
        current_profile_id: identity.profile_id.clone(),
        current_profile_label: identity.profile_label.clone(),
        current_device_id: identity.device_id.clone(),
        current_device_label: identity.device_label.clone(),
        profile_count,
        writable,
        repair_available: true,
        restart_required,
        issue,
    })
}

fn repair_workspace_inner(state: &AppState) -> Result<WorkspaceStatusResponse> {
    let current_shared_dir = shared_workspace_dir(&state.db_path).map(PathBuf::from);
    let target_dir = current_shared_dir
        .clone()
        .unwrap_or_else(workspace::default_shared_workspace_dir);
    let report = workspace::prepare(Some(target_dir.clone()))?;
    let shared_db = Database::open(&report.catalog_db)?;
    shared_db.migrate()?;
    shared_db.register_identity(&state.identity)?;
    let mut pricing_changed = workspace::seed_pricing_cache(&shared_db, &state.db_path)?;
    if !pricing_changed {
        pricing_changed = workspace::seed_pricing_cache_from_default_local(&shared_db)?;
    }
    if pricing_changed {
        rollup::refresh_all(&shared_db)?;
    }

    load_workspace_status(
        &state.db_path,
        &state.identity,
        current_shared_dir.is_none(),
    )
}

fn load_profile_count(db_path: &FsPath) -> Result<i64> {
    let conn =
        Connection::open(db_path).with_context(|| format!("open sqlite {}", db_path.display()))?;
    Ok(conn
        .query_row("SELECT COUNT(*) FROM profiles", [], |row| row.get(0))
        .optional()?
        .unwrap_or_default())
}

fn check_shared_workspace_writable(workspace_dir: &FsPath) -> (bool, Option<String>) {
    for child in ["inbox", "profiles", "collector-state", "logs"] {
        let dir = workspace_dir.join(child);
        if let Err(error) = fs::create_dir_all(&dir) {
            return (
                false,
                Some(format!(
                    "{} is not writable: {}. Run Repair from an admin account.",
                    dir.display(),
                    error
                )),
            );
        }

        let probe = dir.join(format!(
            ".shirabe-write-check-{}-{}",
            std::process::id(),
            crate::db::now_ns()
        ));
        match fs::write(&probe, b"ok").and_then(|_| fs::remove_file(&probe)) {
            Ok(()) => {}
            Err(error) => {
                let _ = fs::remove_file(&probe);
                return (
                    false,
                    Some(format!(
                        "{} is not writable: {}. Run Repair from an admin account.",
                        dir.display(),
                        error
                    )),
                );
            }
        }
    }

    (true, None)
}

fn current_sync_status(state: &AppState) -> SyncStatus {
    state
        .sync
        .lock()
        .expect("sync status lock poisoned")
        .clone()
}

fn run_sync_job(state: Arc<AppState>) {
    let started = Instant::now();
    let result = run_sync_job_inner(&state);
    let finished_at_ns = crate::db::now_ns();
    let duration_ms = elapsed_ms(started);

    let mut sync = state.sync.lock().expect("sync status lock poisoned");
    sync.finished_at_ns = Some(finished_at_ns);
    sync.duration_ms = Some(duration_ms);
    match result {
        Ok(()) => {
            sync.state = "idle".to_string();
            sync.phase = "complete".to_string();
        }
        Err(error) => {
            sync.state = "failed".to_string();
            sync.phase = "failed".to_string();
            sync.error = Some(format!("{error:#}"));
        }
    }
}

fn run_sync_job_inner(state: &Arc<AppState>) -> Result<()> {
    let db = Database::open(&state.db_path)
        .with_context(|| format!("open sqlite {}", state.db_path.display()))?;
    db.migrate()?;

    let mut imported_files = 0usize;
    let mut pricing_changed = false;
    if let Some(workspace_dir) = shared_workspace_dir(&state.db_path) {
        set_sync_phase(state, "sync pricing");
        pricing_changed = workspace::seed_pricing_cache_from_default_local(&db)?;

        set_sync_phase(state, "drain inbox");
        let drain = workspace::drain_inbox(&db, workspace_dir)?;
        if drain.events_projected > 0 {
            imported_files = imported_files.saturating_add(drain.files_drained);
        }
    }

    set_sync_phase(state, "refresh pricing");
    match pricing::refresh_litellm_pricing_if_stale(
        &db,
        pricing::LITELLM_PRICING_URL,
        pricing::PRICING_REFRESH_INTERVAL,
    ) {
        Ok(Some(report)) => {
            pricing_changed = true;
            tracing::info!(
                models = report.model_count,
                aliases = report.alias_count,
                "refreshed model pricing"
            );
        }
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), "model pricing refresh failed; continuing usage sync");
        }
    }

    for source in enabled_sync_sources(&state.source_settings) {
        set_sync_phase(state, &format!("import {source}"));
        let report = import_sync_source(&db, source, &state.source_paths, &state.identity);
        imported_files = imported_files.saturating_add(report.files_imported);
        state
            .sync
            .lock()
            .expect("sync status lock poisoned")
            .sources
            .push(report);
    }

    if imported_files == 0 && !pricing_changed {
        set_sync_phase(state, "complete");
        return Ok(());
    }

    set_sync_phase(state, "refresh rollups");
    let started = Instant::now();
    let report = rollup::refresh_all(&db)?;
    state.sync.lock().expect("sync status lock poisoned").rollup = Some(SyncRollupReport {
        status: report.status.to_string(),
        source_rollups: report.source_rollups,
        model_rollups: report.model_rollups,
        tool_rollups: report.tool_rollups,
        tool_model_rollups: report.tool_model_rollups,
        elapsed_ms: elapsed_ms(started),
    });

    Ok(())
}

fn enabled_sync_sources(settings: &SourceSettings) -> Vec<&'static str> {
    ["codex", "pi", "claude", "kanade", "amp"]
        .into_iter()
        .filter(|source| settings.is_enabled(source))
        .collect()
}

fn shared_workspace_dir(db_path: &FsPath) -> Option<&FsPath> {
    db_path.parent().filter(|path| is_shared_workspace(path))
}

fn is_shared_workspace(path: &FsPath) -> bool {
    is_shared_workspace_dir(path)
}

fn set_sync_phase(state: &AppState, phase: &str) {
    state.sync.lock().expect("sync status lock poisoned").phase = phase.to_string();
}

fn import_sync_source(
    db: &Database,
    source: &str,
    source_paths: &SourcePaths,
    identity: &Identity,
) -> SyncSourceReport {
    let started = Instant::now();
    let modified_since_ns = sync_modified_since_ns(db, source, &identity.profile_id);
    let import_identity =
        ImportIdentity::new(identity.profile_id.clone(), identity.device_id.clone());
    let result = match source {
        "codex" => importers::codex::import_recent_with_identity(
            db,
            Some(source_paths.codex.clone()),
            modified_since_ns,
            &import_identity,
        ),
        "pi" => importers::pi::import_recent_with_identity(
            db,
            Some(source_paths.pi.clone()),
            modified_since_ns,
            &import_identity,
        ),
        "claude" => importers::claude::import_recent_with_identity(
            db,
            Some(source_paths.claude.clone()),
            modified_since_ns,
            &import_identity,
        ),
        "kanade" => importers::kanade::import_recent_with_identity(
            db,
            Some(source_paths.kanade.clone()),
            modified_since_ns,
            &import_identity,
        ),
        "amp" => importers::amp::sync_cli_first_recent_with_identity(
            db,
            &source_paths.amp,
            modified_since_ns,
            &import_identity,
        ),
        _ => unreachable!("unsupported sync source"),
    };

    sync_source_report(source, result, elapsed_ms(started))
}

fn sync_source_report(
    source: &str,
    result: Result<crate::importers::ImportReport>,
    elapsed_ms: i64,
) -> SyncSourceReport {
    match result {
        Ok(report) => SyncSourceReport {
            source: report.source,
            status: "ok".to_string(),
            files_seen: report.files_seen,
            files_imported: report.files_imported,
            files_skipped: report.files_skipped,
            events_projected: report.events_projected,
            source_bytes_scanned: report.source_bytes_scanned,
            mode: report.mode,
            threads_enumerated: report.threads_enumerated,
            threads_exported: report.threads_exported,
            unchanged_threads_skipped: report.unchanged_threads_skipped,
            elapsed_ms,
            error: None,
        },
        Err(error) => SyncSourceReport {
            source: source.to_string(),
            status: "failed".to_string(),
            files_seen: 0,
            files_imported: 0,
            files_skipped: 0,
            events_projected: 0,
            source_bytes_scanned: 0,
            mode: None,
            threads_enumerated: None,
            threads_exported: None,
            unchanged_threads_skipped: None,
            elapsed_ms,
            error: Some(format!("{error:#}")),
        },
    }
}

fn sync_modified_since_ns(db: &Database, source: &str, profile_id: &str) -> i64 {
    db.latest_import_source_scan_ns_for_profile(source, profile_id)
        .ok()
        .flatten()
        .map(|last_scan_ns| last_scan_ns.saturating_sub(SYNC_WATERMARK_OVERLAP_NS))
        .unwrap_or_else(|| crate::db::now_ns().saturating_sub(FALLBACK_RECENT_SYNC_WINDOW_NS))
}

fn elapsed_ms(started: Instant) -> i64 {
    started.elapsed().as_millis().min(i64::MAX as u128) as i64
}

fn load_session_detail(
    db_path: &PathBuf,
    session_id: &str,
) -> Result<Option<SessionDetailResponse>> {
    let conn =
        Connection::open(db_path).with_context(|| format!("open sqlite {}", db_path.display()))?;
    let Some(session) = load_session_summary(&conn, session_id)? else {
        return Ok(None);
    };

    Ok(Some(SessionDetailResponse {
        status: "ok",
        session,
        runs: load_session_runs(&conn, session_id)?,
        turns: load_session_turns(&conn, session_id)?,
        timeline: load_session_timeline(&conn, session_id)?,
        llm_calls: load_session_llm_calls(&conn, session_id)?,
        tool_calls: load_session_tool_calls(&conn, session_id)?,
        skill_events: load_session_skill_events(&conn, session_id)?,
        tool_summaries: load_session_tool_summaries(&conn, session_id)?,
        skill_summaries: load_session_skill_summaries(&conn, session_id)?,
        model_summaries: load_session_model_summaries(&conn, session_id)?,
    }))
}

fn load_run_detail(db_path: &PathBuf, run_id: &str) -> Result<Option<RunDetailResponse>> {
    let conn =
        Connection::open(db_path).with_context(|| format!("open sqlite {}", db_path.display()))?;
    let Some(run) = load_run(&conn, run_id)? else {
        return Ok(None);
    };

    Ok(Some(RunDetailResponse {
        status: "ok",
        signals: load_run_signals(&conn, run_id)?,
        timeline: load_run_steps(&conn, run_id)?,
        llm_calls: load_llm_calls(&conn, run_id)?,
        tool_calls: load_tool_calls(&conn, run_id)?,
        run,
    }))
}

fn load_totals(conn: &Connection) -> Result<OverviewTotals> {
    Ok(OverviewTotals {
        import_sources: count(conn, "import_sources")?,
        import_files: count(conn, "import_files")?,
        imported_files: count_where(conn, "import_files", "status = 'imported'")?,
        partial_files: count_where(conn, "import_files", "status = 'partial'")?,
        failed_files: count_where(conn, "import_files", "status = 'failed'")?,
        sessions: count(conn, "sessions")?,
        runs: count(conn, "runs")?,
        turns: count(conn, "turns")?,
        run_steps: count(conn, "run_steps")?,
        llm_calls: count(conn, "llm_calls")?,
        tool_calls: count(conn, "tool_calls")?,
        failed_tool_calls: count_where(conn, "tool_calls", "status = 'failed'")?,
        signals: count(conn, "run_signals")?,
        input_tokens: sum_i64(conn, "llm_calls", "input_tokens")?,
        output_tokens: sum_i64(conn, "llm_calls", "output_tokens")?,
        cache_read_tokens: sum_i64(conn, "llm_calls", "cache_read_tokens")?,
        cache_write_tokens: sum_i64(conn, "llm_calls", "cache_write_tokens")?,
        total_cost_usd: sum_llm_cost(conn)?,
    })
}

fn load_usage_totals(conn: &Connection, filters: &UsageFilters) -> Result<OverviewTotals> {
    if filters.preset == "today" {
        return load_usage_totals_raw(conn, filters);
    }

    let sessions = if filters.model.is_some() {
        count_sql(
            conn,
            &format!(
                "SELECT COUNT(DISTINCT l.session_id) FROM llm_calls l WHERE {}",
                filters.llm_where("l")
            ),
        )?
    } else {
        count_sql(
            conn,
            &format!(
                "SELECT COUNT(DISTINCT r.session_id) FROM runs r WHERE {}",
                filters.run_where("r")
            ),
        )?
    };

    let model_where = filters.rollup_model_where("m");
    let (
        runs,
        turns,
        llm_calls,
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
        total_cost_usd,
        tool_calls,
        failed_tool_calls,
    ) = if filters.model.is_some() {
        let usage_sql = format!(
            "SELECT
                COALESCE(SUM(m.runs), 0),
                COALESCE(SUM(m.turns), 0),
                COALESCE(SUM(m.llm_calls), 0),
                COALESCE(SUM(m.input_tokens), 0),
                COALESCE(SUM(m.output_tokens), 0),
                COALESCE(SUM(m.cache_read_tokens), 0),
                COALESCE(SUM(m.cache_write_tokens), 0),
                COALESCE(SUM({MODEL_ROLLUP_COST_SQL}), 0)
             FROM usage_model_rollups m
             LEFT JOIN model_prices mp ON mp.model_name = m.model
             WHERE {model_where}"
        );
        let usage = conn.query_row(&usage_sql, [], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, f64>(7)?,
            ))
        })?;

        let tool_model_where = filters.rollup_model_where("tm");
        let tool_sql = format!(
            "SELECT
                COALESCE(SUM(tm.calls), 0),
                COALESCE(SUM(tm.failed_calls), 0)
             FROM usage_tool_model_rollups tm
             WHERE {tool_model_where}"
        );
        let tools = conn.query_row(&tool_sql, [], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })?;

        (
            usage.0, usage.1, usage.2, usage.3, usage.4, usage.5, usage.6, usage.7, tools.0,
            tools.1,
        )
    } else {
        let source_where = filters.rollup_source_where("s");
        let usage_sql = format!(
            "SELECT
                COALESCE(SUM(s.runs), 0),
                COALESCE(SUM(s.turns), 0),
                COALESCE(SUM(s.llm_calls), 0),
                COALESCE(SUM(s.input_tokens), 0),
                COALESCE(SUM(s.output_tokens), 0),
                COALESCE(SUM(s.cache_read_tokens), 0),
                COALESCE(SUM(s.cache_write_tokens), 0),
                COALESCE(SUM(s.tool_calls), 0),
                COALESCE(SUM(s.failed_tool_calls), 0)
             FROM usage_source_rollups s
             WHERE {source_where}"
        );
        let usage = conn.query_row(&usage_sql, [], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
            ))
        })?;

        (
            usage.0,
            usage.1,
            usage.2,
            usage.3,
            usage.4,
            usage.5,
            usage.6,
            sum_model_rollup_cost_where(conn, &filters.rollup_source_where("m"))?,
            usage.7,
            usage.8,
        )
    };

    Ok(OverviewTotals {
        import_sources: 0,
        import_files: 0,
        imported_files: 0,
        partial_files: 0,
        failed_files: 0,
        sessions,
        runs,
        turns,
        run_steps: 0,
        llm_calls,
        tool_calls,
        failed_tool_calls,
        signals: 0,
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
        total_cost_usd,
    })
}

fn load_usage_totals_raw(conn: &Connection, filters: &UsageFilters) -> Result<OverviewTotals> {
    let sessions = if filters.model.is_some() {
        count_sql(
            conn,
            &format!(
                "SELECT COUNT(DISTINCT l.session_id) FROM llm_calls l WHERE {}",
                filters.llm_where("l")
            ),
        )?
    } else {
        count_sql(
            conn,
            &format!(
                "SELECT COUNT(DISTINCT r.session_id) FROM runs r WHERE {}",
                filters.run_where("r")
            ),
        )?
    };
    let runs = if filters.model.is_some() {
        count_sql(
            conn,
            &format!(
                "SELECT COUNT(DISTINCT l.run_id) FROM llm_calls l WHERE {}",
                filters.llm_where("l")
            ),
        )?
    } else {
        count_sql(
            conn,
            &format!(
                "SELECT COUNT(*) FROM runs r WHERE {}",
                filters.run_where("r")
            ),
        )?
    };
    let turns = if filters.model.is_some() {
        count_sql(
            conn,
            &format!(
                "SELECT COUNT(DISTINCT l.turn_id) FROM llm_calls l WHERE {}",
                filters.llm_where("l")
            ),
        )?
    } else {
        count_sql(
            conn,
            &format!(
                "SELECT COUNT(*) FROM turns t WHERE {}",
                filters.turn_where("t")
            ),
        )?
    };

    let llm_sql = format!(
        "SELECT
            COUNT(*),
            COALESCE(SUM(l.input_tokens), 0),
            COALESCE(SUM(l.output_tokens), 0),
            COALESCE(SUM(l.cache_read_tokens), 0),
            COALESCE(SUM(l.cache_write_tokens), 0),
            COALESCE(SUM({LLM_COST_SQL}), 0)
         FROM llm_calls l
         LEFT JOIN model_prices mp ON mp.model_name = l.model
         WHERE {}",
        filters.llm_where("l")
    );
    let llm = conn.query_row(&llm_sql, [], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, f64>(5)?,
        ))
    })?;

    let tool_sql = format!(
        "SELECT
            COUNT(*),
            COALESCE(SUM(CASE WHEN tc.status = 'failed' THEN 1 ELSE 0 END), 0)
         FROM tool_calls tc
         WHERE {}",
        filters.tool_where("tc")
    );
    let tools = conn.query_row(&tool_sql, [], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
    })?;

    Ok(OverviewTotals {
        import_sources: 0,
        import_files: 0,
        imported_files: 0,
        partial_files: 0,
        failed_files: 0,
        sessions,
        runs,
        turns,
        run_steps: 0,
        llm_calls: llm.0,
        tool_calls: tools.0,
        failed_tool_calls: tools.1,
        signals: 0,
        input_tokens: llm.1,
        output_tokens: llm.2,
        cache_read_tokens: llm.3,
        cache_write_tokens: llm.4,
        total_cost_usd: llm.5,
    })
}

fn load_latency_observations(
    conn: &Connection,
    filters: &UsageFilters,
) -> Result<Vec<LatencyObservation>> {
    let bucket_expr = if filters.preset == "today" {
        "printf('%02d:00', CAST(strftime('%H', l.started_at_ns / 1000000000, 'unixepoch', 'localtime') AS INTEGER))"
    } else if filters.usage_grain() == "month" {
        "COALESCE(l.started_month, strftime('%Y-%m', l.started_at_ns / 1000000000, 'unixepoch', 'localtime'))"
    } else {
        "COALESCE(l.started_day, date(l.started_at_ns / 1000000000, 'unixepoch', 'localtime'))"
    };
    let sql = format!(
        "SELECT
            COALESCE(NULLIF(l.provider, ''), 'unknown') AS provider,
            CAST(strftime('%H', l.started_at_ns / 1000000000, 'unixepoch', 'localtime') AS INTEGER) AS hour,
            {bucket_expr} AS bucket_key,
            l.observed_response_delay_ns,
            l.observed_output_tps,
            l.observed_latency_quality
         FROM llm_calls l
         WHERE {}",
        filters.llm_where("l")
    );
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map([], |row| {
        Ok(LatencyObservation {
            provider: row.get(0)?,
            hour: row.get::<_, i64>(1)?.clamp(0, 23) as usize,
            bucket_key: row.get(2)?,
            delay_ns: row.get(3)?,
            tps: row.get(4)?,
            quality: row.get(5)?,
        })
    })?;
    collect_rows(rows)
}

fn latency_summary_from_observations(observations: &[LatencyObservation]) -> LatencySummary {
    let mut llm_calls = 0i64;
    let mut observed_calls = 0i64;
    let mut good_calls = 0i64;
    let mut subsecond_calls = 0i64;
    let mut outlier_calls = 0i64;
    let mut missing_calls = 0i64;
    let mut delays = Vec::new();
    let mut tps_values = Vec::new();

    for observation in observations {
        llm_calls += 1;
        match observation.quality.as_deref() {
            Some("good") => {
                good_calls += 1;
                observed_calls += 1;
            }
            Some("subsecond") => {
                subsecond_calls += 1;
                observed_calls += 1;
            }
            Some("outlier") => {
                outlier_calls += 1;
                observed_calls += 1;
            }
            Some("missing") | None => missing_calls += 1,
            Some(_) => missing_calls += 1,
        }
        if let Some(delay_ns) = observation.delay_ns {
            delays.push(delay_ns);
        }
        if let Some(tps) = observation.tps {
            tps_values.push(tps);
        }
    }

    delays.sort_unstable();
    tps_values.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));

    LatencySummary {
        llm_calls,
        observed_calls,
        good_calls,
        subsecond_calls,
        outlier_calls,
        missing_calls,
        p50_response_delay_ns: percentile_i64(&delays, 0.5),
        p90_response_delay_ns: percentile_i64(&delays, 0.9),
        avg_observed_output_tps: mean_f64(&tps_values),
        p50_observed_output_tps: percentile_f64(&tps_values, 0.5),
    }
}

fn latency_buckets_from_observations(
    filters: &UsageFilters,
    observations: &[LatencyObservation],
) -> Vec<LatencyBucketSummary> {
    let mut grouped = if filters.preset == "today" {
        (0..24)
            .map(|hour| (hour_label(hour), LatencyBucketAccumulator::default()))
            .collect::<HashMap<_, _>>()
    } else {
        HashMap::new()
    };

    for observation in observations {
        if observation.quality.as_deref() != Some("good") {
            continue;
        }
        let Some(delay_ns) = observation.delay_ns else {
            continue;
        };
        let bucket_key = if filters.preset == "today" {
            hour_label(observation.hour)
        } else {
            observation.bucket_key.clone()
        };
        grouped
            .entry(bucket_key)
            .or_default()
            .push(delay_ns, observation.tps);
    }

    let mut buckets = grouped
        .into_iter()
        .map(|(bucket_key, accumulator)| latency_bucket_summary(bucket_key, accumulator))
        .collect::<Vec<_>>();
    buckets.sort_by(|left, right| right.bucket_key.cmp(&left.bucket_key));
    buckets
}

#[derive(Default)]
struct LatencyBucketAccumulator {
    delays: Vec<i64>,
    tps_values: Vec<f64>,
}

impl LatencyBucketAccumulator {
    fn push(&mut self, delay_ns: i64, tps: Option<f64>) {
        self.delays.push(delay_ns);
        if let Some(tps) = tps
            && tps.is_finite()
        {
            self.tps_values.push(tps);
        }
    }
}

fn latency_bucket_summary(
    bucket_key: String,
    mut accumulator: LatencyBucketAccumulator,
) -> LatencyBucketSummary {
    accumulator.delays.sort_unstable();
    accumulator
        .tps_values
        .sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    let avg_tps = if accumulator.tps_values.is_empty() {
        None
    } else {
        Some(accumulator.tps_values.iter().sum::<f64>() / accumulator.tps_values.len() as f64)
    };

    LatencyBucketSummary {
        date: bucket_key.clone(),
        bucket_key,
        bucket_count: 1,
        good_calls: accumulator.delays.len().min(i64::MAX as usize) as i64,
        p50_response_delay_ns: percentile_i64(&accumulator.delays, 0.5),
        p90_response_delay_ns: percentile_i64(&accumulator.delays, 0.9),
        avg_observed_output_tps: avg_tps,
        p50_observed_output_tps: percentile_f64(&accumulator.tps_values, 0.5),
    }
}

#[derive(Debug, Clone)]
struct LatencyObservation {
    provider: String,
    hour: usize,
    bucket_key: String,
    delay_ns: Option<i64>,
    tps: Option<f64>,
    quality: Option<String>,
}

#[derive(Debug, Clone)]
struct LatencySample {
    provider: String,
    hour: usize,
    delay_ns: i64,
}

const MIN_LATENCY_GOOD_CALLS: i64 = 20;
const MIN_LATENCY_ACTIVE_GOOD_HOURS_FOR_PATTERN: i64 = 3;

fn latency_panel_from_observations(
    conn: &Connection,
    filters: &UsageFilters,
    observations: &[LatencyObservation],
) -> Result<LatencyPanel> {
    let samples = observations
        .iter()
        .filter_map(|observation| {
            if observation.quality.as_deref() != Some("good") {
                return None;
            }
            Some(LatencySample {
                provider: observation.provider.clone(),
                hour: observation.hour,
                delay_ns: observation.delay_ns?,
            })
        })
        .collect::<Vec<_>>();
    if filters.preset == "today" {
        load_today_latency_panel(conn, samples)
    } else {
        Ok(load_pattern_latency_panel(samples))
    }
}

fn load_today_latency_panel(
    conn: &Connection,
    samples: Vec<LatencySample>,
) -> Result<LatencyPanel> {
    let current_hour = current_local_hour(conn)?;
    let baseline = latency_baseline(&samples);
    let baseline_p50 = baseline.p50_response_delay_ns;
    let current_samples = samples
        .iter()
        .filter(|sample| sample.hour == current_hour)
        .cloned()
        .collect::<Vec<_>>();
    let current_delays = current_samples
        .iter()
        .map(|sample| sample.delay_ns)
        .collect::<Vec<_>>();
    let current_p50 = percentile_i64_sorted_copy(&current_delays, 0.5);
    let ratio_to_baseline = ratio_i64(current_p50, baseline_p50);

    let status = if baseline.good_calls < MIN_LATENCY_GOOD_CALLS {
        "learning"
    } else if current_samples.len() < 5 {
        "normal"
    } else {
        latency_status(ratio_to_baseline)
    };

    let current = Some(LatencyCurrent {
        hour: hour_label(current_hour),
        p50_response_delay_ns: current_p50,
        ratio_to_baseline,
        good_calls: current_samples.len().min(i64::MAX as usize) as i64,
        slowest_provider: slowest_provider_now(&current_samples, baseline_p50),
    });

    Ok(LatencyPanel {
        mode: "today".to_string(),
        status: status.to_string(),
        baseline,
        current,
        slow_hours: if status == "learning" {
            Vec::new()
        } else {
            slow_latency_hours(&samples, baseline_p50, current_hour)
        },
        best_windows: Vec::new(),
        slow_windows: Vec::new(),
        provider_patterns: Vec::new(),
    })
}

fn load_pattern_latency_panel(samples: Vec<LatencySample>) -> LatencyPanel {
    let baseline = latency_baseline(&samples);
    let learning = baseline.good_calls < MIN_LATENCY_GOOD_CALLS
        || baseline.active_good_hours < MIN_LATENCY_ACTIVE_GOOD_HOURS_FOR_PATTERN;
    let (best_windows, slow_windows) = if learning {
        (Vec::new(), Vec::new())
    } else {
        latency_windows(&samples)
    };

    LatencyPanel {
        mode: "pattern".to_string(),
        status: if learning {
            "learning".to_string()
        } else {
            "pattern".to_string()
        },
        baseline,
        current: None,
        slow_hours: Vec::new(),
        best_windows,
        slow_windows,
        provider_patterns: provider_latency_patterns(&samples),
    }
}

fn latency_baseline(samples: &[LatencySample]) -> LatencyBaseline {
    let delays = samples
        .iter()
        .map(|sample| sample.delay_ns)
        .collect::<Vec<_>>();
    let mut calls_by_hour = [0usize; 24];
    for sample in samples {
        calls_by_hour[sample.hour] += 1;
    }

    LatencyBaseline {
        p50_response_delay_ns: percentile_i64_sorted_copy(&delays, 0.5),
        good_calls: samples.len().min(i64::MAX as usize) as i64,
        active_good_hours: calls_by_hour
            .into_iter()
            .filter(|calls| *calls >= 5)
            .count()
            .min(i64::MAX as usize) as i64,
    }
}

fn slow_latency_hours(
    samples: &[LatencySample],
    baseline_p50: Option<i64>,
    current_hour: usize,
) -> Vec<LatencyHour> {
    let Some(baseline_p50) = baseline_p50 else {
        return Vec::new();
    };
    let threshold = baseline_p50 as f64 * 1.3;
    let mut by_hour = vec![Vec::<i64>::new(); 24];
    for sample in samples {
        by_hour[sample.hour].push(sample.delay_ns);
    }

    let mut hours = by_hour
        .iter()
        .enumerate()
        .filter_map(|(hour, delays)| {
            if delays.len() < 5 {
                return None;
            }
            let p50 = percentile_i64_sorted_copy(delays, 0.5)?;
            (p50 as f64 >= threshold).then(|| LatencyHour {
                hour: hour_label(hour),
                p50_response_delay_ns: p50,
                ratio_to_baseline: ratio_i64(Some(p50), Some(baseline_p50)),
                good_calls: delays.len().min(i64::MAX as usize) as i64,
                is_current: hour == current_hour,
            })
        })
        .collect::<Vec<_>>();
    hours.sort_by(|left, right| {
        right
            .p50_response_delay_ns
            .cmp(&left.p50_response_delay_ns)
            .then(left.hour.cmp(&right.hour))
    });
    hours.truncate(3);
    hours
}

fn slowest_provider_now(
    samples: &[LatencySample],
    baseline_p50: Option<i64>,
) -> Option<LatencyProviderNow> {
    let mut by_provider = HashMap::<String, Vec<i64>>::new();
    for sample in samples {
        by_provider
            .entry(sample.provider.clone())
            .or_default()
            .push(sample.delay_ns);
    }

    by_provider
        .into_iter()
        .filter_map(|(provider, delays)| {
            let p50 = percentile_i64_sorted_copy(&delays, 0.5)?;
            Some(LatencyProviderNow {
                provider,
                p50_response_delay_ns: p50,
                ratio_to_baseline: ratio_i64(Some(p50), baseline_p50),
                good_calls: delays.len().min(i64::MAX as usize) as i64,
            })
        })
        .max_by(|left, right| {
            left.p50_response_delay_ns
                .cmp(&right.p50_response_delay_ns)
                .then(left.good_calls.cmp(&right.good_calls))
        })
}

fn latency_windows(samples: &[LatencySample]) -> (Vec<LatencyWindow>, Vec<LatencyWindow>) {
    let mut by_hour = vec![Vec::<i64>::new(); 24];
    for sample in samples {
        by_hour[sample.hour].push(sample.delay_ns);
    }

    let mut windows = Vec::new();
    for hour in 0..23 {
        let mut delays = by_hour[hour].clone();
        delays.extend_from_slice(&by_hour[hour + 1]);
        if delays.len() < 10 {
            continue;
        }
        if let Some(p50) = percentile_i64_sorted_copy(&delays, 0.5) {
            windows.push(LatencyWindow {
                label: format!("{:02}-{:02}", hour, hour + 2),
                p50_response_delay_ns: p50,
                good_calls: delays.len().min(i64::MAX as usize) as i64,
            });
        }
    }

    let mut best = windows.clone();
    best.sort_by(|left, right| {
        left.p50_response_delay_ns
            .cmp(&right.p50_response_delay_ns)
            .then(right.good_calls.cmp(&left.good_calls))
    });
    best.truncate(2);

    let mut slow = windows;
    slow.sort_by(|left, right| {
        right
            .p50_response_delay_ns
            .cmp(&left.p50_response_delay_ns)
            .then(right.good_calls.cmp(&left.good_calls))
    });
    slow.truncate(2);

    (best, slow)
}

fn provider_latency_patterns(samples: &[LatencySample]) -> Vec<ProviderLatencyPattern> {
    let mut by_provider = HashMap::<String, Vec<&LatencySample>>::new();
    for sample in samples {
        by_provider
            .entry(sample.provider.clone())
            .or_default()
            .push(sample);
    }

    let mut providers = by_provider.into_iter().collect::<Vec<_>>();
    providers.sort_by(|left, right| right.1.len().cmp(&left.1.len()).then(left.0.cmp(&right.0)));
    providers.truncate(2);

    providers
        .into_iter()
        .map(|(provider, provider_samples)| {
            let mut by_hour = vec![Vec::<i64>::new(); 24];
            for sample in &provider_samples {
                by_hour[sample.hour].push(sample.delay_ns);
            }
            let mut hour_stats = by_hour
                .iter()
                .enumerate()
                .filter_map(|(hour, delays)| {
                    if delays.len() < 3 {
                        return None;
                    }
                    Some((hour, percentile_i64_sorted_copy(delays, 0.5)?))
                })
                .collect::<Vec<_>>();
            hour_stats.sort_by_key(|item| item.1);
            let best = hour_stats.first().copied();
            let slow = hour_stats.last().copied();

            ProviderLatencyPattern {
                provider,
                good_calls: provider_samples.len().min(i64::MAX as usize) as i64,
                best_hour: best.map(|(hour, _)| hour_label(hour)),
                best_p50_response_delay_ns: best.map(|(_, p50)| p50),
                slow_hour: slow.map(|(hour, _)| hour_label(hour)),
                slow_p50_response_delay_ns: slow.map(|(_, p50)| p50),
            }
        })
        .collect()
}

fn current_local_hour(conn: &Connection) -> Result<usize> {
    let hour: i64 = conn.query_row(
        "SELECT CAST(strftime('%H', 'now', 'localtime') AS INTEGER)",
        [],
        |row| row.get(0),
    )?;
    Ok(hour.clamp(0, 23) as usize)
}

fn hour_label(hour: usize) -> String {
    format!("{:02}:00", hour.min(23))
}

fn latency_status(ratio: Option<f64>) -> &'static str {
    match ratio {
        Some(ratio) if ratio >= 2.0 => "very_slow",
        Some(ratio) if ratio >= 1.3 => "slower",
        Some(_) => "normal",
        None => "learning",
    }
}

fn ratio_i64(value: Option<i64>, baseline: Option<i64>) -> Option<f64> {
    let value = value?;
    let baseline = baseline?;
    (baseline > 0).then(|| value as f64 / baseline as f64)
}

fn percentile_i64_sorted_copy(values: &[i64], percentile: f64) -> Option<i64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    percentile_i64(&sorted, percentile)
}

fn percentile_i64(values: &[i64], percentile: f64) -> Option<i64> {
    percentile_index(values.len(), percentile).map(|index| values[index])
}

fn percentile_f64(values: &[f64], percentile: f64) -> Option<f64> {
    percentile_index(values.len(), percentile).map(|index| values[index])
}

fn percentile_index(len: usize, percentile: f64) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let clamped = percentile.clamp(0.0, 1.0);
    Some(((len - 1) as f64 * clamped).round() as usize)
}

fn mean_f64(values: &[f64]) -> Option<f64> {
    (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
}

fn load_import_summaries(conn: &Connection) -> Result<Vec<ImportSourceSummary>> {
    let mut statement = conn.prepare(
        "SELECT
            s.source_id,
            s.source,
            s.source_kind,
            s.root_path,
            s.last_scan_ns,
            s.parser_version,
            COUNT(f.file_id) AS file_count,
            COALESCE(SUM(CASE WHEN f.status = 'imported' THEN 1 ELSE 0 END), 0) AS imported_files,
            COALESCE(SUM(CASE WHEN f.status = 'partial' THEN 1 ELSE 0 END), 0) AS partial_files,
            COALESCE(SUM(CASE WHEN f.status = 'failed' THEN 1 ELSE 0 END), 0) AS failed_files,
            COALESCE(SUM(f.size_bytes), 0) AS source_bytes,
            COALESCE(SUM(f.event_count), 0) AS event_count,
            COALESCE(SUM(f.warning_count), 0) AS warning_count
         FROM import_sources s
         LEFT JOIN import_files f ON f.source_id = s.source_id
         GROUP BY s.source_id
         ORDER BY s.last_scan_ns DESC
         LIMIT 20",
    )?;

    let rows = statement.query_map([], |row| {
        Ok(ImportSourceSummary {
            source_id: row.get(0)?,
            source: row.get(1)?,
            source_kind: row.get(2)?,
            root_path: row.get(3)?,
            last_scan_ns: row.get(4)?,
            parser_version: row.get(5)?,
            file_count: row.get(6)?,
            imported_files: row.get(7)?,
            partial_files: row.get(8)?,
            failed_files: row.get(9)?,
            source_bytes: row.get(10)?,
            event_count: row.get(11)?,
            warning_count: row.get(12)?,
        })
    })?;

    collect_rows(rows)
}

fn load_profile_options(
    conn: &Connection,
    selected_profile_id: Option<&str>,
) -> Result<Vec<ProfileOption>> {
    let mut statement = conn.prepare(
        "WITH profile_ids AS (
            SELECT profile_id FROM profiles
            UNION
            SELECT profile_id FROM runs
            UNION
            SELECT profile_id FROM sessions
            UNION
            SELECT profile_id FROM import_sources
         )
         SELECT
            ids.profile_id,
            COALESCE(p.profile_label, ids.profile_id) AS profile_label,
            COALESCE(p.device_id, 'local_device') AS device_id,
            d.device_label,
            p.macos_username
         FROM profile_ids ids
         LEFT JOIN profiles p ON p.profile_id = ids.profile_id
         LEFT JOIN devices d ON d.device_id = p.device_id
         WHERE ids.profile_id IS NOT NULL AND ids.profile_id != ''
         ORDER BY profile_label, ids.profile_id",
    )?;
    let rows = statement.query_map([], |row| {
        let profile_id: String = row.get(0)?;
        Ok(ProfileOption {
            is_current: selected_profile_id == Some(profile_id.as_str()),
            profile_id,
            profile_label: row.get(1)?,
            device_id: row.get(2)?,
            device_label: row.get(3)?,
            macos_username: row.get(4)?,
        })
    })?;
    collect_rows(rows)
}

fn load_source_options(conn: &Connection) -> Result<Vec<String>> {
    let mut statement = conn.prepare(
        "WITH sources AS (
            SELECT source FROM import_sources
            UNION
            SELECT source FROM usage_source_rollups
         )
         SELECT source
         FROM sources
         WHERE source IS NOT NULL AND source != ''
         ORDER BY source",
    )?;
    let rows = statement.query_map([], |row| row.get(0))?;
    collect_rows(rows)
}

fn load_model_options(conn: &Connection) -> Result<Vec<String>> {
    let mut statement = conn.prepare(
        "SELECT DISTINCT COALESCE(NULLIF(model, ''), 'unknown') AS model
         FROM usage_model_rollups
         ORDER BY model",
    )?;
    let rows = statement.query_map([], |row| row.get(0))?;
    collect_rows(rows)
}

fn load_usage_buckets(
    conn: &Connection,
    filters: &UsageFilters,
) -> Result<Vec<UsageBucketSummary>> {
    let rollup_grain = filters.chart_rollup_grain();
    let sql = if filters.model.is_some() {
        let model_where = filters.rollup_model_where_for("m", rollup_grain);
        let tool_model_where = filters.rollup_model_where_for("tm", rollup_grain);
        let model_bucket_key = filters.chart_bucket_key_expr("m", rollup_grain);
        let tool_bucket_key = filters.chart_bucket_key_expr("tm", rollup_grain);
        format!(
            "WITH model_buckets AS (
                SELECT
                    {model_bucket_key} AS bucket_key,
                    COALESCE(SUM(m.sessions), 0) AS sessions,
                    COALESCE(SUM(m.runs), 0) AS runs,
                    COALESCE(SUM(m.turns), 0) AS turns,
                    COALESCE(SUM(m.llm_calls), 0) AS llm_calls,
                    COALESCE(SUM(m.input_tokens), 0) AS input_tokens,
                    COALESCE(SUM(m.output_tokens), 0) AS output_tokens,
                    COALESCE(SUM(m.cache_read_tokens), 0) AS cache_read_tokens,
                    COALESCE(SUM(m.cache_write_tokens), 0) AS cache_write_tokens,
                    COALESCE(SUM({MODEL_ROLLUP_COST_SQL}), 0) AS total_cost_usd
                 FROM usage_model_rollups m
                 LEFT JOIN model_prices mp ON mp.model_name = m.model
                 WHERE {model_where}
                 GROUP BY 1
             ),
             tool_buckets AS (
                SELECT
                    {tool_bucket_key} AS bucket_key,
                    COALESCE(SUM(tm.calls), 0) AS tool_calls,
                    COALESCE(SUM(tm.failed_calls), 0) AS failed_tool_calls
                 FROM usage_tool_model_rollups tm
                 WHERE {tool_model_where}
                 GROUP BY 1
             )
             SELECT
                mb.bucket_key,
                mb.sessions,
                mb.runs,
                mb.turns,
                mb.llm_calls,
                COALESCE(tb.tool_calls, 0),
                COALESCE(tb.failed_tool_calls, 0),
                mb.input_tokens,
                mb.output_tokens,
                mb.cache_read_tokens,
                mb.cache_write_tokens,
                mb.total_cost_usd
             FROM model_buckets mb
             LEFT JOIN tool_buckets tb ON tb.bucket_key = mb.bucket_key
             ORDER BY mb.bucket_key DESC"
        )
    } else {
        let source_where = filters.rollup_source_where_for("s", rollup_grain);
        let model_where = filters.rollup_source_where_for("m", rollup_grain);
        let source_bucket_key = filters.chart_bucket_key_expr("s", rollup_grain);
        let model_bucket_key = filters.chart_bucket_key_expr("m", rollup_grain);
        format!(
            "WITH source_buckets AS (
                SELECT
                    {source_bucket_key} AS bucket_key,
                    COALESCE(SUM(s.sessions), 0) AS sessions,
                    COALESCE(SUM(s.runs), 0) AS runs,
                    COALESCE(SUM(s.turns), 0) AS turns,
                    COALESCE(SUM(s.llm_calls), 0) AS llm_calls,
                    COALESCE(SUM(s.tool_calls), 0) AS tool_calls,
                    COALESCE(SUM(s.failed_tool_calls), 0) AS failed_tool_calls,
                    COALESCE(SUM(s.input_tokens), 0) AS input_tokens,
                    COALESCE(SUM(s.output_tokens), 0) AS output_tokens,
                    COALESCE(SUM(s.cache_read_tokens), 0) AS cache_read_tokens,
                    COALESCE(SUM(s.cache_write_tokens), 0) AS cache_write_tokens
                 FROM usage_source_rollups s
                 WHERE {source_where}
                 GROUP BY 1
             ),
             model_costs AS (
                SELECT
                    {model_bucket_key} AS bucket_key,
                    COALESCE(SUM({MODEL_ROLLUP_COST_SQL}), 0) AS total_cost_usd
                 FROM usage_model_rollups m
                 LEFT JOIN model_prices mp ON mp.model_name = m.model
                 WHERE {model_where}
                 GROUP BY 1
             )
             SELECT
                sb.bucket_key,
                sb.sessions,
                sb.runs,
                sb.turns,
                sb.llm_calls,
                sb.tool_calls,
                sb.failed_tool_calls,
                sb.input_tokens,
                sb.output_tokens,
                sb.cache_read_tokens,
                sb.cache_write_tokens,
                COALESCE(mc.total_cost_usd, 0)
             FROM source_buckets sb
             LEFT JOIN model_costs mc ON mc.bucket_key = sb.bucket_key
             ORDER BY sb.bucket_key DESC"
        )
    };
    let mut statement = conn.prepare(&sql)?;

    let rows = statement.query_map([], |row| {
        let bucket_key: String = row.get(0)?;
        Ok(UsageBucketSummary {
            date: bucket_key.clone(),
            bucket_key,
            bucket_count: 1,
            sessions: row.get(1)?,
            runs: row.get(2)?,
            turns: row.get(3)?,
            llm_calls: row.get(4)?,
            tool_calls: row.get(5)?,
            failed_tool_calls: row.get(6)?,
            input_tokens: row.get(7)?,
            output_tokens: row.get(8)?,
            cache_read_tokens: row.get(9)?,
            cache_write_tokens: row.get(10)?,
            total_cost_usd: row.get(11)?,
        })
    })?;

    collect_rows(rows)
}

fn load_today_hourly_usage_buckets(
    conn: &Connection,
    filters: &UsageFilters,
) -> Result<Vec<UsageBucketSummary>> {
    let llm_where = filters.llm_where("l");
    let tool_where = filters.tool_where("tc");
    let sql = format!(
        "WITH RECURSIVE hours(hour) AS (
            SELECT 0
            UNION ALL
            SELECT hour + 1 FROM hours WHERE hour < 23
         ),
         hour_bounds AS (
            SELECT
                hour,
                datetime(date('now', 'localtime', 'start of day'), printf('+%d hours', hour)) AS hour_start,
                datetime(date('now', 'localtime', 'start of day'), printf('+%d hours', hour + 1)) AS hour_end
            FROM hours
         ),
         llm_buckets AS (
            SELECT
                CAST(strftime('%H', l.started_at_ns / 1000000000, 'unixepoch', 'localtime') AS INTEGER) AS hour,
                COUNT(DISTINCT l.session_id) AS sessions,
                COUNT(DISTINCT l.run_id) AS runs,
                COUNT(DISTINCT l.turn_id) AS turns,
                COUNT(*) AS llm_calls,
                COALESCE(SUM(l.input_tokens), 0) AS input_tokens,
                COALESCE(SUM(l.output_tokens), 0) AS output_tokens,
                COALESCE(SUM(l.cache_read_tokens), 0) AS cache_read_tokens,
                COALESCE(SUM(l.cache_write_tokens), 0) AS cache_write_tokens,
                COALESCE(SUM({LLM_COST_SQL}), 0) AS total_cost_usd
            FROM llm_calls l
            LEFT JOIN model_prices mp ON mp.model_name = l.model
            WHERE {llm_where}
            GROUP BY hour
         ),
         tool_buckets AS (
            SELECT
                CAST(strftime('%H', tc.started_at_ns / 1000000000, 'unixepoch', 'localtime') AS INTEGER) AS hour,
                COUNT(*) AS tool_calls,
                COALESCE(SUM(CASE WHEN tc.status = 'failed' THEN 1 ELSE 0 END), 0) AS failed_tool_calls
            FROM tool_calls tc
            WHERE {tool_where}
            GROUP BY hour
         )
         SELECT
            printf('%02d:00', hb.hour) AS bucket_key,
            COALESCE(lb.sessions, 0),
            COALESCE(lb.runs, 0),
            COALESCE(lb.turns, 0),
            COALESCE(lb.llm_calls, 0),
            COALESCE(tb.tool_calls, 0),
            COALESCE(tb.failed_tool_calls, 0),
            COALESCE(lb.input_tokens, 0),
            COALESCE(lb.output_tokens, 0),
            COALESCE(lb.cache_read_tokens, 0),
            COALESCE(lb.cache_write_tokens, 0),
            COALESCE(lb.total_cost_usd, 0)
         FROM hour_bounds hb
         LEFT JOIN llm_buckets lb ON lb.hour = hb.hour
         LEFT JOIN tool_buckets tb ON tb.hour = hb.hour
         ORDER BY hb.hour DESC",
        llm_where = llm_where,
        tool_where = tool_where,
    );
    let mut statement = conn.prepare(&sql)?;

    let rows = statement.query_map([], |row| {
        let bucket_key: String = row.get(0)?;
        Ok(UsageBucketSummary {
            date: bucket_key.clone(),
            bucket_key,
            bucket_count: 1,
            sessions: row.get(1)?,
            runs: row.get(2)?,
            turns: row.get(3)?,
            llm_calls: row.get(4)?,
            tool_calls: row.get(5)?,
            failed_tool_calls: row.get(6)?,
            input_tokens: row.get(7)?,
            output_tokens: row.get(8)?,
            cache_read_tokens: row.get(9)?,
            cache_write_tokens: row.get(10)?,
            total_cost_usd: row.get(11)?,
        })
    })?;

    collect_rows(rows)
}

fn compact_usage_buckets(buckets: Vec<UsageBucketSummary>) -> Vec<UsageBucketSummary> {
    if buckets.len() <= MAX_USAGE_BUCKETS {
        return buckets;
    }

    let mut ascending = buckets;
    ascending.reverse();
    let group_size = ascending.len().div_ceil(MAX_USAGE_BUCKETS);
    let mut compacted = Vec::with_capacity(ascending.len().div_ceil(group_size));

    for group in ascending.chunks(group_size) {
        compacted.push(aggregate_usage_bucket_group(group));
    }

    compacted.reverse();
    compacted
}

fn aggregate_usage_bucket_group(group: &[UsageBucketSummary]) -> UsageBucketSummary {
    let first = &group[0];
    let last = &group[group.len() - 1];
    let mut summary = UsageBucketSummary {
        bucket_key: if group.len() == 1 {
            first.bucket_key.clone()
        } else {
            format!("{}..{}", first.bucket_key, last.bucket_key)
        },
        date: if group.len() == 1 {
            first.date.clone()
        } else {
            format!("{} - {}", first.date, last.date)
        },
        bucket_count: 0,
        sessions: 0,
        runs: 0,
        turns: 0,
        llm_calls: 0,
        tool_calls: 0,
        failed_tool_calls: 0,
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        total_cost_usd: 0.0,
    };

    for bucket in group {
        summary.bucket_count += bucket.bucket_count;
        summary.sessions += bucket.sessions;
        summary.runs += bucket.runs;
        summary.turns += bucket.turns;
        summary.llm_calls += bucket.llm_calls;
        summary.tool_calls += bucket.tool_calls;
        summary.failed_tool_calls += bucket.failed_tool_calls;
        summary.input_tokens += bucket.input_tokens;
        summary.output_tokens += bucket.output_tokens;
        summary.cache_read_tokens += bucket.cache_read_tokens;
        summary.cache_write_tokens += bucket.cache_write_tokens;
        summary.total_cost_usd += bucket.total_cost_usd;
    }

    summary
}

fn load_source_usage(conn: &Connection) -> Result<Vec<SourceUsageSummary>> {
    load_source_usage_scoped(conn, &UsageFilters::all())
}

fn load_source_usage_scoped(
    conn: &Connection,
    filters: &UsageFilters,
) -> Result<Vec<SourceUsageSummary>> {
    if filters.preset == "today" {
        return load_source_usage_scoped_raw(conn, filters);
    }

    let sql = if filters.model.is_some() {
        let model_where = filters.rollup_model_where("m");
        let tool_model_where = filters.rollup_model_where("tm");
        let llm_where = filters.llm_where("l");
        format!(
            "SELECT
                m.source,
                COALESCE((
                    SELECT COUNT(DISTINCT l.session_id)
                    FROM llm_calls l
                    WHERE l.source = m.source AND {llm_where}
                ), 0),
                COALESCE(SUM(m.runs), 0),
                COALESCE(SUM(m.turns), 0),
                COALESCE(SUM(m.llm_calls), 0),
                COALESCE((
                    SELECT SUM(tm.calls)
                    FROM usage_tool_model_rollups tm
                    WHERE {tool_model_where} AND tm.source = m.source
                ), 0),
                COALESCE((
                    SELECT SUM(tm.failed_calls)
                    FROM usage_tool_model_rollups tm
                    WHERE {tool_model_where} AND tm.source = m.source
                ), 0),
                COALESCE(SUM(m.input_tokens), 0),
                COALESCE(SUM(m.output_tokens), 0),
                COALESCE(SUM(m.cache_read_tokens), 0),
                COALESCE(SUM(m.cache_write_tokens), 0),
                COALESCE(SUM({MODEL_ROLLUP_COST_SQL}), 0)
             FROM usage_model_rollups m
             LEFT JOIN model_prices mp ON mp.model_name = m.model
             WHERE {model_where}
             GROUP BY m.source
             ORDER BY input_tokens + output_tokens + cache_read_tokens DESC, m.source"
        )
    } else {
        let source_where = filters.rollup_source_where("s");
        let model_where = filters.rollup_source_where("m");
        let run_where = filters.run_where("r");
        format!(
            "SELECT
                s.source,
                COALESCE((
                    SELECT COUNT(DISTINCT r.session_id)
                    FROM runs r
                    WHERE r.source = s.source AND {run_where}
                ), 0),
                COALESCE(SUM(s.runs), 0),
                COALESCE(SUM(s.turns), 0),
                COALESCE(SUM(s.llm_calls), 0),
                COALESCE(SUM(s.tool_calls), 0),
                COALESCE(SUM(s.failed_tool_calls), 0),
                COALESCE(SUM(s.input_tokens), 0),
                COALESCE(SUM(s.output_tokens), 0),
                COALESCE(SUM(s.cache_read_tokens), 0),
                COALESCE(SUM(s.cache_write_tokens), 0),
                COALESCE((
                    SELECT SUM({MODEL_ROLLUP_COST_SQL})
                    FROM usage_model_rollups m
                    LEFT JOIN model_prices mp ON mp.model_name = m.model
                    WHERE {model_where} AND m.source = s.source
                ), 0)
             FROM usage_source_rollups s
             WHERE {source_where}
             GROUP BY s.source
             ORDER BY input_tokens + output_tokens + cache_read_tokens DESC, s.source"
        )
    };
    let mut statement = conn.prepare(&sql)?;

    let rows = statement.query_map([], |row| {
        Ok(SourceUsageSummary {
            source: row.get(0)?,
            sessions: row.get(1)?,
            runs: row.get(2)?,
            turns: row.get(3)?,
            llm_calls: row.get(4)?,
            tool_calls: row.get(5)?,
            failed_tool_calls: row.get(6)?,
            input_tokens: row.get(7)?,
            output_tokens: row.get(8)?,
            cache_read_tokens: row.get(9)?,
            cache_write_tokens: row.get(10)?,
            total_cost_usd: row.get(11)?,
        })
    })?;

    collect_rows(rows)
}

fn load_source_usage_scoped_raw(
    conn: &Connection,
    filters: &UsageFilters,
) -> Result<Vec<SourceUsageSummary>> {
    let sql = format!(
        "SELECT source FROM runs r WHERE {}
         UNION SELECT source FROM turns t WHERE {}
         UNION SELECT source FROM llm_calls l WHERE {}
         UNION SELECT source FROM tool_calls tc WHERE {}",
        filters.run_where("r"),
        filters.turn_where("t"),
        filters.llm_where("l"),
        filters.tool_where("tc")
    );
    let mut statement = conn.prepare(&sql)?;
    let sources = collect_rows(statement.query_map([], |row| row.get::<_, String>(0))?)?;
    let mut summaries = Vec::with_capacity(sources.len());
    for source in sources {
        let mut source_filters = filters.clone();
        source_filters.source = Some(source.clone());
        let totals = load_usage_totals_raw(conn, &source_filters)?;
        summaries.push(SourceUsageSummary {
            source,
            sessions: totals.sessions,
            runs: totals.runs,
            turns: totals.turns,
            llm_calls: totals.llm_calls,
            tool_calls: totals.tool_calls,
            failed_tool_calls: totals.failed_tool_calls,
            input_tokens: totals.input_tokens,
            output_tokens: totals.output_tokens,
            cache_read_tokens: totals.cache_read_tokens,
            cache_write_tokens: totals.cache_write_tokens,
            total_cost_usd: totals.total_cost_usd,
        });
    }
    summaries.sort_by(|left, right| {
        let left_tokens = left
            .input_tokens
            .saturating_add(left.output_tokens)
            .saturating_add(left.cache_read_tokens);
        let right_tokens = right
            .input_tokens
            .saturating_add(right.output_tokens)
            .saturating_add(right.cache_read_tokens);
        right_tokens
            .cmp(&left_tokens)
            .then_with(|| left.source.cmp(&right.source))
    });
    Ok(summaries)
}

fn load_recent_sessions(conn: &Connection) -> Result<Vec<SessionSummary>> {
    load_recent_sessions_scoped(conn, &UsageFilters::all())
}

fn load_recent_sessions_scoped(
    conn: &Connection,
    filters: &UsageFilters,
) -> Result<Vec<SessionSummary>> {
    load_recent_sessions_scoped_limit(conn, filters, 20)
}

fn load_recent_sessions_scoped_limit(
    conn: &Connection,
    filters: &UsageFilters,
    limit: i64,
) -> Result<Vec<SessionSummary>> {
    let limit = limit.clamp(1, 200);
    let session_where = filters.session_where("s");
    let runs_where = filters.run_where("r");
    let turns_where = filters.turn_where("t");
    let llm_where = filters.llm_where("l");
    let tool_where = filters.tool_where("tc");
    let sql = if filters.model.is_some() {
        format!(
            "WITH recent AS (
                SELECT
                    s.session_id,
                    s.source,
                    s.kind,
                    s.title,
                    s.first_seen_ns,
                    s.last_seen_ns
                FROM sessions s
                WHERE {session_where}
                ORDER BY s.last_seen_ns DESC
                LIMIT {limit}
             ),
             run_agg AS (
                SELECT r.session_id, COUNT(*) AS runs
                FROM runs r
                WHERE r.session_id IN (SELECT session_id FROM recent) AND {runs_where}
                GROUP BY r.session_id
             ),
             turn_agg AS (
                SELECT t.session_id, COUNT(*) AS turns
                FROM turns t
                WHERE t.session_id IN (SELECT session_id FROM recent) AND {turns_where}
                GROUP BY t.session_id
             ),
             llm_agg AS (
                SELECT
                    l.session_id,
                    COUNT(*) AS llm_calls,
                    COALESCE(SUM(l.input_tokens), 0) AS input_tokens,
                    COALESCE(SUM(l.output_tokens), 0) AS output_tokens,
                    COALESCE(SUM(l.cache_read_tokens), 0) AS cache_read_tokens,
                    COALESCE(SUM(l.cache_write_tokens), 0) AS cache_write_tokens,
                    COALESCE(SUM({LLM_COST_SQL}), 0) AS total_cost_usd,
                    COALESCE(GROUP_CONCAT(DISTINCT COALESCE(NULLIF(l.model, ''), 'unknown')), '') AS models
                FROM llm_calls l
                LEFT JOIN model_prices mp ON mp.model_name = l.model
                WHERE l.session_id IN (SELECT session_id FROM recent) AND {llm_where}
                GROUP BY l.session_id
             ),
             tool_agg AS (
                SELECT
                    tc.session_id,
                    COUNT(*) AS tool_calls,
                    COALESCE(SUM(CASE WHEN tc.status = 'failed' THEN 1 ELSE 0 END), 0) AS failed_tool_calls
                FROM tool_calls tc
                WHERE tc.session_id IN (SELECT session_id FROM recent) AND {tool_where}
                GROUP BY tc.session_id
             )
             SELECT
                recent.session_id,
                recent.source,
                recent.kind,
                recent.title,
                recent.first_seen_ns,
                recent.last_seen_ns,
                COALESCE(run_agg.runs, 0) AS runs,
                COALESCE(turn_agg.turns, 0) AS turns,
                COALESCE(llm_agg.llm_calls, 0) AS llm_calls,
                COALESCE(tool_agg.tool_calls, 0) AS tool_calls,
                COALESCE(tool_agg.failed_tool_calls, 0) AS failed_tool_calls,
                COALESCE(llm_agg.input_tokens, 0) AS input_tokens,
                COALESCE(llm_agg.output_tokens, 0) AS output_tokens,
                COALESCE(llm_agg.cache_read_tokens, 0) AS cache_read_tokens,
                COALESCE(llm_agg.cache_write_tokens, 0) AS cache_write_tokens,
                COALESCE(llm_agg.total_cost_usd, 0) AS total_cost_usd,
                COALESCE(llm_agg.models, '') AS models
             FROM recent
             LEFT JOIN run_agg ON run_agg.session_id = recent.session_id
             LEFT JOIN turn_agg ON turn_agg.session_id = recent.session_id
             LEFT JOIN llm_agg ON llm_agg.session_id = recent.session_id
             LEFT JOIN tool_agg ON tool_agg.session_id = recent.session_id
             ORDER BY recent.last_seen_ns DESC"
        )
    } else {
        format!(
            "WITH recent AS (
                SELECT
                    s.session_id,
                    s.source,
                    s.kind,
                    s.title,
                    s.first_seen_ns,
                    s.last_seen_ns
                FROM sessions s
                WHERE {session_where}
                ORDER BY s.last_seen_ns DESC
                LIMIT {limit}
             ),
             run_agg AS (
                SELECT
                    r.session_id,
                    COUNT(*) AS runs,
                    COALESCE(SUM(r.llm_call_count), 0) AS llm_calls,
                    COALESCE(SUM(r.tool_call_count), 0) AS tool_calls,
                    COALESCE(SUM(r.failed_tool_count), 0) AS failed_tool_calls,
                    COALESCE(SUM(r.input_tokens), 0) AS input_tokens,
                    COALESCE(SUM(r.output_tokens), 0) AS output_tokens,
                    COALESCE(SUM(r.cache_read_tokens), 0) AS cache_read_tokens,
                    COALESCE(SUM(r.cache_write_tokens), 0) AS cache_write_tokens,
                    COALESCE(SUM(r.total_cost_usd), 0) AS total_cost_usd
                FROM runs r
                WHERE r.session_id IN (SELECT session_id FROM recent) AND {runs_where}
                GROUP BY r.session_id
             ),
             turn_agg AS (
                SELECT t.session_id, COUNT(*) AS turns
                FROM turns t
                WHERE t.session_id IN (SELECT session_id FROM recent) AND {turns_where}
                GROUP BY t.session_id
             )
             SELECT
                recent.session_id,
                recent.source,
                recent.kind,
                recent.title,
                recent.first_seen_ns,
                recent.last_seen_ns,
                COALESCE(run_agg.runs, 0) AS runs,
                COALESCE(turn_agg.turns, 0) AS turns,
                COALESCE(run_agg.llm_calls, 0) AS llm_calls,
                COALESCE(run_agg.tool_calls, 0) AS tool_calls,
                COALESCE(run_agg.failed_tool_calls, 0) AS failed_tool_calls,
                COALESCE(run_agg.input_tokens, 0) AS input_tokens,
                COALESCE(run_agg.output_tokens, 0) AS output_tokens,
                COALESCE(run_agg.cache_read_tokens, 0) AS cache_read_tokens,
                COALESCE(run_agg.cache_write_tokens, 0) AS cache_write_tokens,
                COALESCE(run_agg.total_cost_usd, 0) AS total_cost_usd,
                COALESCE((
                    SELECT COALESCE(NULLIF(l.model, ''), 'unknown')
                    FROM llm_calls l
                    WHERE l.session_id = recent.session_id AND {llm_where}
                    ORDER BY l.started_at_ns DESC
                    LIMIT 1
                ), '') AS models
             FROM recent
             LEFT JOIN run_agg ON run_agg.session_id = recent.session_id
             LEFT JOIN turn_agg ON turn_agg.session_id = recent.session_id
             ORDER BY recent.last_seen_ns DESC"
        )
    };
    let mut statement = conn.prepare(&sql)?;

    let rows = statement.query_map([], |row| {
        let models: String = row.get(16)?;
        Ok(SessionSummary {
            session_id: row.get(0)?,
            source: row.get(1)?,
            kind: row.get(2)?,
            title: row.get(3)?,
            first_seen_ns: row.get(4)?,
            last_seen_ns: row.get(5)?,
            runs: row.get(6)?,
            turns: row.get(7)?,
            llm_calls: row.get(8)?,
            tool_calls: row.get(9)?,
            failed_tool_calls: row.get(10)?,
            input_tokens: row.get(11)?,
            output_tokens: row.get(12)?,
            cache_read_tokens: row.get(13)?,
            cache_write_tokens: row.get(14)?,
            total_cost_usd: row.get(15)?,
            models: models
                .split(',')
                .filter(|model| !model.is_empty())
                .map(str::to_string)
                .collect(),
        })
    })?;

    collect_rows(rows)
}

fn load_session_summary(conn: &Connection, session_id: &str) -> Result<Option<SessionSummary>> {
    let sql = format!(
        "WITH selected AS (
            SELECT
                s.session_id,
                s.source,
                s.kind,
                s.title,
                s.first_seen_ns,
                s.last_seen_ns
            FROM sessions s
            WHERE s.session_id = ?1
         ),
         run_agg AS (
            SELECT
                r.session_id,
                COUNT(*) AS runs,
                COALESCE(SUM(r.llm_call_count), 0) AS llm_calls,
                COALESCE(SUM(r.tool_call_count), 0) AS tool_calls,
                COALESCE(SUM(r.failed_tool_count), 0) AS failed_tool_calls
            FROM runs r
            WHERE r.session_id = ?1
            GROUP BY r.session_id
         ),
         turn_agg AS (
            SELECT t.session_id, COUNT(*) AS turns
            FROM turns t
            WHERE t.session_id = ?1
            GROUP BY t.session_id
         ),
         llm_agg AS (
            SELECT
                l.session_id,
                COALESCE(SUM(l.input_tokens), 0) AS input_tokens,
                COALESCE(SUM(l.output_tokens), 0) AS output_tokens,
                COALESCE(SUM(l.cache_read_tokens), 0) AS cache_read_tokens,
                COALESCE(SUM(l.cache_write_tokens), 0) AS cache_write_tokens,
                COALESCE(SUM({LLM_COST_SQL}), 0) AS total_cost_usd,
                COALESCE(GROUP_CONCAT(DISTINCT COALESCE(NULLIF(l.model, ''), 'unknown')), '') AS models
            FROM llm_calls l
            LEFT JOIN model_prices mp ON mp.model_name = l.model
            WHERE l.session_id = ?1
            GROUP BY l.session_id
         )
         SELECT
            selected.session_id,
            selected.source,
            selected.kind,
            selected.title,
            selected.first_seen_ns,
            selected.last_seen_ns,
            COALESCE(run_agg.runs, 0),
            COALESCE(turn_agg.turns, 0),
            COALESCE(run_agg.llm_calls, 0),
            COALESCE(run_agg.tool_calls, 0),
            COALESCE(run_agg.failed_tool_calls, 0),
            COALESCE(llm_agg.input_tokens, 0),
            COALESCE(llm_agg.output_tokens, 0),
            COALESCE(llm_agg.cache_read_tokens, 0),
            COALESCE(llm_agg.cache_write_tokens, 0),
            COALESCE(llm_agg.total_cost_usd, 0),
            COALESCE(llm_agg.models, '')
         FROM selected
         LEFT JOIN run_agg ON run_agg.session_id = selected.session_id
         LEFT JOIN turn_agg ON turn_agg.session_id = selected.session_id
         LEFT JOIN llm_agg ON llm_agg.session_id = selected.session_id"
    );

    conn.query_row(&sql, params![session_id], |row| {
        let models: String = row.get(16)?;
        Ok(SessionSummary {
            session_id: row.get(0)?,
            source: row.get(1)?,
            kind: row.get(2)?,
            title: row.get(3)?,
            first_seen_ns: row.get(4)?,
            last_seen_ns: row.get(5)?,
            runs: row.get(6)?,
            turns: row.get(7)?,
            llm_calls: row.get(8)?,
            tool_calls: row.get(9)?,
            failed_tool_calls: row.get(10)?,
            input_tokens: row.get(11)?,
            output_tokens: row.get(12)?,
            cache_read_tokens: row.get(13)?,
            cache_write_tokens: row.get(14)?,
            total_cost_usd: row.get(15)?,
            models: split_models(&models),
        })
    })
    .optional()
    .map_err(Into::into)
}

fn load_session_runs(conn: &Connection, session_id: &str) -> Result<Vec<RecentRun>> {
    let sql = format!(
        "SELECT
            r.run_id,
            r.source,
            r.kind,
            r.title,
            r.status,
            r.session_id,
            r.started_at_ns,
            r.ended_at_ns,
            r.duration_ns,
            r.llm_call_count,
            r.tool_call_count,
            r.failed_tool_count,
            r.signal_count,
            r.primary_signal,
            r.input_tokens,
            r.output_tokens,
            r.cache_read_tokens,
            COALESCE((
                SELECT SUM({LLM_COST_SQL})
                FROM llm_calls l
                LEFT JOIN model_prices mp ON mp.model_name = l.model
                WHERE l.run_id = r.run_id
            ), r.total_cost_usd) AS total_cost_usd
         FROM runs r
         WHERE r.session_id = ?1
         ORDER BY r.started_at_ns DESC, r.run_id
         LIMIT 100"
    );
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map(params![session_id], |row| {
        Ok(RecentRun {
            run_id: row.get(0)?,
            source: row.get(1)?,
            kind: row.get(2)?,
            title: row.get(3)?,
            status: row.get(4)?,
            session_id: row.get(5)?,
            started_at_ns: row.get(6)?,
            ended_at_ns: row.get(7)?,
            duration_ns: row.get(8)?,
            llm_call_count: row.get(9)?,
            tool_call_count: row.get(10)?,
            failed_tool_count: row.get(11)?,
            signal_count: row.get(12)?,
            primary_signal: row.get(13)?,
            input_tokens: row.get(14)?,
            output_tokens: row.get(15)?,
            cache_read_tokens: row.get(16)?,
            total_cost_usd: row.get(17)?,
        })
    })?;

    collect_rows(rows)
}

fn load_session_turns(conn: &Connection, session_id: &str) -> Result<Vec<TurnSummary>> {
    let sql = format!(
        "SELECT
            t.turn_id,
            t.run_id,
            t.session_id,
            t.source,
            t.turn_index,
            t.role,
            t.status,
            t.started_at_ns,
            t.ended_at_ns,
            t.duration_ns,
            COALESCE(NULLIF(t.llm_call_count, 0), COUNT(l.llm_call_id), 0) AS llm_calls,
            COALESCE(NULLIF(t.tool_call_count, 0), (
                SELECT COUNT(*) FROM tool_calls tc WHERE tc.turn_id = t.turn_id
            ), 0) AS tool_calls,
            COALESCE(NULLIF(t.failed_tool_count, 0), (
                SELECT COUNT(*) FROM tool_calls tc WHERE tc.turn_id = t.turn_id AND tc.status = 'failed'
            ), 0) AS failed_tool_calls,
            COALESCE(NULLIF(t.input_tokens, 0), COALESCE(SUM(l.input_tokens), 0)) AS input_tokens,
            COALESCE(NULLIF(t.output_tokens, 0), COALESCE(SUM(l.output_tokens), 0)) AS output_tokens,
            COALESCE(SUM(l.cache_read_tokens), 0) AS cache_read_tokens,
            COALESCE(SUM({LLM_COST_SQL}), 0) AS total_cost_usd,
            COALESCE(GROUP_CONCAT(DISTINCT COALESCE(NULLIF(l.model, ''), 'unknown')), '') AS models
         FROM turns t
         LEFT JOIN llm_calls l ON l.turn_id = t.turn_id
         LEFT JOIN model_prices mp ON mp.model_name = l.model
         WHERE t.session_id = ?1
         GROUP BY
            t.turn_id, t.run_id, t.session_id, t.source, t.turn_index, t.role, t.status,
            t.started_at_ns, t.ended_at_ns, t.duration_ns, t.llm_call_count,
            t.tool_call_count, t.failed_tool_count, t.input_tokens, t.output_tokens
         ORDER BY COALESCE(t.turn_index, 9223372036854775807), t.started_at_ns, t.turn_id
         LIMIT 200"
    );
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map(params![session_id], |row| {
        let models: String = row.get(17)?;
        Ok(TurnSummary {
            turn_id: row.get(0)?,
            run_id: row.get(1)?,
            session_id: row.get(2)?,
            source: row.get(3)?,
            turn_index: row.get(4)?,
            role: row.get(5)?,
            status: row.get(6)?,
            started_at_ns: row.get(7)?,
            ended_at_ns: row.get(8)?,
            duration_ns: row.get(9)?,
            llm_calls: row.get(10)?,
            tool_calls: row.get(11)?,
            failed_tool_calls: row.get(12)?,
            input_tokens: row.get(13)?,
            output_tokens: row.get(14)?,
            cache_read_tokens: row.get(15)?,
            total_cost_usd: row.get(16)?,
            models: split_models(&models),
        })
    })?;

    collect_rows(rows)
}

fn load_recent_runs(conn: &Connection) -> Result<Vec<RecentRun>> {
    load_recent_runs_scoped(conn, &UsageFilters::all())
}

fn load_recent_runs_scoped(conn: &Connection, filters: &UsageFilters) -> Result<Vec<RecentRun>> {
    let run_where = filters.run_where("r");
    let llm_where = filters.llm_where("l");
    let tool_where = filters.tool_where("tc");
    let sql = format!(
        "WITH recent AS (
            SELECT
                r.run_id,
                r.source,
                r.kind,
                r.title,
                r.status,
                r.session_id,
                r.started_at_ns,
                r.ended_at_ns,
                r.duration_ns,
                r.signal_count,
                r.primary_signal,
                r.total_cost_usd AS stored_cost_usd
            FROM runs r
            WHERE {run_where}
            ORDER BY r.started_at_ns DESC
            LIMIT 20
         ),
         llm_agg AS (
            SELECT
                l.run_id,
                COUNT(*) AS llm_call_count,
                COALESCE(SUM(l.input_tokens), 0) AS input_tokens,
                COALESCE(SUM(l.output_tokens), 0) AS output_tokens,
                COALESCE(SUM(l.cache_read_tokens), 0) AS cache_read_tokens,
                COALESCE(SUM({LLM_COST_SQL}), 0) AS total_cost_usd
            FROM llm_calls l
            LEFT JOIN model_prices mp ON mp.model_name = l.model
            WHERE l.run_id IN (SELECT run_id FROM recent) AND {llm_where}
            GROUP BY l.run_id
         ),
         tool_agg AS (
            SELECT
                tc.run_id,
                COUNT(*) AS tool_call_count,
                COALESCE(SUM(CASE WHEN tc.status = 'failed' THEN 1 ELSE 0 END), 0) AS failed_tool_count
            FROM tool_calls tc
            WHERE tc.run_id IN (SELECT run_id FROM recent) AND {tool_where}
            GROUP BY tc.run_id
         )
         SELECT
            recent.run_id,
            recent.source,
            recent.kind,
            recent.title,
            recent.status,
            recent.session_id,
            recent.started_at_ns,
            recent.ended_at_ns,
            recent.duration_ns,
            COALESCE(llm_agg.llm_call_count, 0) AS llm_call_count,
            COALESCE(tool_agg.tool_call_count, 0) AS tool_call_count,
            COALESCE(tool_agg.failed_tool_count, 0) AS failed_tool_count,
            recent.signal_count,
            recent.primary_signal,
            COALESCE(llm_agg.input_tokens, 0) AS input_tokens,
            COALESCE(llm_agg.output_tokens, 0) AS output_tokens,
            COALESCE(llm_agg.cache_read_tokens, 0) AS cache_read_tokens,
            COALESCE(llm_agg.total_cost_usd, recent.stored_cost_usd) AS total_cost_usd
         FROM recent
         LEFT JOIN llm_agg ON llm_agg.run_id = recent.run_id
         LEFT JOIN tool_agg ON tool_agg.run_id = recent.run_id
         ORDER BY recent.started_at_ns DESC"
    );
    let mut statement = conn.prepare(&sql)?;

    let rows = statement.query_map([], |row| {
        Ok(RecentRun {
            run_id: row.get(0)?,
            source: row.get(1)?,
            kind: row.get(2)?,
            title: row.get(3)?,
            status: row.get(4)?,
            session_id: row.get(5)?,
            started_at_ns: row.get(6)?,
            ended_at_ns: row.get(7)?,
            duration_ns: row.get(8)?,
            llm_call_count: row.get(9)?,
            tool_call_count: row.get(10)?,
            failed_tool_count: row.get(11)?,
            signal_count: row.get(12)?,
            primary_signal: row.get(13)?,
            input_tokens: row.get(14)?,
            output_tokens: row.get(15)?,
            cache_read_tokens: row.get(16)?,
            total_cost_usd: row.get(17)?,
        })
    })?;

    collect_rows(rows)
}

fn load_tool_summaries(conn: &Connection) -> Result<Vec<ToolSummary>> {
    load_tool_summaries_scoped(conn, &UsageFilters::all())
}

fn load_tool_summaries_scoped(
    conn: &Connection,
    filters: &UsageFilters,
) -> Result<Vec<ToolSummary>> {
    let sql = if filters.model.is_some() {
        let tool_where = filters.rollup_model_where("tm");
        format!(
            "SELECT
                tm.tool_name,
                COALESCE(SUM(tm.calls), 0) AS calls,
                COALESCE(SUM(tm.success_calls), 0) AS success_calls,
                COALESCE(SUM(tm.failed_calls), 0) AS failed_calls,
                0 AS last_called_at_ns
             FROM usage_tool_model_rollups tm
             WHERE {tool_where}
             GROUP BY tm.tool_name
             ORDER BY calls DESC, failed_calls DESC, tm.tool_name
             LIMIT 20"
        )
    } else {
        let tool_where = filters.rollup_source_where("t");
        format!(
            "SELECT
                t.tool_name,
                COALESCE(SUM(t.calls), 0) AS calls,
                COALESCE(SUM(t.success_calls), 0) AS success_calls,
                COALESCE(SUM(t.failed_calls), 0) AS failed_calls,
                0 AS last_called_at_ns
             FROM usage_tool_rollups t
             WHERE {tool_where}
             GROUP BY t.tool_name
             ORDER BY calls DESC, failed_calls DESC, t.tool_name
             LIMIT 20"
        )
    };
    let mut statement = conn.prepare(&sql)?;

    let rows = statement.query_map([], |row| {
        let calls: i64 = row.get(1)?;
        let failed_calls: i64 = row.get(3)?;
        Ok(ToolSummary {
            tool_name: row.get(0)?,
            calls,
            success_calls: row.get(2)?,
            failed_calls,
            failure_rate: ratio(failed_calls, calls),
            last_called_at_ns: row.get(4)?,
        })
    })?;

    collect_rows(rows)
}

fn load_tool_failures_scoped(
    conn: &Connection,
    filters: &UsageFilters,
) -> Result<Vec<ToolFailureSummary>> {
    let sql = if filters.model.is_some() {
        let tool_where = filters.rollup_model_where("tm");
        format!(
            "SELECT
                tm.source,
                tm.tool_name,
                COALESCE(SUM(tm.calls), 0) AS calls,
                COALESCE(SUM(tm.success_calls), 0) AS success_calls,
                COALESCE(SUM(tm.failed_calls), 0) AS failed_calls
             FROM usage_tool_model_rollups tm
             WHERE {tool_where}
             GROUP BY tm.source, tm.tool_name
             HAVING failed_calls > 0
             ORDER BY failed_calls DESC, CAST(failed_calls AS REAL) / NULLIF(calls, 0) DESC, calls DESC, tm.source, tm.tool_name
             LIMIT 30"
        )
    } else {
        let tool_where = filters.rollup_source_where("t");
        format!(
            "SELECT
                t.source,
                t.tool_name,
                COALESCE(SUM(t.calls), 0) AS calls,
                COALESCE(SUM(t.success_calls), 0) AS success_calls,
                COALESCE(SUM(t.failed_calls), 0) AS failed_calls
             FROM usage_tool_rollups t
             WHERE {tool_where}
             GROUP BY t.source, t.tool_name
             HAVING failed_calls > 0
             ORDER BY failed_calls DESC, CAST(failed_calls AS REAL) / NULLIF(calls, 0) DESC, calls DESC, t.source, t.tool_name
             LIMIT 30"
        )
    };
    let mut statement = conn.prepare(&sql)?;

    let rows = statement.query_map([], |row| {
        let calls: i64 = row.get(2)?;
        let failed_calls: i64 = row.get(4)?;
        Ok(ToolFailureSummary {
            source: row.get(0)?,
            tool_name: row.get(1)?,
            calls,
            success_calls: row.get(3)?,
            failed_calls,
            failure_rate: ratio(failed_calls, calls),
        })
    })?;

    collect_rows(rows)
}

fn load_skill_summaries_scoped(
    conn: &Connection,
    filters: &UsageFilters,
) -> Result<Vec<SkillSummary>> {
    let skill_where = filters.skill_where("se");
    let sql = format!(
        "SELECT
            se.source,
            se.skill_name,
            COALESCE(SUM(CASE WHEN se.event_type = 'loaded' THEN 1 ELSE 0 END), 0) AS loaded_count,
            COALESCE(SUM(CASE WHEN se.event_type = 'invoked' THEN 1 ELSE 0 END), 0) AS invoked_count,
            COALESCE(SUM(CASE WHEN se.event_type = 'attributed' THEN 1 ELSE 0 END), 0) AS attributed_count,
            COUNT(DISTINCT se.session_id) AS sessions,
            COUNT(DISTINCT se.run_id) AS runs,
            COALESCE(MAX(se.occurred_at_ns), 0) AS last_used_at_ns,
            COALESCE(AVG(se.confidence), 0) AS confidence
         FROM skill_events se
         WHERE {skill_where}
         GROUP BY se.source, se.skill_name
         ORDER BY runs DESC, sessions DESC, invoked_count DESC, loaded_count DESC,
                  attributed_count DESC, last_used_at_ns DESC, se.source, se.skill_name"
    );
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map([], |row| {
        Ok(SkillSummary {
            source: row.get(0)?,
            skill_name: row.get(1)?,
            loaded_count: row.get(2)?,
            invoked_count: row.get(3)?,
            attributed_count: row.get(4)?,
            sessions: row.get(5)?,
            runs: row.get(6)?,
            last_used_at_ns: row.get(7)?,
            confidence: row.get(8)?,
        })
    })?;

    collect_rows(rows)
}

fn load_model_summaries(conn: &Connection) -> Result<Vec<ModelSummary>> {
    load_model_summaries_scoped(conn, &UsageFilters::all())
}

fn load_model_summaries_scoped(
    conn: &Connection,
    filters: &UsageFilters,
) -> Result<Vec<ModelSummary>> {
    if filters.preset == "today" {
        let sql = format!(
            "SELECT
                COALESCE(NULLIF(l.model, ''), 'unknown') AS model,
                COUNT(*) AS calls,
                COALESCE(SUM(l.input_tokens), 0) AS input_tokens,
                COALESCE(SUM(l.output_tokens), 0) AS output_tokens,
                COALESCE(SUM(l.cache_read_tokens), 0) AS cache_read_tokens,
                COALESCE(SUM({LLM_COST_SQL}), 0) AS total_cost_usd
             FROM llm_calls l
             LEFT JOIN model_prices mp ON mp.model_name = l.model
             WHERE {}
             GROUP BY COALESCE(NULLIF(l.model, ''), 'unknown')
             ORDER BY total_cost_usd DESC,
                      input_tokens + output_tokens DESC,
                      calls DESC,
                      model
             LIMIT 20",
            filters.llm_where("l")
        );
        let mut statement = conn.prepare(&sql)?;
        let rows = statement.query_map([], |row| {
            Ok(ModelSummary {
                model: row.get(0)?,
                calls: row.get(1)?,
                input_tokens: row.get(2)?,
                output_tokens: row.get(3)?,
                cache_read_tokens: row.get(4)?,
                total_cost_usd: row.get(5)?,
            })
        })?;
        return collect_rows(rows);
    }

    let model_where = filters.rollup_model_where("m");
    let sql = format!(
        "SELECT
            m.model,
            COALESCE(SUM(m.llm_calls), 0) AS calls,
            COALESCE(SUM(m.input_tokens), 0) AS input_tokens,
            COALESCE(SUM(m.output_tokens), 0) AS output_tokens,
            COALESCE(SUM(m.cache_read_tokens), 0) AS cache_read_tokens,
            COALESCE(SUM({MODEL_ROLLUP_COST_SQL}), 0) AS total_cost_usd
         FROM usage_model_rollups m
         LEFT JOIN model_prices mp ON mp.model_name = m.model
         WHERE {model_where}
         GROUP BY m.model
         ORDER BY total_cost_usd DESC,
                  COALESCE(SUM(m.input_tokens), 0) + COALESCE(SUM(m.output_tokens), 0) DESC,
                  calls DESC,
                  model
         LIMIT 20"
    );
    let mut statement = conn.prepare(&sql)?;

    let rows = statement.query_map([], |row| {
        Ok(ModelSummary {
            model: row.get(0)?,
            calls: row.get(1)?,
            input_tokens: row.get(2)?,
            output_tokens: row.get(3)?,
            cache_read_tokens: row.get(4)?,
            total_cost_usd: row.get(5)?,
        })
    })?;

    collect_rows(rows)
}

fn load_run(conn: &Connection, run_id: &str) -> Result<Option<RecentRun>> {
    let sql = format!(
        "SELECT
            r.run_id,
            r.source,
            r.kind,
            r.title,
            r.status,
            r.session_id,
            r.started_at_ns,
            r.ended_at_ns,
            r.duration_ns,
            r.llm_call_count,
            r.tool_call_count,
            r.failed_tool_count,
            r.signal_count,
            r.primary_signal,
            r.input_tokens,
            r.output_tokens,
            r.cache_read_tokens,
            COALESCE((
                SELECT SUM({LLM_COST_SQL})
                FROM llm_calls l
                LEFT JOIN model_prices mp ON mp.model_name = l.model
                WHERE l.run_id = r.run_id
            ), r.total_cost_usd) AS total_cost_usd
         FROM runs r
         WHERE r.run_id = ?1"
    );
    conn.query_row(&sql, params![run_id], |row| {
        Ok(RecentRun {
            run_id: row.get(0)?,
            source: row.get(1)?,
            kind: row.get(2)?,
            title: row.get(3)?,
            status: row.get(4)?,
            session_id: row.get(5)?,
            started_at_ns: row.get(6)?,
            ended_at_ns: row.get(7)?,
            duration_ns: row.get(8)?,
            llm_call_count: row.get(9)?,
            tool_call_count: row.get(10)?,
            failed_tool_count: row.get(11)?,
            signal_count: row.get(12)?,
            primary_signal: row.get(13)?,
            input_tokens: row.get(14)?,
            output_tokens: row.get(15)?,
            cache_read_tokens: row.get(16)?,
            total_cost_usd: row.get(17)?,
        })
    })
    .optional()
    .map_err(Into::into)
}

fn load_run_steps(conn: &Connection, run_id: &str) -> Result<Vec<RunStep>> {
    let sql = format!(
        "SELECT *
         FROM (
            SELECT
                step_id,
                step_type,
                name,
                status,
                error_type,
                source_event_id,
                source_ref,
                started_at_ns,
                ended_at_ns,
                duration_ns,
                order_index,
                llm_call_id,
                tool_call_id,
                input_tokens,
                output_tokens,
                cost_usd,
                metadata_json
            FROM run_steps
            WHERE run_id = ?1
              AND llm_call_id IS NULL
              AND tool_call_id IS NULL
            UNION ALL
            SELECT
                l.llm_call_id AS step_id,
                'llm' AS step_type,
                COALESCE(NULLIF(l.model, ''), l.operation, 'llm') AS name,
                l.status AS status,
                l.error_type AS error_type,
                NULL AS source_event_id,
                NULL AS source_ref,
                l.started_at_ns AS started_at_ns,
                l.ended_at_ns AS ended_at_ns,
                l.duration_ns AS duration_ns,
                NULL AS order_index,
                l.llm_call_id AS llm_call_id,
                NULL AS tool_call_id,
                l.input_tokens AS input_tokens,
                l.output_tokens AS output_tokens,
                {LLM_COST_SQL} AS cost_usd,
                NULL AS metadata_json
            FROM llm_calls l
            LEFT JOIN model_prices mp ON mp.model_name = l.model
            WHERE l.run_id = ?1
            UNION ALL
            SELECT
                tc.tool_call_id AS step_id,
                'tool' AS step_type,
                tc.tool_name AS name,
                tc.status AS status,
                tc.error_type AS error_type,
                NULL AS source_event_id,
                NULL AS source_ref,
                tc.started_at_ns AS started_at_ns,
                tc.ended_at_ns AS ended_at_ns,
                tc.duration_ns AS duration_ns,
                NULL AS order_index,
                NULL AS llm_call_id,
                tc.tool_call_id AS tool_call_id,
                0 AS input_tokens,
                0 AS output_tokens,
                0 AS cost_usd,
                NULL AS metadata_json
            FROM tool_calls tc
            WHERE tc.run_id = ?1
         )
         ORDER BY started_at_ns,
                  COALESCE(order_index, 9223372036854775807),
                  step_type,
                  step_id"
    );
    let mut statement = conn.prepare(&sql)?;

    let rows = statement.query_map(params![run_id], |row| {
        Ok(RunStep {
            step_id: row.get(0)?,
            step_type: row.get(1)?,
            name: row.get(2)?,
            status: row.get(3)?,
            error_type: row.get(4)?,
            source_event_id: row.get(5)?,
            source_ref: row.get(6)?,
            started_at_ns: row.get(7)?,
            ended_at_ns: row.get(8)?,
            duration_ns: row.get(9)?,
            order_index: row.get(10)?,
            llm_call_id: row.get(11)?,
            tool_call_id: row.get(12)?,
            input_tokens: row.get(13)?,
            output_tokens: row.get(14)?,
            cost_usd: row.get(15)?,
            metadata_json: row.get(16)?,
        })
    })?;

    collect_rows(rows)
}

fn load_top_signals(conn: &Connection) -> Result<Vec<SignalSummary>> {
    let mut statement = conn.prepare(
        "SELECT signal_type, severity, COUNT(*) AS count
         FROM run_signals
         GROUP BY signal_type, severity
         ORDER BY CASE severity WHEN 'high' THEN 3 WHEN 'medium' THEN 2 ELSE 1 END DESC,
                  count DESC,
                  signal_type
         LIMIT 10",
    )?;

    let rows = statement.query_map([], |row| {
        Ok(SignalSummary {
            signal_type: row.get(0)?,
            severity: row.get(1)?,
            count: row.get(2)?,
        })
    })?;

    collect_rows(rows)
}

fn load_run_signals(conn: &Connection, run_id: &str) -> Result<Vec<RunSignal>> {
    let mut statement = conn.prepare(
        "SELECT
            signal_id,
            signal_type,
            severity,
            title,
            evidence_json,
            suggestion,
            created_at_ns
         FROM run_signals
         WHERE run_id = ?1
         ORDER BY CASE severity WHEN 'high' THEN 3 WHEN 'medium' THEN 2 ELSE 1 END DESC,
                  created_at_ns DESC",
    )?;

    let rows = statement.query_map(params![run_id], |row| {
        Ok(RunSignal {
            signal_id: row.get(0)?,
            signal_type: row.get(1)?,
            severity: row.get(2)?,
            title: row.get(3)?,
            evidence_json: row.get(4)?,
            suggestion: row.get(5)?,
            created_at_ns: row.get(6)?,
        })
    })?;

    collect_rows(rows)
}

fn load_llm_calls(conn: &Connection, run_id: &str) -> Result<Vec<LlmCall>> {
    let sql = format!(
        "SELECT
            l.llm_call_id,
            l.provider,
            l.model,
            l.operation,
            l.status,
            l.error_type,
            l.input_tokens,
            l.output_tokens,
            l.reasoning_tokens,
            l.uncached_input_tokens,
            l.cache_read_tokens,
            l.cache_write_tokens,
            l.cache_ratio,
            l.model_context_window,
            l.context_window_percent,
            {LLM_COST_SQL} AS total_cost_usd,
            l.pricing_status,
            l.cost_confidence,
            l.started_at_ns,
            l.ended_at_ns,
            l.duration_ns,
            l.metadata_json
         FROM llm_calls l
         LEFT JOIN model_prices mp ON mp.model_name = l.model
         WHERE l.run_id = ?1
         ORDER BY l.started_at_ns, l.llm_call_id"
    );
    let mut statement = conn.prepare(&sql)?;

    let rows = statement.query_map(params![run_id], |row| {
        Ok(LlmCall {
            llm_call_id: row.get(0)?,
            provider: row.get(1)?,
            model: row.get(2)?,
            operation: row.get(3)?,
            status: row.get(4)?,
            error_type: row.get(5)?,
            input_tokens: row.get(6)?,
            output_tokens: row.get(7)?,
            reasoning_tokens: row.get(8)?,
            uncached_input_tokens: row.get(9)?,
            cache_read_tokens: row.get(10)?,
            cache_write_tokens: row.get(11)?,
            cache_ratio: row.get(12)?,
            model_context_window: row.get(13)?,
            context_window_percent: row.get(14)?,
            total_cost_usd: row.get(15)?,
            pricing_status: row.get(16)?,
            cost_confidence: row.get(17)?,
            started_at_ns: row.get(18)?,
            ended_at_ns: row.get(19)?,
            duration_ns: row.get(20)?,
            metadata_json: row.get(21)?,
        })
    })?;

    collect_rows(rows)
}

fn load_tool_calls(conn: &Connection, run_id: &str) -> Result<Vec<ToolCall>> {
    let mut statement = conn.prepare(
        "SELECT
            tool_call_id,
            tool_name,
            status,
            error_type,
            output_bytes,
            started_at_ns,
            ended_at_ns,
            duration_ns,
            metadata_json
         FROM tool_calls
         WHERE run_id = ?1
         ORDER BY started_at_ns, tool_call_id",
    )?;

    let rows = statement.query_map(params![run_id], |row| {
        Ok(ToolCall {
            tool_call_id: row.get(0)?,
            tool_name: row.get(1)?,
            status: row.get(2)?,
            error_type: row.get(3)?,
            output_bytes: row.get(4)?,
            started_at_ns: row.get(5)?,
            ended_at_ns: row.get(6)?,
            duration_ns: row.get(7)?,
            metadata_json: row.get(8)?,
        })
    })?;

    collect_rows(rows)
}

fn load_session_llm_calls(conn: &Connection, session_id: &str) -> Result<Vec<LlmCall>> {
    let sql = format!(
        "SELECT
            l.llm_call_id,
            l.provider,
            l.model,
            l.operation,
            l.status,
            l.error_type,
            l.input_tokens,
            l.output_tokens,
            l.reasoning_tokens,
            l.uncached_input_tokens,
            l.cache_read_tokens,
            l.cache_write_tokens,
            l.cache_ratio,
            l.model_context_window,
            l.context_window_percent,
            {LLM_COST_SQL} AS total_cost_usd,
            l.pricing_status,
            l.cost_confidence,
            l.started_at_ns,
            l.ended_at_ns,
            l.duration_ns,
            l.metadata_json
         FROM llm_calls l
         LEFT JOIN model_prices mp ON mp.model_name = l.model
         WHERE l.session_id = ?1
         ORDER BY l.started_at_ns, l.llm_call_id
         LIMIT 300"
    );
    let mut statement = conn.prepare(&sql)?;

    let rows = statement.query_map(params![session_id], |row| {
        Ok(LlmCall {
            llm_call_id: row.get(0)?,
            provider: row.get(1)?,
            model: row.get(2)?,
            operation: row.get(3)?,
            status: row.get(4)?,
            error_type: row.get(5)?,
            input_tokens: row.get(6)?,
            output_tokens: row.get(7)?,
            reasoning_tokens: row.get(8)?,
            uncached_input_tokens: row.get(9)?,
            cache_read_tokens: row.get(10)?,
            cache_write_tokens: row.get(11)?,
            cache_ratio: row.get(12)?,
            model_context_window: row.get(13)?,
            context_window_percent: row.get(14)?,
            total_cost_usd: row.get(15)?,
            pricing_status: row.get(16)?,
            cost_confidence: row.get(17)?,
            started_at_ns: row.get(18)?,
            ended_at_ns: row.get(19)?,
            duration_ns: row.get(20)?,
            metadata_json: row.get(21)?,
        })
    })?;

    collect_rows(rows)
}

fn load_session_tool_calls(conn: &Connection, session_id: &str) -> Result<Vec<ToolCall>> {
    let mut statement = conn.prepare(
        "SELECT
            tool_call_id,
            tool_name,
            status,
            error_type,
            output_bytes,
            started_at_ns,
            ended_at_ns,
            duration_ns,
            metadata_json
         FROM tool_calls
         WHERE session_id = ?1
         ORDER BY started_at_ns, tool_call_id
         LIMIT 300",
    )?;

    let rows = statement.query_map(params![session_id], |row| {
        Ok(ToolCall {
            tool_call_id: row.get(0)?,
            tool_name: row.get(1)?,
            status: row.get(2)?,
            error_type: row.get(3)?,
            output_bytes: row.get(4)?,
            started_at_ns: row.get(5)?,
            ended_at_ns: row.get(6)?,
            duration_ns: row.get(7)?,
            metadata_json: row.get(8)?,
        })
    })?;

    collect_rows(rows)
}

fn load_session_skill_events(conn: &Connection, session_id: &str) -> Result<Vec<SkillEventRecord>> {
    let mut statement = conn.prepare(
        "SELECT
            skill_event_id,
            source,
            skill_name,
            event_type,
            confidence,
            session_id,
            run_id,
            turn_id,
            source_event_id,
            source_ref,
            occurred_at_ns,
            metadata_json
         FROM skill_events
         WHERE session_id = ?1
         ORDER BY occurred_at_ns, skill_name, event_type
         LIMIT 300",
    )?;

    let rows = statement.query_map(params![session_id], |row| {
        Ok(SkillEventRecord {
            skill_event_id: row.get(0)?,
            source: row.get(1)?,
            skill_name: row.get(2)?,
            event_type: row.get(3)?,
            confidence: row.get(4)?,
            session_id: row.get(5)?,
            run_id: row.get(6)?,
            turn_id: row.get(7)?,
            source_event_id: row.get(8)?,
            source_ref: row.get(9)?,
            occurred_at_ns: row.get(10)?,
            metadata_json: row.get(11)?,
        })
    })?;

    collect_rows(rows)
}

fn load_session_timeline(conn: &Connection, session_id: &str) -> Result<Vec<SessionTimelineEvent>> {
    let sql = format!(
        "SELECT *
         FROM (
            SELECT *
            FROM (
            SELECT
                l.llm_call_id AS event_id,
                'llm' AS event_type,
                l.run_id AS run_id,
                l.turn_id AS turn_id,
                COALESCE(NULLIF(l.model, ''), l.operation, 'llm') AS label,
                l.status AS status,
                l.error_type AS error_type,
                l.started_at_ns AS started_at_ns,
                l.ended_at_ns AS ended_at_ns,
                l.duration_ns AS duration_ns,
                l.input_tokens AS input_tokens,
                l.output_tokens AS output_tokens,
                l.cache_read_tokens AS cache_read_tokens,
                {LLM_COST_SQL} AS total_cost_usd,
                COALESCE(NULLIF(l.model, ''), 'unknown') AS model,
                NULL AS tool_name,
                NULL AS skill_name,
                NULL AS skill_event_type
            FROM llm_calls l
            LEFT JOIN model_prices mp ON mp.model_name = l.model
            WHERE l.session_id = ?1
            UNION ALL
            SELECT
                tc.tool_call_id AS event_id,
                'tool' AS event_type,
                tc.run_id AS run_id,
                tc.turn_id AS turn_id,
                tc.tool_name AS label,
                tc.status AS status,
                tc.error_type AS error_type,
                tc.started_at_ns AS started_at_ns,
                tc.ended_at_ns AS ended_at_ns,
                tc.duration_ns AS duration_ns,
                0 AS input_tokens,
                0 AS output_tokens,
                0 AS cache_read_tokens,
                0 AS total_cost_usd,
                NULL AS model,
                tc.tool_name AS tool_name,
                NULL AS skill_name,
                NULL AS skill_event_type
            FROM tool_calls tc
            WHERE tc.session_id = ?1
            UNION ALL
            SELECT
                se.skill_event_id AS event_id,
                'skill' AS event_type,
                se.run_id AS run_id,
                se.turn_id AS turn_id,
                se.skill_name || ' ' || se.event_type AS label,
                se.event_type AS status,
                NULL AS error_type,
                se.occurred_at_ns AS started_at_ns,
                NULL AS ended_at_ns,
                NULL AS duration_ns,
                0 AS input_tokens,
                0 AS output_tokens,
                0 AS cache_read_tokens,
                0 AS total_cost_usd,
                NULL AS model,
                NULL AS tool_name,
                se.skill_name AS skill_name,
                se.event_type AS skill_event_type
            FROM skill_events se
            WHERE se.session_id = ?1
            )
            ORDER BY started_at_ns DESC, event_type, event_id
            LIMIT 300
         )
         ORDER BY started_at_ns, event_type, event_id
         "
    );
    let mut statement = conn.prepare(&sql)?;

    let rows = statement.query_map(params![session_id], |row| {
        Ok(SessionTimelineEvent {
            event_id: row.get(0)?,
            event_type: row.get(1)?,
            run_id: row.get(2)?,
            turn_id: row.get(3)?,
            label: row.get(4)?,
            status: row.get(5)?,
            error_type: row.get(6)?,
            started_at_ns: row.get(7)?,
            ended_at_ns: row.get(8)?,
            duration_ns: row.get(9)?,
            input_tokens: row.get(10)?,
            output_tokens: row.get(11)?,
            cache_read_tokens: row.get(12)?,
            total_cost_usd: row.get(13)?,
            model: row.get(14)?,
            tool_name: row.get(15)?,
            skill_name: row.get(16)?,
            skill_event_type: row.get(17)?,
        })
    })?;

    collect_rows(rows)
}

fn load_session_tool_summaries(conn: &Connection, session_id: &str) -> Result<Vec<ToolSummary>> {
    let mut statement = conn.prepare(
        "SELECT
            tool_name,
            COUNT(*) AS calls,
            COALESCE(SUM(CASE WHEN status = 'success' THEN 1 ELSE 0 END), 0) AS success_calls,
            COALESCE(SUM(CASE WHEN status = 'failed' THEN 1 ELSE 0 END), 0) AS failed_calls,
            COALESCE(MAX(started_at_ns), 0) AS last_called_at_ns
         FROM tool_calls
         WHERE session_id = ?1
         GROUP BY tool_name
         ORDER BY calls DESC, failed_calls DESC, tool_name
         LIMIT 30",
    )?;

    let rows = statement.query_map(params![session_id], |row| {
        let calls: i64 = row.get(1)?;
        let failed_calls: i64 = row.get(3)?;
        Ok(ToolSummary {
            tool_name: row.get(0)?,
            calls,
            success_calls: row.get(2)?,
            failed_calls,
            failure_rate: ratio(failed_calls, calls),
            last_called_at_ns: row.get(4)?,
        })
    })?;

    collect_rows(rows)
}

fn load_session_skill_summaries(conn: &Connection, session_id: &str) -> Result<Vec<SkillSummary>> {
    let mut statement = conn.prepare(
        "SELECT
            source,
            skill_name,
            COALESCE(SUM(CASE WHEN event_type = 'loaded' THEN 1 ELSE 0 END), 0) AS loaded_count,
            COALESCE(SUM(CASE WHEN event_type = 'invoked' THEN 1 ELSE 0 END), 0) AS invoked_count,
            COALESCE(SUM(CASE WHEN event_type = 'attributed' THEN 1 ELSE 0 END), 0) AS attributed_count,
            COUNT(DISTINCT session_id) AS sessions,
            COUNT(DISTINCT run_id) AS runs,
            COALESCE(MAX(occurred_at_ns), 0) AS last_used_at_ns,
            COALESCE(AVG(confidence), 0) AS confidence
         FROM skill_events
         WHERE session_id = ?1
         GROUP BY source, skill_name
         ORDER BY runs DESC, invoked_count DESC, loaded_count DESC, skill_name
         LIMIT 30",
    )?;

    let rows = statement.query_map(params![session_id], |row| {
        Ok(SkillSummary {
            source: row.get(0)?,
            skill_name: row.get(1)?,
            loaded_count: row.get(2)?,
            invoked_count: row.get(3)?,
            attributed_count: row.get(4)?,
            sessions: row.get(5)?,
            runs: row.get(6)?,
            last_used_at_ns: row.get(7)?,
            confidence: row.get(8)?,
        })
    })?;

    collect_rows(rows)
}

fn load_session_model_summaries(conn: &Connection, session_id: &str) -> Result<Vec<ModelSummary>> {
    let sql = format!(
        "SELECT
            COALESCE(NULLIF(l.model, ''), 'unknown') AS model,
            COUNT(*) AS calls,
            COALESCE(SUM(l.input_tokens), 0) AS input_tokens,
            COALESCE(SUM(l.output_tokens), 0) AS output_tokens,
            COALESCE(SUM(l.cache_read_tokens), 0) AS cache_read_tokens,
            COALESCE(SUM({LLM_COST_SQL}), 0) AS total_cost_usd
         FROM llm_calls l
         LEFT JOIN model_prices mp ON mp.model_name = l.model
         WHERE l.session_id = ?1
         GROUP BY COALESCE(NULLIF(l.model, ''), 'unknown')
         ORDER BY total_cost_usd DESC,
                  input_tokens + output_tokens DESC,
                  calls DESC,
                  model
         LIMIT 30"
    );
    let mut statement = conn.prepare(&sql)?;

    let rows = statement.query_map(params![session_id], |row| {
        Ok(ModelSummary {
            model: row.get(0)?,
            calls: row.get(1)?,
            input_tokens: row.get(2)?,
            output_tokens: row.get(3)?,
            cache_read_tokens: row.get(4)?,
            total_cost_usd: row.get(5)?,
        })
    })?;

    collect_rows(rows)
}

fn count(conn: &Connection, table: &str) -> Result<i64> {
    let sql = format!("SELECT COUNT(*) FROM {table}");
    Ok(conn.query_row(&sql, [], |row| row.get(0))?)
}

fn latest_import_scan_ns(conn: &Connection) -> Result<Option<i64>> {
    Ok(
        conn.query_row("SELECT MAX(last_scan_ns) FROM import_sources", [], |row| {
            row.get(0)
        })?,
    )
}

fn count_where(conn: &Connection, table: &str, predicate: &str) -> Result<i64> {
    let sql = format!("SELECT COUNT(*) FROM {table} WHERE {predicate}");
    Ok(conn.query_row(&sql, [], |row| row.get(0))?)
}

fn sum_i64(conn: &Connection, table: &str, column: &str) -> Result<i64> {
    let sql = format!("SELECT COALESCE(SUM({column}), 0) FROM {table}");
    Ok(conn.query_row(&sql, [], |row| row.get(0))?)
}

fn sum_llm_cost(conn: &Connection) -> Result<f64> {
    let sql = format!(
        "SELECT COALESCE(SUM({LLM_COST_SQL}), 0)
         FROM llm_calls l
         LEFT JOIN model_prices mp ON mp.model_name = l.model"
    );
    Ok(conn.query_row(&sql, [], |row| row.get(0))?)
}

fn sum_model_rollup_cost_where(conn: &Connection, predicate: &str) -> Result<f64> {
    let sql = format!(
        "SELECT COALESCE(SUM({MODEL_ROLLUP_COST_SQL}), 0)
         FROM usage_model_rollups m
         LEFT JOIN model_prices mp ON mp.model_name = m.model
         WHERE {predicate}"
    );
    Ok(conn.query_row(&sql, [], |row| row.get(0))?)
}

fn count_sql(conn: &Connection, sql: &str) -> Result<i64> {
    Ok(conn.query_row(sql, [], |row| row.get(0))?)
}

fn normalize_filter_value(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim();
        if value.is_empty() || value == "all" {
            None
        } else {
            Some(value.to_string())
        }
    })
}

fn normalize_model_filter_value(value: Option<String>) -> Option<String> {
    let value = normalize_filter_value(value)?;
    if value == "unknown" {
        return Some(value);
    }
    if let Some((_, suffix)) = value.rsplit_once('/') {
        return normalize_filter_value(Some(suffix.to_string()));
    }
    if let Some((_, suffix)) = value.rsplit_once(':') {
        return normalize_filter_value(Some(suffix.to_string()));
    }
    Some(value)
}

fn normalize_date_value(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim();
        if parse_date_parts(value).is_some() {
            Some(value.to_string())
        } else {
            None
        }
    })
}

fn split_models(models: &str) -> Vec<String> {
    models
        .split(',')
        .filter(|model| !model.is_empty())
        .map(str::to_string)
        .collect()
}

fn parse_date_parts(value: &str) -> Option<(i32, u32, u32)> {
    if value.len() != 10 {
        return None;
    }
    let bytes = value.as_bytes();
    if bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    if !bytes
        .iter()
        .enumerate()
        .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
    {
        return None;
    }

    let year: i32 = value[0..4].parse().ok()?;
    let month: u32 = value[5..7].parse().ok()?;
    let day: u32 = value[8..10].parse().ok()?;
    if !(1..=12).contains(&month) {
        return None;
    }
    let max_day = days_in_month(year, month);
    if day == 0 || day > max_day {
        return None;
    }
    Some((year, month, day))
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_since_epoch(value: &str) -> Option<i64> {
    let (year, month, day) = parse_date_parts(value)?;
    Some(days_from_civil(year, month, day))
}

fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let year = year - i32::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let month = month as i32;
    let day = day as i32;
    let mp = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    i64::from(era * 146097 + doe - 719468)
}

fn sql_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn date_expr_start_ns_sql(date_expr: &str) -> String {
    format!(
        "(CAST(strftime('%s', {}, 'utc') AS INTEGER) * 1000000000)",
        date_expr
    )
}

fn join_conditions(conditions: Vec<String>) -> String {
    let conditions: Vec<String> = conditions
        .into_iter()
        .filter(|condition| !condition.trim().is_empty())
        .collect();
    if conditions.is_empty() {
        "1 = 1".to_string()
    } else {
        conditions.join(" AND ")
    }
}

fn ratio(numerator: i64, denominator: i64) -> f64 {
    if denominator <= 0 {
        return 0.0;
    }

    numerator as f64 / denominator as f64
}

fn optional_ratio(numerator: i64, denominator: i64) -> Option<f64> {
    if denominator <= 0 {
        None
    } else {
        Some(numerator as f64 / denominator as f64)
    }
}

fn collect_rows<T>(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>>,
) -> Result<Vec<T>> {
    let mut values = Vec::new();
    for row in rows {
        values.push(row?);
    }
    Ok(values)
}

fn api_error(error: anyhow::Error) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse {
            status: "error",
            message: error.to_string(),
        }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf, sync::Mutex};

    use anyhow::Result;

    use crate::{
        config::{Identity, SourcePaths},
        db::Database,
        importers::{ImportReport, ImportTiming},
        projection::{
            event::{
                EntityHint, NormalizedEvent, Operation, OperationStatus, OperationType,
                SkillEventHint, SkillEventType, TraceContext, Usage,
            },
            projector::Projector,
        },
        rollup, workspace,
    };

    use super::{
        AppState, LatencySample, MenubarQuery, SyncStatus, UsageFilters, UsageQuery, load_menubar,
        load_overview, load_pattern_latency_panel, load_run_detail, load_session_detail,
        load_sessions, load_today_latency_panel, load_usage, load_workspace_status,
        repair_workspace_inner, sync_source_report,
    };

    #[test]
    fn amp_sync_report_preserves_local_fallback_mode_and_thread_counters() -> Result<()> {
        let report = ImportReport {
            source: "amp".into(),
            source_id: "amp-source".into(),
            root_path: PathBuf::from("<amp-local>"),
            files_seen: 9,
            files_imported: 4,
            files_skipped: 5,
            events_projected: 12,
            source_bytes_scanned: 345,
            shirabe_bytes_written: 678,
            mode: Some("local_fallback"),
            threads_enumerated: Some(11),
            threads_exported: Some(7),
            unchanged_threads_skipped: Some(3),
            timings: vec![ImportTiming {
                stage: "total",
                elapsed_ms: 1,
            }],
            warnings: Vec::new(),
        };

        let value = serde_json::to_value(sync_source_report("amp", Ok(report), 23))?;
        assert_eq!(value["mode"], "local_fallback");
        assert_eq!(value["threads_enumerated"], 11);
        assert_eq!(value["threads_exported"], 7);
        assert_eq!(value["unchanged_threads_skipped"], 3);

        let error = serde_json::to_value(sync_source_report(
            "amp",
            Err(anyhow::anyhow!("failed")),
            23,
        ))?;
        assert!(error.get("mode").is_none());
        Ok(())
    }

    #[test]
    fn overview_reads_sqlite_totals_and_recent_runs() -> Result<()> {
        let db_path = temp_db_path("overview");
        let db = Database::open(&db_path)?;
        db.migrate()?;
        db.record_import_source(
            "kanade",
            "otlp_jsonl_file",
            PathBuf::from("trace.jsonl").as_path(),
        )?;

        let event = NormalizedEvent {
            profile_id: "local".to_string(),
            device_id: "local_device".to_string(),
            source: "kanade".to_string(),
            source_kind: "otlp_jsonl_span".to_string(),
            source_event_id: Some("span-1".to_string()),
            observed_at_ns: 100,
            occurred_at_ns: 100,
            source_ref: Some("trace.jsonl:1".to_string()),
            cwd: None,
            project_id: None,
            trace: TraceContext::default(),
            session: Some(EntityHint {
                external_id: "session-1".to_string(),
                kind: "workflow".to_string(),
                title: Some("Session".to_string()),
            }),
            run: EntityHint {
                external_id: "run-1".to_string(),
                kind: "workflow".to_string(),
                title: Some("Run".to_string()),
            },
            turn: None,
            operation: Operation {
                operation_type: OperationType::LlmCall,
                name: "workflow.agent".to_string(),
                status: OperationStatus::Success,
                error_type: None,
                started_at_ns: 100,
                ended_at_ns: Some(200),
                duration_ns: Some(100),
                order_index: Some(0),
                metadata: None,
            },
            usage: Usage {
                input_tokens: 180000,
                output_tokens: 5,
                cache_read_tokens: 5000,
                model_context_window: Some(200000),
                context_window_percent: Some(0.9),
                total_cost_usd: 0.01,
                ..Usage::default()
            },
            skill_events: Vec::new(),
            confidence: 1.0,
        };
        Projector::new(&db).project(&event)?;

        let overview = load_overview(&db_path)?;

        assert_eq!(overview.status, "ok");
        assert_eq!(overview.totals.import_sources, 1);
        assert_eq!(overview.totals.runs, 1);
        assert_eq!(overview.totals.llm_calls, 1);
        assert_eq!(overview.totals.signals, 2);
        assert_eq!(overview.totals.input_tokens, 180000);
        assert!(!overview.top_signals.is_empty());
        assert_eq!(overview.recent_runs.len(), 1);
        assert_eq!(overview.recent_runs[0].run_id, "kanade:run-1");
        assert_eq!(overview.recent_runs[0].signal_count, 2);

        let detail = load_run_detail(&db_path, "kanade:run-1")?.expect("run detail exists");
        assert_eq!(detail.status, "ok");
        assert_eq!(detail.run.run_id, "kanade:run-1");
        assert_eq!(detail.signals.len(), 2);
        assert_eq!(detail.timeline.len(), 1);
        assert_eq!(detail.llm_calls.len(), 1);
        assert_eq!(detail.tool_calls.len(), 0);
        assert!(load_run_detail(&db_path, "missing")?.is_none());

        let _ = fs::remove_file(db_path);
        Ok(())
    }

    #[test]
    fn usage_reads_summary_and_buckets_from_rollups() -> Result<()> {
        let db_path = temp_db_path("usage");
        let db = Database::open(&db_path)?;
        db.migrate()?;

        let event = NormalizedEvent {
            profile_id: "local".to_string(),
            device_id: "local_device".to_string(),
            source: "codex".to_string(),
            source_kind: "session_jsonl_event".to_string(),
            source_event_id: Some("event-1".to_string()),
            observed_at_ns: 100,
            occurred_at_ns: 100,
            source_ref: Some("session.jsonl:1".to_string()),
            cwd: None,
            project_id: None,
            trace: TraceContext::default(),
            session: Some(EntityHint {
                external_id: "session-1".to_string(),
                kind: "session".to_string(),
                title: None,
            }),
            run: EntityHint {
                external_id: "run-1".to_string(),
                kind: "session".to_string(),
                title: None,
            },
            turn: None,
            operation: Operation {
                operation_type: OperationType::LlmCall,
                name: "assistant".to_string(),
                status: OperationStatus::Success,
                error_type: None,
                started_at_ns: 100,
                ended_at_ns: Some(200),
                duration_ns: Some(100),
                order_index: Some(0),
                metadata: None,
            },
            usage: Usage {
                model: Some("gpt-test".to_string()),
                input_tokens: 1000,
                output_tokens: 250,
                cache_read_tokens: 400,
                cache_write_tokens: 50,
                total_cost_usd: 0.02,
                ..Usage::default()
            },
            skill_events: vec![SkillEventHint {
                skill_name: "imagegen".to_string(),
                event_type: SkillEventType::Loaded,
                confidence: 0.75,
                metadata: None,
            }],
            confidence: 1.0,
        };
        Projector::new(&db).project(&event)?;
        rollup::refresh_all(&db)?;

        let usage = load_usage(&db_path, UsageFilters::all())?;

        assert_eq!(usage.status, "ok");
        assert_eq!(usage.usage_grain, "month");
        assert_eq!(usage.summary.sessions, 1);
        assert_eq!(usage.summary.runs, 1);
        assert_eq!(usage.summary.llm_calls, 1);
        assert_eq!(usage.summary.input_tokens, 1000);
        assert_eq!(usage.summary.output_tokens, 250);
        assert_eq!(usage.summary.total_tokens, 1250);
        assert_eq!(usage.summary.cache_read_tokens, 400);
        assert_eq!(usage.summary.cache_write_tokens, 50);
        assert_eq!(usage.summary.cache_hit_rate, Some(0.4));
        assert_eq!(usage.summary.total_cost_usd, 0.02);
        assert_eq!(usage.buckets.len(), 1);
        assert_eq!(usage.buckets[0].bucket_key, "1970-01");
        assert_eq!(usage.buckets[0].input_tokens, 1000);
        assert_eq!(usage.buckets[0].output_tokens, 250);
        assert_eq!(usage.skill_summaries.len(), 1);
        assert_eq!(usage.skill_summaries[0].skill_name, "imagegen");
        assert_eq!(usage.skill_summaries[0].loaded_count, 1);
        assert_eq!(usage.skill_summaries[0].invoked_count, 0);
        assert_eq!(usage.skill_summaries[0].confidence, 0.75);

        let custom = UsageFilters::from_query(UsageQuery {
            preset: Some("custom".to_string()),
            range: None,
            from: Some("1970-01-01".to_string()),
            to: Some("1970-01-02".to_string()),
            grain: Some("day".to_string()),
            profile_id: None,
            device_id: None,
            source: None,
            model: None,
        });
        let custom_usage = load_usage(&db_path, custom)?;
        assert_eq!(custom_usage.usage_grain, "day");
        assert_eq!(custom_usage.filters.preset, "custom");
        assert_eq!(custom_usage.filters.from.as_deref(), Some("1970-01-01"));
        assert_eq!(custom_usage.filters.to.as_deref(), Some("1970-01-02"));
        assert_eq!(custom_usage.buckets.len(), 1);
        assert_eq!(custom_usage.buckets[0].bucket_key, "1970-01-01");

        let long_custom = UsageFilters::from_query(UsageQuery {
            preset: Some("custom".to_string()),
            range: None,
            from: Some("1970-01-01".to_string()),
            to: Some("1970-05-02".to_string()),
            grain: Some("auto".to_string()),
            profile_id: None,
            device_id: None,
            source: None,
            model: None,
        });
        assert_eq!(long_custom.usage_grain(), "month");

        let normalized_model = UsageFilters::from_query(UsageQuery {
            preset: Some("7d".to_string()),
            range: None,
            from: None,
            to: None,
            grain: Some("auto".to_string()),
            profile_id: None,
            device_id: None,
            source: None,
            model: Some("cline-pass/deepseek-v4-pro".to_string()),
        });
        assert_eq!(normalized_model.model.as_deref(), Some("deepseek-v4-pro"));

        let _ = fs::remove_file(db_path);
        Ok(())
    }

    #[test]
    fn today_usage_reads_raw_cross_midnight_calls_before_rollup_refresh() -> Result<()> {
        let db_path = temp_db_path("usage-cross-midnight");
        let db = Database::open(&db_path)?;
        db.migrate()?;
        let today_start_ns: i64 = db.connection().query_row(
            "SELECT CAST(strftime('%s', date('now', 'localtime', 'start of day'), 'utc') AS INTEGER) * 1000000000",
            [],
            |row| row.get(0),
        )?;

        let mut before_midnight = usage_test_event(
            "cross-midnight-before",
            today_start_ns - 1_000_000_000,
            100,
            10,
        );
        before_midnight.session = Some(EntityHint {
            external_id: "cross-midnight".to_string(),
            kind: "session".to_string(),
            title: None,
        });
        before_midnight.run.external_id = "cross-midnight".to_string();

        let mut after_midnight = usage_test_event(
            "cross-midnight-after",
            today_start_ns + 1_000_000_000,
            200,
            20,
        );
        after_midnight.session = before_midnight.session.clone();
        after_midnight.run.external_id = before_midnight.run.external_id.clone();

        let projector = Projector::new(&db);
        projector.project(&before_midnight)?;
        projector.project(&after_midnight)?;

        let usage = load_usage(
            &db_path,
            UsageFilters::from_query(UsageQuery {
                preset: Some("today".to_string()),
                range: None,
                from: None,
                to: None,
                grain: Some("auto".to_string()),
                profile_id: None,
                device_id: None,
                source: Some("codex".to_string()),
                model: None,
            }),
        )?;
        let bucket_tokens: i64 = usage
            .buckets
            .iter()
            .map(|bucket| bucket.input_tokens + bucket.output_tokens)
            .sum();

        assert_eq!(usage.summary.total_tokens, 220);
        assert_eq!(bucket_tokens, usage.summary.total_tokens);
        assert_eq!(usage.source_usage.len(), 1);
        assert_eq!(usage.source_usage[0].input_tokens, 200);
        assert_eq!(usage.model_summaries.len(), 1);
        assert_eq!(usage.model_summaries[0].input_tokens, 200);

        let _ = fs::remove_file(db_path);
        Ok(())
    }

    #[test]
    fn startup_rebuilds_rollups_without_current_semantics_version() -> Result<()> {
        let db_path = temp_db_path("rollup-version");
        let db = Database::open(&db_path)?;
        db.migrate()?;
        Projector::new(&db).project(&usage_test_event("rollup-version", 100, 100, 10))?;
        rollup::refresh_all(&db)?;
        db.connection()
            .execute("UPDATE usage_source_rollups SET input_tokens = 999", [])?;
        db.connection()
            .execute("DELETE FROM meta WHERE key = 'usage_rollup_version'", [])?;

        assert!(rollup::refresh_if_missing(&db)?.is_some());
        let input_tokens: i64 = db.connection().query_row(
            "SELECT input_tokens FROM usage_source_rollups WHERE bucket = 'day'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(input_tokens, 100);

        let _ = fs::remove_file(db_path);
        Ok(())
    }

    #[test]
    fn usage_cost_fallback_prices_uncached_input_and_cache_once() -> Result<()> {
        let db_path = temp_db_path("usage-cost-fallback");
        let db = Database::open(&db_path)?;
        db.migrate()?;
        db.connection().execute(
            "INSERT INTO pricing_sources (
                pricing_source_id, source_url, fetched_at_ns, model_count, raw_json, metadata_json
             )
             VALUES ('source_1', 'https://example.test/prices.json', 10, 1, '{}', NULL)",
            [],
        )?;
        db.connection().execute(
            "INSERT INTO model_prices (
                model_name, source_id, provider, input_cost_per_token, output_cost_per_token,
                cache_read_input_token_cost, cache_creation_input_token_cost, updated_at_ns, metadata_json
             )
             VALUES ('gpt-priced', 'source_1', 'openai', 0.001, 0.01, 0.0001, 0.0002, 10, NULL)",
            [],
        )?;

        let mut event = usage_test_event("priced", 100, 1000, 250);
        event.usage.model = Some("gpt-priced".to_string());
        event.usage.uncached_input_tokens = 600;
        event.usage.cache_read_tokens = 400;
        event.usage.cache_write_tokens = 50;
        Projector::new(&db).project(&event)?;
        rollup::refresh_all(&db)?;

        let usage = load_usage(&db_path, UsageFilters::all())?;
        let expected_cost = 3.15;
        assert!((usage.summary.total_cost_usd - expected_cost).abs() < 0.000001);
        assert!((usage.buckets[0].total_cost_usd - expected_cost).abs() < 0.000001);

        let source_rollup_cost: f64 = db.connection().query_row(
            "SELECT total_cost_usd FROM usage_source_rollups WHERE bucket = 'day'",
            [],
            |row| row.get(0),
        )?;
        assert!((source_rollup_cost - expected_cost).abs() < 0.000001);

        let _ = fs::remove_file(db_path);
        Ok(())
    }

    #[test]
    fn usage_month_sampling_keeps_range_totals_exact() -> Result<()> {
        let db_path = temp_db_path("usage-month-range");
        let db = Database::open(&db_path)?;
        db.migrate()?;

        let projector = Projector::new(&db);
        projector.project(&usage_test_event("jan", epoch_day_noon_ns(14), 100, 10))?;
        projector.project(&usage_test_event("feb", epoch_day_noon_ns(45), 200, 20))?;
        rollup::refresh_all(&db)?;

        let day_usage = load_usage(
            &db_path,
            UsageFilters::from_query(UsageQuery {
                preset: Some("custom".to_string()),
                range: None,
                from: Some("1970-01-10".to_string()),
                to: Some("1970-02-10".to_string()),
                grain: Some("day".to_string()),
                profile_id: None,
                device_id: None,
                source: None,
                model: None,
            }),
        )?;
        let month_usage = load_usage(
            &db_path,
            UsageFilters::from_query(UsageQuery {
                preset: Some("custom".to_string()),
                range: None,
                from: Some("1970-01-10".to_string()),
                to: Some("1970-02-10".to_string()),
                grain: Some("month".to_string()),
                profile_id: None,
                device_id: None,
                source: None,
                model: None,
            }),
        )?;

        assert_eq!(day_usage.summary.total_tokens, 110);
        assert_eq!(
            month_usage.summary.total_tokens,
            day_usage.summary.total_tokens
        );
        assert_eq!(month_usage.buckets.len(), 1);
        assert_eq!(month_usage.buckets[0].bucket_key, "1970-01");
        assert_eq!(month_usage.buckets[0].input_tokens, 100);
        assert_eq!(month_usage.buckets[0].output_tokens, 10);

        let _ = fs::remove_file(db_path);
        Ok(())
    }

    #[test]
    fn usage_all_keeps_lifetime_scope_beyond_30d() -> Result<()> {
        let db_path = temp_db_path("usage-all-lifetime");
        let db = Database::open(&db_path)?;
        db.migrate()?;

        let now = crate::db::now_ns();
        let day_ns = 86_400 * 1_000_000_000;
        let projector = Projector::new(&db);
        projector.project(&usage_test_event("today", now, 100, 10))?;
        projector.project(&usage_test_event("ten-days", now - 10 * day_ns, 200, 20))?;
        projector.project(&usage_test_event("forty-days", now - 40 * day_ns, 300, 30))?;
        rollup::refresh_all(&db)?;

        let seven_days = load_usage(
            &db_path,
            UsageFilters::from_query(UsageQuery {
                preset: Some("7d".to_string()),
                range: None,
                from: None,
                to: None,
                grain: Some("auto".to_string()),
                profile_id: None,
                device_id: None,
                source: None,
                model: None,
            }),
        )?;
        let thirty_days = load_usage(
            &db_path,
            UsageFilters::from_query(UsageQuery {
                preset: Some("30d".to_string()),
                range: None,
                from: None,
                to: None,
                grain: Some("auto".to_string()),
                profile_id: None,
                device_id: None,
                source: None,
                model: None,
            }),
        )?;
        let all = load_usage(&db_path, UsageFilters::all())?;

        assert_eq!(seven_days.summary.input_tokens, 100);
        assert_eq!(thirty_days.summary.input_tokens, 300);
        assert_eq!(all.summary.input_tokens, 600);
        assert_eq!(seven_days.buckets.len(), 1);
        assert_eq!(thirty_days.buckets.len(), 2);
        assert!(
            all.buckets.len() >= 2,
            "all keeps the older month instead of collapsing to 30d"
        );

        let _ = fs::remove_file(db_path);
        Ok(())
    }

    #[test]
    fn usage_filters_profile_scoped_imports_without_id_collisions() -> Result<()> {
        let db_path = temp_db_path("usage-profile-filter");
        let db = Database::open(&db_path)?;
        db.migrate()?;

        let mut profile_a = usage_test_event("shared", epoch_day_noon_ns(20), 100, 10);
        profile_a.profile_id = "profile_a".to_string();
        profile_a.device_id = "device_1".to_string();
        let mut profile_b = usage_test_event("shared", epoch_day_noon_ns(20), 200, 20);
        profile_b.profile_id = "profile_b".to_string();
        profile_b.device_id = "device_1".to_string();

        let projector = Projector::new(&db);
        let result_a = projector.project(&profile_a)?;
        let result_b = projector.project(&profile_b)?;
        rollup::refresh_all(&db)?;

        assert_ne!(result_a.run_id, result_b.run_id);

        let all_usage = load_usage(&db_path, UsageFilters::all())?;
        assert_eq!(all_usage.summary.sessions, 2);
        assert_eq!(all_usage.summary.runs, 2);
        assert_eq!(all_usage.summary.input_tokens, 300);

        let profile_usage = load_usage(
            &db_path,
            UsageFilters {
                profile_id: Some("profile_a".to_string()),
                ..UsageFilters::all()
            },
        )?;
        assert_eq!(profile_usage.summary.sessions, 1);
        assert_eq!(profile_usage.summary.runs, 1);
        assert_eq!(profile_usage.summary.input_tokens, 100);
        assert!(
            all_usage
                .profile_options
                .iter()
                .any(|profile| profile.profile_id == "profile_a")
        );

        let _ = fs::remove_file(db_path);
        Ok(())
    }

    #[test]
    fn menubar_today_trend_respects_profile_filter() -> Result<()> {
        let db_path = temp_db_path("menubar-profile-trend-filter");
        let db = Database::open(&db_path)?;
        db.migrate()?;

        let timestamp_ns = crate::db::now_ns();
        let mut profile_a = usage_test_event("today-a", timestamp_ns, 77, 11);
        profile_a.profile_id = "profile_a".to_string();
        profile_a.device_id = "device_1".to_string();
        let mut profile_b = usage_test_event("today-b", timestamp_ns + 1, 33, 7);
        profile_b.profile_id = "profile_b".to_string();
        profile_b.device_id = "device_1".to_string();

        let projector = Projector::new(&db);
        projector.project(&profile_a)?;
        projector.project(&profile_b)?;

        let menubar = load_menubar(
            &db_path,
            MenubarQuery {
                range: None,
                profile_id: Some("profile_b".to_string()),
            },
            SyncStatus::default(),
            &test_identity("profile_a"),
        )?;

        let trend_tokens = menubar
            .trend
            .iter()
            .map(|bucket| bucket.input_tokens)
            .sum::<i64>();
        assert_eq!(menubar.current_profile_id, "profile_b");
        assert_eq!(menubar.summary.input_tokens, 33);
        assert_eq!(trend_tokens, 33);

        let _ = fs::remove_file(db_path);
        Ok(())
    }

    #[test]
    fn menubar_defaults_to_current_profile() -> Result<()> {
        let db_path = temp_db_path("menubar-default-current-profile");
        let db = Database::open(&db_path)?;
        db.migrate()?;

        let timestamp_ns = crate::db::now_ns();
        let mut profile_a = usage_test_event("default-all-a", timestamp_ns, 70, 7);
        profile_a.profile_id = "profile_a".to_string();
        profile_a.device_id = "device_1".to_string();
        let mut profile_b = usage_test_event("default-all-b", timestamp_ns + 1, 30, 3);
        profile_b.profile_id = "profile_b".to_string();
        profile_b.device_id = "device_1".to_string();

        let projector = Projector::new(&db);
        projector.project(&profile_a)?;
        projector.project(&profile_b)?;
        rollup::refresh_all(&db)?;

        let menubar = load_menubar(
            &db_path,
            MenubarQuery {
                range: None,
                profile_id: None,
            },
            SyncStatus::default(),
            &test_identity("profile_a"),
        )?;

        let trend_tokens = menubar
            .trend
            .iter()
            .map(|bucket| bucket.input_tokens)
            .sum::<i64>();
        assert_eq!(menubar.current_profile_id, "profile_a");
        assert_eq!(menubar.summary.input_tokens, 70);
        assert_eq!(trend_tokens, 70);

        let _ = fs::remove_file(db_path);
        Ok(())
    }

    #[test]
    fn workspace_status_reports_shared_workspace_after_repair() -> Result<()> {
        let root =
            std::env::temp_dir().join(format!("shirabe-server-workspace-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let workspace_dir = root.join("workspace");
        let workspace_report = workspace::prepare(Some(workspace_dir.clone()))?;
        let db = Database::open(&workspace_report.catalog_db)?;
        db.migrate()?;

        let identity = test_identity("profile_shared");
        let state = AppState {
            db_path: workspace_report.catalog_db.clone(),
            ui_dir: root.join("ui"),
            source_paths: test_source_paths(&root),
            source_settings: crate::config::SourceSettings::from_values(Vec::new(), None),
            identity: identity.clone(),
            sync: Mutex::new(SyncStatus::default()),
        };

        let repaired = repair_workspace_inner(&state)?;
        assert_eq!(repaired.status, "ok");
        assert_eq!(repaired.mode, "shared");
        assert_eq!(repaired.workspace_dir, workspace_dir.display().to_string());
        assert_eq!(repaired.current_profile_id, "profile_shared");
        assert_eq!(repaired.current_profile_label, "profile_shared");
        assert_eq!(repaired.profile_count, 1);
        assert!(repaired.writable);
        assert!(!repaired.restart_required);
        assert!(repaired.issue.is_none());

        let status = load_workspace_status(&workspace_report.catalog_db, &identity, false)?;
        assert_eq!(status.mode, "shared");
        assert_eq!(status.profile_count, 1);
        assert_eq!(status.current_device_id, "device_1");

        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn sessions_api_reads_list_and_detail() -> Result<()> {
        let db_path = temp_db_path("sessions-api");
        let db = Database::open(&db_path)?;
        db.migrate()?;

        let mut event = usage_test_event("detail", epoch_day_noon_ns(20), 100, 20);
        event.skill_events = vec![SkillEventHint {
            skill_name: "cmux".to_string(),
            event_type: SkillEventType::Loaded,
            confidence: 0.75,
            metadata: None,
        }];
        Projector::new(&db).project(&event)?;

        let sessions = load_sessions(&db_path)?;
        assert!(sessions.sessions.iter().any(|session| {
            session.session_id == "codex:session-detail" && session.input_tokens == 100
        }));

        let detail =
            load_session_detail(&db_path, "codex:session-detail")?.expect("session detail exists");
        assert_eq!(detail.session.session_id, "codex:session-detail");
        assert_eq!(detail.runs.len(), 1);
        assert_eq!(detail.llm_calls.len(), 1);
        assert_eq!(detail.skill_events.len(), 1);
        assert!(
            detail
                .timeline
                .iter()
                .any(|event| event.event_type == "llm")
        );
        assert!(
            detail
                .timeline
                .iter()
                .any(|event| event.event_type == "skill")
        );
        assert_eq!(detail.model_summaries[0].model, "gpt-test");
        assert!(load_session_detail(&db_path, "missing")?.is_none());

        let _ = fs::remove_file(db_path);
        Ok(())
    }

    #[test]
    fn session_timeline_keeps_latest_events_when_limited() -> Result<()> {
        let db_path = temp_db_path("session-timeline-limit");
        let db = Database::open(&db_path)?;
        db.migrate()?;

        for index in 0..305 {
            let mut event = usage_test_event(
                &format!("timeline-{index}"),
                epoch_day_noon_ns(20) + index * 1_000_000_000,
                10,
                1,
            );
            event.session = Some(EntityHint {
                external_id: "timeline-limit".to_string(),
                kind: "session".to_string(),
                title: None,
            });
            Projector::new(&db).project(&event)?;
        }

        let detail =
            load_session_detail(&db_path, "codex:timeline-limit")?.expect("session detail exists");

        assert_eq!(detail.timeline.len(), 300);
        assert!(
            !detail
                .timeline
                .iter()
                .any(|event| event.run_id == "codex:run-timeline-0")
        );
        assert!(
            detail
                .timeline
                .iter()
                .any(|event| event.run_id == "codex:run-timeline-304")
        );

        let _ = fs::remove_file(db_path);
        Ok(())
    }

    #[test]
    fn usage_returns_tool_failure_hotspots_by_source() -> Result<()> {
        let db_path = temp_db_path("usage-tool-failures");
        let db = Database::open(&db_path)?;
        db.migrate()?;

        let event = NormalizedEvent {
            profile_id: "local".to_string(),
            device_id: "local_device".to_string(),
            source: "pi".to_string(),
            source_kind: "session_jsonl_event".to_string(),
            source_event_id: Some("pi-tool-1".to_string()),
            observed_at_ns: 100,
            occurred_at_ns: 100,
            source_ref: Some("pi.jsonl:1".to_string()),
            cwd: None,
            project_id: None,
            trace: TraceContext::default(),
            session: Some(EntityHint {
                external_id: "session-1".to_string(),
                kind: "session".to_string(),
                title: None,
            }),
            run: EntityHint {
                external_id: "run-1".to_string(),
                kind: "session".to_string(),
                title: None,
            },
            turn: None,
            operation: Operation {
                operation_type: OperationType::ToolCall,
                name: "edit".to_string(),
                status: OperationStatus::Failed,
                error_type: Some("apply_failed".to_string()),
                started_at_ns: 100,
                ended_at_ns: Some(200),
                duration_ns: Some(100),
                order_index: Some(0),
                metadata: None,
            },
            usage: Usage::default(),
            skill_events: Vec::new(),
            confidence: 1.0,
        };

        let projector = Projector::new(&db);
        projector.project(&event)?;
        projector.project(&NormalizedEvent {
            source: "codex".to_string(),
            source_event_id: Some("codex-tool-1".to_string()),
            operation: Operation {
                status: OperationStatus::Success,
                ..event.operation.clone()
            },
            ..event
        })?;
        rollup::refresh_all(&db)?;

        let usage = load_usage(&db_path, UsageFilters::all())?;
        let hotspot = usage
            .tool_failures
            .iter()
            .find(|tool| tool.source == "pi" && tool.tool_name == "edit")
            .expect("pi edit failure is returned");

        assert_eq!(hotspot.calls, 1);
        assert_eq!(hotspot.failed_calls, 1);
        assert_eq!(hotspot.success_calls, 0);
        assert_eq!(hotspot.failure_rate, 1.0);

        let _ = fs::remove_file(db_path);
        Ok(())
    }

    #[test]
    fn today_latency_panel_leaves_learning_after_enough_calls_without_hour_coverage() -> Result<()>
    {
        let db_path = temp_db_path("today-latency-learning");
        let db = Database::open(&db_path)?;
        db.migrate()?;
        let samples = latency_samples_in_hour(20, 9);

        let today_panel = load_today_latency_panel(db.connection(), samples.clone())?;
        let pattern_panel = load_pattern_latency_panel(samples);

        assert_eq!(today_panel.status, "normal");
        assert_eq!(today_panel.baseline.good_calls, 20);
        assert_eq!(today_panel.baseline.active_good_hours, 1);
        assert_eq!(pattern_panel.status, "learning");

        let _ = fs::remove_file(db_path);
        Ok(())
    }

    fn temp_db_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("shirabe-{name}-{}.sqlite", std::process::id()))
    }

    fn epoch_day_noon_ns(day: i64) -> i64 {
        (day * 86_400 + 12 * 3_600) * 1_000_000_000
    }

    fn usage_test_event(
        id: &str,
        timestamp_ns: i64,
        input_tokens: i64,
        output_tokens: i64,
    ) -> NormalizedEvent {
        NormalizedEvent {
            profile_id: "local".to_string(),
            device_id: "local_device".to_string(),
            source: "codex".to_string(),
            source_kind: "session_jsonl_event".to_string(),
            source_event_id: Some(format!("event-{id}")),
            observed_at_ns: timestamp_ns,
            occurred_at_ns: timestamp_ns,
            source_ref: Some(format!("{id}.jsonl:1")),
            cwd: None,
            project_id: None,
            trace: TraceContext::default(),
            session: Some(EntityHint {
                external_id: format!("session-{id}"),
                kind: "session".to_string(),
                title: None,
            }),
            run: EntityHint {
                external_id: format!("run-{id}"),
                kind: "session".to_string(),
                title: None,
            },
            turn: None,
            operation: Operation {
                operation_type: OperationType::LlmCall,
                name: "assistant".to_string(),
                status: OperationStatus::Success,
                error_type: None,
                started_at_ns: timestamp_ns,
                ended_at_ns: Some(timestamp_ns),
                duration_ns: Some(0),
                order_index: Some(0),
                metadata: None,
            },
            usage: Usage {
                model: Some("gpt-test".to_string()),
                input_tokens,
                output_tokens,
                ..Usage::default()
            },
            skill_events: Vec::new(),
            confidence: 1.0,
        }
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

    fn test_source_paths(root: &std::path::Path) -> SourcePaths {
        SourcePaths {
            codex: root.join("codex"),
            pi: root.join("pi"),
            claude: root.join("claude"),
            kanade: root.join("kanade"),
            amp: vec![root.join("amp")],
        }
    }

    fn latency_samples_in_hour(count: usize, hour: usize) -> Vec<LatencySample> {
        (0..count)
            .map(|index| LatencySample {
                provider: "openai".to_string(),
                hour,
                delay_ns: (index as i64 + 1) * 1_000_000_000,
            })
            .collect()
    }
}
#[test]
fn enabled_sync_sources_include_amp_by_default_and_skip_disabled_amp() {
    let default = crate::config::SourceSettings::from_values(Vec::new(), None);
    assert_eq!(
        enabled_sync_sources(&default),
        vec!["codex", "pi", "claude", "kanade", "amp"]
    );
    let disabled =
        crate::config::SourceSettings::from_values(vec!["amp".into(), "pi".into()], None);
    assert_eq!(
        enabled_sync_sources(&disabled),
        vec!["codex", "claude", "kanade"]
    );
}
