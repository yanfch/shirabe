use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

pub const SCHEMA_VERSION: i64 = 4;

pub struct Database {
    path: PathBuf,
    conn: Connection,
}

pub struct PreparedImportFile {
    pub file_id: String,
    pub should_import: bool,
}

pub struct BatchTransaction<'a> {
    conn: &'a Connection,
    finished: bool,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }

        let conn =
            Connection::open(path).with_context(|| format!("open sqlite {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        conn.pragma_update(None, "cache_size", -200_000i64)?;
        conn.pragma_update(None, "mmap_size", 268_435_456i64)?;

        Ok(Self {
            path: path.to_path_buf(),
            conn,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn connection(&self) -> &Connection {
        &self.conn
    }

    pub fn begin_batch(&self) -> Result<BatchTransaction<'_>> {
        self.conn
            .execute_batch("BEGIN IMMEDIATE")
            .context("begin sqlite import transaction")?;
        Ok(BatchTransaction {
            conn: &self.conn,
            finished: false,
        })
    }

    pub fn migrate(&self) -> Result<()> {
        self.conn
            .execute_batch(SCHEMA_SQL)
            .context("apply schema")?;
        self.ensure_column(
            "usage_source_rollups",
            "total_cost_usd",
            "REAL NOT NULL DEFAULT 0",
        )?;
        self.ensure_column(
            "usage_model_rollups",
            "total_cost_usd",
            "REAL NOT NULL DEFAULT 0",
        )?;
        for table in ["runs", "turns", "llm_calls", "tool_calls"] {
            self.ensure_column(table, "started_day", "TEXT")?;
            self.ensure_column(table, "started_month", "TEXT")?;
        }
        self.backfill_time_buckets()?;

        self.conn.execute(
            "INSERT INTO meta (key, value, updated_at_ns)
             VALUES ('schema_version', ?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at_ns = excluded.updated_at_ns",
            params![SCHEMA_VERSION.to_string(), now_ns()],
        )?;

        Ok(())
    }

    fn ensure_column(&self, table: &str, column: &str, definition: &str) -> Result<()> {
        let count: i64 = self.conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM pragma_table_info({}) WHERE name = ?1",
                sql_string(table)
            ),
            params![column],
            |row| row.get(0),
        )?;
        if count == 0 {
            self.conn.execute(
                &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
                [],
            )?;
        }
        Ok(())
    }

    fn backfill_time_buckets(&self) -> Result<()> {
        self.conn
            .execute_batch(
                "UPDATE runs
                 SET started_day = date(started_at_ns / 1000000000, 'unixepoch', 'localtime'),
                     started_month = strftime('%Y-%m', started_at_ns / 1000000000, 'unixepoch', 'localtime')
                 WHERE started_day IS NULL OR started_month IS NULL;

                 UPDATE turns
                 SET started_day = date(started_at_ns / 1000000000, 'unixepoch', 'localtime'),
                     started_month = strftime('%Y-%m', started_at_ns / 1000000000, 'unixepoch', 'localtime')
                 WHERE started_day IS NULL OR started_month IS NULL;

                 UPDATE llm_calls
                 SET started_day = date(started_at_ns / 1000000000, 'unixepoch', 'localtime'),
                     started_month = strftime('%Y-%m', started_at_ns / 1000000000, 'unixepoch', 'localtime')
                 WHERE started_day IS NULL OR started_month IS NULL;

                 UPDATE tool_calls
                 SET started_day = date(started_at_ns / 1000000000, 'unixepoch', 'localtime'),
                     started_month = strftime('%Y-%m', started_at_ns / 1000000000, 'unixepoch', 'localtime')
                 WHERE started_day IS NULL OR started_month IS NULL;",
            )
            .context("backfill operation time buckets")?;
        Ok(())
    }

    pub fn record_import_source(
        &self,
        source: &str,
        source_kind: &str,
        root_path: &Path,
    ) -> Result<String> {
        let root_path = root_path.to_string_lossy().to_string();
        let root_path_hash = stable_hash(&root_path);
        let source_id = format!("{source}:{root_path_hash}");
        let now = now_ns();

        self.conn.execute(
            "INSERT INTO import_sources (
                source_id, source, source_kind, root_path, root_path_hash,
                enabled, last_scan_ns, parser_version
             )
             VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7)
             ON CONFLICT(source_id) DO UPDATE SET
                source_kind = excluded.source_kind,
                root_path = excluded.root_path,
                root_path_hash = excluded.root_path_hash,
                last_scan_ns = excluded.last_scan_ns,
                parser_version = excluded.parser_version",
            params![
                source_id,
                source,
                source_kind,
                root_path,
                root_path_hash,
                now,
                env!("CARGO_PKG_VERSION")
            ],
        )?;

        Ok(source_id)
    }

    pub fn record_import_file(
        &self,
        source_id: &str,
        path: &Path,
        size_bytes: u64,
        modified_ns: i64,
        status: &str,
    ) -> Result<String> {
        let path = path.to_string_lossy().to_string();
        let path_hash = stable_hash(&path);
        let file_id = format!("{source_id}:file:{path_hash}");
        let fingerprint = stable_hash(&format!("{path_hash}:{size_bytes}:{modified_ns}"));
        let now = now_ns();

        self.conn.execute(
            "INSERT INTO import_files (
                file_id, source_id, path, path_hash, size_bytes, modified_ns,
                fingerprint, parser_version, status, last_import_ns
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(file_id) DO UPDATE SET
                path = excluded.path,
                path_hash = excluded.path_hash,
                size_bytes = excluded.size_bytes,
                modified_ns = excluded.modified_ns,
                fingerprint = excluded.fingerprint,
                parser_version = excluded.parser_version,
                status = excluded.status,
                last_import_ns = excluded.last_import_ns",
            params![
                file_id,
                source_id,
                path,
                path_hash,
                size_bytes.min(i64::MAX as u64) as i64,
                modified_ns,
                fingerprint,
                env!("CARGO_PKG_VERSION"),
                status,
                now
            ],
        )?;

        Ok(file_id)
    }

    pub fn prepare_import_file(
        &self,
        source_id: &str,
        path: &Path,
        size_bytes: u64,
        modified_ns: i64,
    ) -> Result<PreparedImportFile> {
        let path = path.to_string_lossy().to_string();
        let path_hash = stable_hash(&path);
        let file_id = format!("{source_id}:file:{path_hash}");
        let fingerprint = stable_hash(&format!("{path_hash}:{size_bytes}:{modified_ns}"));
        let parser_version = env!("CARGO_PKG_VERSION");

        let existing = self
            .conn
            .query_row(
                "SELECT fingerprint, parser_version, status
                 FROM import_files
                 WHERE file_id = ?1",
                params![file_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;

        if let Some((existing_fingerprint, existing_parser_version, status)) = existing {
            let parser_matches = existing_parser_version.as_deref() == Some(parser_version);
            let complete = matches!(status.as_str(), "imported" | "partial");
            if existing_fingerprint == fingerprint && parser_matches && complete {
                return Ok(PreparedImportFile {
                    file_id,
                    should_import: false,
                });
            }
        }

        self.conn.execute(
            "INSERT INTO import_files (
                file_id, source_id, path, path_hash, size_bytes, modified_ns,
                fingerprint, parser_version, status, last_import_ns
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending', ?9)
             ON CONFLICT(file_id) DO UPDATE SET
                path = excluded.path,
                path_hash = excluded.path_hash,
                size_bytes = excluded.size_bytes,
                modified_ns = excluded.modified_ns,
                fingerprint = excluded.fingerprint,
                parser_version = excluded.parser_version,
                status = excluded.status,
                last_import_ns = excluded.last_import_ns",
            params![
                file_id,
                source_id,
                path,
                path_hash,
                size_bytes.min(i64::MAX as u64) as i64,
                modified_ns,
                fingerprint,
                parser_version,
                now_ns()
            ],
        )?;

        Ok(PreparedImportFile {
            file_id,
            should_import: true,
        })
    }

    pub fn finish_import_file(
        &self,
        file_id: &str,
        status: &str,
        event_count: usize,
        warning_count: usize,
        last_offset: Option<u64>,
        first_event_ns: Option<i64>,
        last_event_ns: Option<i64>,
        error_message: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE import_files SET
                status = ?2,
                event_count = ?3,
                warning_count = ?4,
                last_offset = ?5,
                first_event_ns = ?6,
                last_event_ns = ?7,
                last_import_ns = ?8,
                error_message = ?9
             WHERE file_id = ?1",
            params![
                file_id,
                status,
                event_count.min(i64::MAX as usize) as i64,
                warning_count.min(i64::MAX as usize) as i64,
                last_offset.map(|offset| offset.min(i64::MAX as u64) as i64),
                first_event_ns,
                last_event_ns,
                now_ns(),
                error_message,
            ],
        )?;

        Ok(())
    }
}

impl BatchTransaction<'_> {
    pub fn commit(mut self) -> Result<()> {
        self.conn
            .execute_batch("COMMIT")
            .context("commit sqlite import transaction")?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for BatchTransaction<'_> {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.conn.execute_batch("ROLLBACK");
        }
    }
}

pub fn stable_hash(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

fn sql_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

pub fn now_ns() -> i64 {
    system_time_ns(SystemTime::now())
}

pub fn system_time_ns(time: SystemTime) -> i64 {
    let duration = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    duration.as_nanos().min(i64::MAX as u128) as i64
}

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT,
    updated_at_ns INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS import_sources (
    source_id TEXT PRIMARY KEY,
    source TEXT NOT NULL,
    source_kind TEXT NOT NULL,
    root_path TEXT NOT NULL,
    root_path_hash TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    last_scan_ns INTEGER,
    last_success_ns INTEGER,
    cursor_time_ns INTEGER,
    parser_version TEXT,
    metadata_json TEXT
);

CREATE TABLE IF NOT EXISTS import_files (
    file_id TEXT PRIMARY KEY,
    source_id TEXT NOT NULL,
    path TEXT NOT NULL,
    path_hash TEXT NOT NULL,
    size_bytes INTEGER NOT NULL,
    modified_ns INTEGER NOT NULL,
    fingerprint TEXT NOT NULL,
    parser_version TEXT,
    status TEXT NOT NULL,
    last_offset INTEGER,
    event_count INTEGER NOT NULL DEFAULT 0,
    warning_count INTEGER NOT NULL DEFAULT 0,
    first_event_ns INTEGER,
    last_event_ns INTEGER,
    last_import_ns INTEGER,
    error_message TEXT,
    metadata_json TEXT,
    FOREIGN KEY (source_id) REFERENCES import_sources(source_id)
);
CREATE INDEX IF NOT EXISTS idx_import_files_source ON import_files(source_id);
CREATE INDEX IF NOT EXISTS idx_import_files_status ON import_files(status);

CREATE TABLE IF NOT EXISTS pricing_sources (
    pricing_source_id TEXT PRIMARY KEY,
    source_url TEXT NOT NULL,
    fetched_at_ns INTEGER NOT NULL,
    model_count INTEGER NOT NULL DEFAULT 0,
    raw_json TEXT NOT NULL,
    metadata_json TEXT
);

CREATE TABLE IF NOT EXISTS model_prices (
    model_name TEXT PRIMARY KEY,
    source_id TEXT NOT NULL,
    provider TEXT,
    input_cost_per_token REAL,
    output_cost_per_token REAL,
    cache_read_input_token_cost REAL,
    cache_creation_input_token_cost REAL,
    updated_at_ns INTEGER NOT NULL,
    metadata_json TEXT,
    FOREIGN KEY (source_id) REFERENCES pricing_sources(pricing_source_id)
);
CREATE INDEX IF NOT EXISTS idx_model_prices_provider ON model_prices(provider);

CREATE TABLE IF NOT EXISTS sessions (
    session_id TEXT PRIMARY KEY,
    source TEXT NOT NULL,
    kind TEXT NOT NULL,
    title TEXT,
    external_id TEXT,
    project_id TEXT,
    cwd TEXT,
    first_seen_ns INTEGER NOT NULL,
    last_seen_ns INTEGER NOT NULL,
    run_count INTEGER NOT NULL DEFAULT 0,
    turn_count INTEGER NOT NULL DEFAULT 0,
    active_state TEXT,
    attention_score REAL NOT NULL DEFAULT 0,
    primary_signal TEXT,
    metadata_json TEXT
);
CREATE INDEX IF NOT EXISTS idx_sessions_source_seen ON sessions(source, last_seen_ns);
CREATE INDEX IF NOT EXISTS idx_sessions_last_seen ON sessions(last_seen_ns);

CREATE TABLE IF NOT EXISTS runs (
    run_id TEXT PRIMARY KEY,
    source TEXT NOT NULL,
    kind TEXT NOT NULL,
    title TEXT,
    status TEXT NOT NULL,
    trace_id TEXT,
    root_span_id TEXT,
    parent_run_id TEXT,
    session_id TEXT,
    workflow_id TEXT,
    agent_id TEXT,
    external_id TEXT,
    cwd TEXT,
    project_id TEXT,
    started_at_ns INTEGER NOT NULL,
    started_day TEXT,
    started_month TEXT,
    ended_at_ns INTEGER,
    duration_ns INTEGER,
    llm_call_count INTEGER NOT NULL DEFAULT 0,
    tool_call_count INTEGER NOT NULL DEFAULT 0,
    failed_tool_count INTEGER NOT NULL DEFAULT 0,
    error_count INTEGER NOT NULL DEFAULT 0,
    signal_count INTEGER NOT NULL DEFAULT 0,
    attention_score REAL NOT NULL DEFAULT 0,
    primary_signal TEXT,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    total_cost_usd REAL NOT NULL DEFAULT 0,
    estimated_wasted_tokens INTEGER NOT NULL DEFAULT 0,
    cache_ratio REAL,
    max_context_window_percent REAL,
    metadata_json TEXT,
    FOREIGN KEY (session_id) REFERENCES sessions(session_id)
);
CREATE INDEX IF NOT EXISTS idx_runs_started ON runs(started_at_ns);
CREATE INDEX IF NOT EXISTS idx_runs_source_started ON runs(source, started_at_ns);
CREATE INDEX IF NOT EXISTS idx_runs_source_day ON runs(source, started_day);
CREATE INDEX IF NOT EXISTS idx_runs_source_month ON runs(source, started_month);
CREATE INDEX IF NOT EXISTS idx_runs_session ON runs(session_id);
CREATE INDEX IF NOT EXISTS idx_runs_session_started ON runs(session_id, started_at_ns);

CREATE TABLE IF NOT EXISTS turns (
    turn_id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL,
    session_id TEXT,
    source TEXT NOT NULL,
    turn_index INTEGER,
    role TEXT,
    input_hash TEXT,
    output_hash TEXT,
    status TEXT,
    started_at_ns INTEGER NOT NULL,
    started_day TEXT,
    started_month TEXT,
    ended_at_ns INTEGER,
    duration_ns INTEGER,
    llm_call_count INTEGER NOT NULL DEFAULT 0,
    tool_call_count INTEGER NOT NULL DEFAULT 0,
    failed_tool_count INTEGER NOT NULL DEFAULT 0,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    estimated_wasted_tokens INTEGER NOT NULL DEFAULT 0,
    trace_id TEXT,
    span_id TEXT,
    metadata_json TEXT,
    FOREIGN KEY (run_id) REFERENCES runs(run_id),
    FOREIGN KEY (session_id) REFERENCES sessions(session_id)
);
CREATE INDEX IF NOT EXISTS idx_turns_run ON turns(run_id);
CREATE INDEX IF NOT EXISTS idx_turns_session ON turns(session_id);
CREATE INDEX IF NOT EXISTS idx_turns_session_started ON turns(session_id, started_at_ns);
CREATE INDEX IF NOT EXISTS idx_turns_started ON turns(started_at_ns);
CREATE INDEX IF NOT EXISTS idx_turns_source_started ON turns(source, started_at_ns);
CREATE INDEX IF NOT EXISTS idx_turns_source_day ON turns(source, started_day);
CREATE INDEX IF NOT EXISTS idx_turns_source_month ON turns(source, started_month);

CREATE TABLE IF NOT EXISTS run_steps (
    step_id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL,
    session_id TEXT,
    turn_id TEXT,
    source TEXT NOT NULL,
    source_event_id TEXT,
    source_ref TEXT,
    parent_step_id TEXT,
    step_type TEXT NOT NULL,
    name TEXT NOT NULL,
    label TEXT,
    status TEXT NOT NULL,
    error_type TEXT,
    started_at_ns INTEGER NOT NULL,
    ended_at_ns INTEGER,
    duration_ns INTEGER,
    order_index INTEGER,
    llm_call_id TEXT,
    tool_call_id TEXT,
    trace_id TEXT,
    span_id TEXT,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    estimated_wasted_tokens INTEGER NOT NULL DEFAULT 0,
    cost_usd REAL NOT NULL DEFAULT 0,
    metadata_json TEXT,
    FOREIGN KEY (run_id) REFERENCES runs(run_id),
    FOREIGN KEY (session_id) REFERENCES sessions(session_id),
    FOREIGN KEY (turn_id) REFERENCES turns(turn_id)
);
CREATE INDEX IF NOT EXISTS idx_run_steps_run_order ON run_steps(run_id, order_index, started_at_ns);
CREATE INDEX IF NOT EXISTS idx_run_steps_run_status ON run_steps(run_id, status);

CREATE TABLE IF NOT EXISTS llm_calls (
    llm_call_id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL,
    session_id TEXT,
    turn_id TEXT,
    source TEXT NOT NULL,
    provider TEXT,
    model TEXT,
    operation TEXT,
    status TEXT NOT NULL,
    error_type TEXT,
    system_prompt_hash TEXT,
    prompt_ref TEXT,
    response_ref TEXT,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    reasoning_tokens INTEGER NOT NULL DEFAULT 0,
    uncached_input_tokens INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    cumulative_total_tokens INTEGER,
    cache_ratio REAL,
    reasoning_output_ratio REAL,
    model_context_window INTEGER,
    context_window_percent REAL,
    total_cost_usd REAL NOT NULL DEFAULT 0,
    pricing_status TEXT,
    cost_confidence TEXT,
    initiator TEXT,
    previous_llm_call_id TEXT,
    next_llm_call_id TEXT,
    started_at_ns INTEGER NOT NULL,
    started_day TEXT,
    started_month TEXT,
    ended_at_ns INTEGER,
    duration_ns INTEGER,
    trace_id TEXT,
    span_id TEXT,
    metadata_json TEXT,
    FOREIGN KEY (run_id) REFERENCES runs(run_id)
);
CREATE INDEX IF NOT EXISTS idx_llm_calls_run ON llm_calls(run_id);
CREATE INDEX IF NOT EXISTS idx_llm_calls_run_started ON llm_calls(run_id, started_at_ns);
CREATE INDEX IF NOT EXISTS idx_llm_calls_source_run ON llm_calls(source, run_id);
CREATE INDEX IF NOT EXISTS idx_llm_calls_turn ON llm_calls(turn_id);
CREATE INDEX IF NOT EXISTS idx_llm_calls_run_model ON llm_calls(run_id, model);
CREATE INDEX IF NOT EXISTS idx_llm_calls_session ON llm_calls(session_id);
CREATE INDEX IF NOT EXISTS idx_llm_calls_session_started ON llm_calls(session_id, started_at_ns);
CREATE INDEX IF NOT EXISTS idx_llm_calls_session_model ON llm_calls(session_id, model);
CREATE INDEX IF NOT EXISTS idx_llm_calls_started ON llm_calls(started_at_ns);
CREATE INDEX IF NOT EXISTS idx_llm_calls_source_started ON llm_calls(source, started_at_ns);
CREATE INDEX IF NOT EXISTS idx_llm_calls_source_day_model ON llm_calls(source, started_day, model);
CREATE INDEX IF NOT EXISTS idx_llm_calls_source_month_model ON llm_calls(source, started_month, model);
CREATE INDEX IF NOT EXISTS idx_llm_calls_model ON llm_calls(model, started_at_ns);

CREATE TABLE IF NOT EXISTS tool_calls (
    tool_call_id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL,
    session_id TEXT,
    turn_id TEXT,
    source TEXT NOT NULL,
    tool_name TEXT NOT NULL,
    status TEXT NOT NULL,
    error_type TEXT,
    input_ref TEXT,
    output_ref TEXT,
    output_bytes INTEGER,
    started_at_ns INTEGER NOT NULL,
    started_day TEXT,
    started_month TEXT,
    ended_at_ns INTEGER,
    duration_ns INTEGER,
    parent_llm_call_id TEXT,
    trace_id TEXT,
    span_id TEXT,
    estimated_wasted_tokens INTEGER NOT NULL DEFAULT 0,
    metadata_json TEXT,
    FOREIGN KEY (run_id) REFERENCES runs(run_id)
);
CREATE INDEX IF NOT EXISTS idx_tool_calls_run ON tool_calls(run_id);
CREATE INDEX IF NOT EXISTS idx_tool_calls_run_started ON tool_calls(run_id, started_at_ns);
CREATE INDEX IF NOT EXISTS idx_tool_calls_source_run ON tool_calls(source, run_id);
CREATE INDEX IF NOT EXISTS idx_tool_calls_turn ON tool_calls(turn_id);
CREATE INDEX IF NOT EXISTS idx_tool_calls_turn_status ON tool_calls(turn_id, status);
CREATE INDEX IF NOT EXISTS idx_tool_calls_session ON tool_calls(session_id);
CREATE INDEX IF NOT EXISTS idx_tool_calls_session_started ON tool_calls(session_id, started_at_ns);
CREATE INDEX IF NOT EXISTS idx_tool_calls_started ON tool_calls(started_at_ns);
CREATE INDEX IF NOT EXISTS idx_tool_calls_source_started ON tool_calls(source, started_at_ns);
CREATE INDEX IF NOT EXISTS idx_tool_calls_source_day_name ON tool_calls(source, started_day, tool_name);
CREATE INDEX IF NOT EXISTS idx_tool_calls_source_month_name ON tool_calls(source, started_month, tool_name);
CREATE INDEX IF NOT EXISTS idx_tool_calls_name_status ON tool_calls(tool_name, status);

CREATE TABLE IF NOT EXISTS run_signals (
    signal_id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL,
    source TEXT NOT NULL,
    signal_type TEXT NOT NULL,
    severity TEXT NOT NULL,
    title TEXT NOT NULL,
    evidence_json TEXT,
    suggestion TEXT,
    created_at_ns INTEGER NOT NULL,
    FOREIGN KEY (run_id) REFERENCES runs(run_id)
);
CREATE INDEX IF NOT EXISTS idx_run_signals_run ON run_signals(run_id);
CREATE INDEX IF NOT EXISTS idx_run_signals_type ON run_signals(signal_type, severity);

CREATE TABLE IF NOT EXISTS skill_events (
    skill_event_id TEXT PRIMARY KEY,
    source TEXT NOT NULL,
    skill_name TEXT NOT NULL,
    event_type TEXT NOT NULL,
    confidence REAL NOT NULL DEFAULT 0,
    session_id TEXT,
    run_id TEXT NOT NULL,
    turn_id TEXT,
    source_event_id TEXT,
    source_ref TEXT,
    occurred_at_ns INTEGER NOT NULL,
    occurred_day TEXT,
    occurred_month TEXT,
    metadata_json TEXT,
    FOREIGN KEY (session_id) REFERENCES sessions(session_id),
    FOREIGN KEY (run_id) REFERENCES runs(run_id),
    FOREIGN KEY (turn_id) REFERENCES turns(turn_id)
);
CREATE INDEX IF NOT EXISTS idx_skill_events_source_time
ON skill_events(source, occurred_at_ns);
CREATE INDEX IF NOT EXISTS idx_skill_events_source_day_name
ON skill_events(source, occurred_day, skill_name);
CREATE INDEX IF NOT EXISTS idx_skill_events_source_month_name
ON skill_events(source, occurred_month, skill_name);
CREATE INDEX IF NOT EXISTS idx_skill_events_run
ON skill_events(run_id);
CREATE INDEX IF NOT EXISTS idx_skill_events_session
ON skill_events(session_id);
CREATE INDEX IF NOT EXISTS idx_skill_events_session_time
ON skill_events(session_id, occurred_at_ns);
CREATE INDEX IF NOT EXISTS idx_skill_events_name_type
ON skill_events(skill_name, event_type);

CREATE TABLE IF NOT EXISTS metric_rollups (
    bucket_start_ns INTEGER NOT NULL,
    bucket_size_s INTEGER NOT NULL,
    cube TEXT NOT NULL,
    dimensions_json TEXT NOT NULL,
    metrics_json TEXT NOT NULL,
    updated_at_ns INTEGER NOT NULL,
    PRIMARY KEY (bucket_start_ns, bucket_size_s, cube, dimensions_json)
);

CREATE TABLE IF NOT EXISTS usage_source_rollups (
    bucket TEXT NOT NULL,
    bucket_key TEXT NOT NULL,
    source TEXT NOT NULL,
    sessions INTEGER NOT NULL DEFAULT 0,
    runs INTEGER NOT NULL DEFAULT 0,
    turns INTEGER NOT NULL DEFAULT 0,
    llm_calls INTEGER NOT NULL DEFAULT 0,
    tool_calls INTEGER NOT NULL DEFAULT 0,
    failed_tool_calls INTEGER NOT NULL DEFAULT 0,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    total_cost_usd REAL NOT NULL DEFAULT 0,
    updated_at_ns INTEGER NOT NULL,
    PRIMARY KEY (bucket, bucket_key, source)
);
CREATE INDEX IF NOT EXISTS idx_usage_source_rollups_lookup
ON usage_source_rollups(bucket, bucket_key, source);

CREATE TABLE IF NOT EXISTS usage_model_rollups (
    bucket TEXT NOT NULL,
    bucket_key TEXT NOT NULL,
    source TEXT NOT NULL,
    model TEXT NOT NULL,
    sessions INTEGER NOT NULL DEFAULT 0,
    runs INTEGER NOT NULL DEFAULT 0,
    turns INTEGER NOT NULL DEFAULT 0,
    llm_calls INTEGER NOT NULL DEFAULT 0,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    total_cost_usd REAL NOT NULL DEFAULT 0,
    updated_at_ns INTEGER NOT NULL,
    PRIMARY KEY (bucket, bucket_key, source, model)
);
CREATE INDEX IF NOT EXISTS idx_usage_model_rollups_lookup
ON usage_model_rollups(bucket, bucket_key, source, model);

CREATE TABLE IF NOT EXISTS usage_tool_rollups (
    bucket TEXT NOT NULL,
    bucket_key TEXT NOT NULL,
    source TEXT NOT NULL,
    tool_name TEXT NOT NULL,
    calls INTEGER NOT NULL DEFAULT 0,
    success_calls INTEGER NOT NULL DEFAULT 0,
    failed_calls INTEGER NOT NULL DEFAULT 0,
    updated_at_ns INTEGER NOT NULL,
    PRIMARY KEY (bucket, bucket_key, source, tool_name)
);
CREATE INDEX IF NOT EXISTS idx_usage_tool_rollups_lookup
ON usage_tool_rollups(bucket, bucket_key, source, tool_name);

CREATE TABLE IF NOT EXISTS usage_tool_model_rollups (
    bucket TEXT NOT NULL,
    bucket_key TEXT NOT NULL,
    source TEXT NOT NULL,
    model TEXT NOT NULL,
    tool_name TEXT NOT NULL,
    calls INTEGER NOT NULL DEFAULT 0,
    success_calls INTEGER NOT NULL DEFAULT 0,
    failed_calls INTEGER NOT NULL DEFAULT 0,
    updated_at_ns INTEGER NOT NULL,
    PRIMARY KEY (bucket, bucket_key, source, model, tool_name)
);
CREATE INDEX IF NOT EXISTS idx_usage_tool_model_rollups_lookup
ON usage_tool_model_rollups(bucket, bucket_key, source, model, tool_name);
"#;
