use std::{
    collections::{HashMap, HashSet},
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::config::Identity;

pub const SCHEMA_VERSION: i64 = 6;
const IMPORT_PARSER_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+import-v2");

pub struct Database {
    path: PathBuf,
    conn: Connection,
}

pub struct PreparedImportFile {
    pub file_id: String,
    pub should_import: bool,
    pub can_resume: bool,
    pub previous_size_bytes: Option<u64>,
    pub last_offset: Option<u64>,
    pub event_count: usize,
    pub warning_count: usize,
    pub metadata_json: Option<String>,
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
        set_shared_sqlite_modes(path);

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
        self.recreate_rollups_if_legacy()?;
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
        self.ensure_column("llm_calls", "observed_response_delay_ns", "INTEGER")?;
        self.ensure_column("llm_calls", "observed_output_tps", "REAL")?;
        self.ensure_column("llm_calls", "observed_trigger_step_id", "TEXT")?;
        self.ensure_column("llm_calls", "observed_trigger_step_type", "TEXT")?;
        self.ensure_column("llm_calls", "observed_latency_quality", "TEXT")?;
        for table in [
            "import_sources",
            "import_files",
            "sessions",
            "runs",
            "turns",
            "run_steps",
            "llm_calls",
            "tool_calls",
            "run_signals",
            "skill_events",
        ] {
            self.ensure_column(table, "profile_id", "TEXT NOT NULL DEFAULT 'local'")?;
            self.ensure_column(table, "device_id", "TEXT NOT NULL DEFAULT 'local_device'")?;
        }
        self.backfill_time_buckets()?;
        self.backfill_observed_llm_latency()?;

        self.conn.execute(
            "INSERT INTO meta (key, value, updated_at_ns)
             VALUES ('schema_version', ?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at_ns = excluded.updated_at_ns",
            params![SCHEMA_VERSION.to_string(), now_ns()],
        )?;

        Ok(())
    }

    pub fn register_identity(&self, identity: &Identity) -> Result<()> {
        let now = now_ns();
        self.conn.execute(
            "INSERT INTO devices (
                device_id, device_label, hostname, created_at_ns, last_seen_ns
             )
             VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT(device_id) DO UPDATE SET
                device_label = excluded.device_label,
                hostname = excluded.hostname,
                last_seen_ns = excluded.last_seen_ns",
            params![
                identity.device_id,
                identity.device_label,
                env_hostname(),
                now
            ],
        )?;
        self.conn.execute(
            "INSERT INTO profiles (
                profile_id, device_id, profile_label, macos_uid, macos_username,
                created_at_ns, last_seen_ns
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
             ON CONFLICT(profile_id) DO UPDATE SET
                device_id = excluded.device_id,
                profile_label = excluded.profile_label,
                macos_uid = excluded.macos_uid,
                macos_username = excluded.macos_username,
                last_seen_ns = excluded.last_seen_ns",
            params![
                identity.profile_id,
                identity.device_id,
                identity.profile_label,
                identity.macos_uid,
                identity.macos_username,
                now
            ],
        )?;

        Ok(())
    }

    pub fn refresh_observed_llm_latency_for_runs<'a, I>(&self, run_ids: I) -> Result<()>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let run_ids = run_ids
            .into_iter()
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        let started_transaction = self.conn.is_autocommit();
        if started_transaction {
            self.conn
                .execute_batch("BEGIN IMMEDIATE")
                .context("begin observed latency refresh transaction")?;
        }

        let result = self.refresh_observed_llm_latency_for_run_ids(&run_ids);
        if started_transaction {
            match result {
                Ok(()) => self
                    .conn
                    .execute_batch("COMMIT")
                    .context("commit observed latency refresh transaction")?,
                Err(error) => {
                    let _ = self.conn.execute_batch("ROLLBACK");
                    return Err(error);
                }
            }
        }

        Ok(())
    }

    fn refresh_observed_llm_latency_for_run_ids(&self, run_ids: &[String]) -> Result<()> {
        const NS_PER_SECOND: i64 = 1_000_000_000;
        const OUTLIER_NS: i64 = 30 * 60 * NS_PER_SECOND;

        #[derive(Debug)]
        struct TriggerStep {
            step_id: String,
            step_type: String,
            trigger_at_ns: i64,
            order_index: i64,
        }

        #[derive(Debug)]
        struct LlmCall {
            llm_call_id: String,
            started_at_ns: i64,
            order_index: i64,
            output_tokens: i64,
        }

        let mut unique_run_ids = Vec::new();
        let mut seen = HashSet::new();
        for run_id in run_ids {
            if seen.insert(run_id.clone()) {
                unique_run_ids.push(run_id.clone());
            }
        }

        if unique_run_ids.is_empty() {
            return Ok(());
        }

        self.conn
            .execute_batch(
                "CREATE TEMP TABLE IF NOT EXISTS selected_observed_latency_runs (
                    run_id TEXT PRIMARY KEY
                 );
                 DELETE FROM selected_observed_latency_runs;",
            )
            .context("prepare observed latency run selection")?;
        {
            let mut insert_run = self
                .conn
                .prepare("INSERT INTO selected_observed_latency_runs(run_id) VALUES (?1)")?;
            for run_id in &unique_run_ids {
                insert_run.execute(params![run_id])?;
            }
        }

        let mut triggers_by_run = HashMap::<String, Vec<TriggerStep>>::new();
        {
            let mut trigger_stmt = self.conn.prepare(
                "SELECT s.run_id, s.step_id, s.step_type,
                    COALESCE(s.ended_at_ns, s.started_at_ns),
                    COALESCE(s.order_index, 0)
                 FROM run_steps s
                 INNER JOIN selected_observed_latency_runs r ON r.run_id = s.run_id
                 WHERE s.step_type IN ('user', 'tool', 'approval', 'system')
                 ORDER BY s.run_id, COALESCE(s.ended_at_ns, s.started_at_ns), COALESCE(s.order_index, 0)",
            )?;
            let trigger_rows = trigger_stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        TriggerStep {
                            step_id: row.get(1)?,
                            step_type: row.get(2)?,
                            trigger_at_ns: row.get(3)?,
                            order_index: row.get(4)?,
                        },
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for (run_id, trigger) in trigger_rows {
                triggers_by_run.entry(run_id).or_default().push(trigger);
            }
        }

        let llm_calls = {
            let mut llm_stmt = self.conn.prepare(
                "SELECT l.run_id, l.llm_call_id, l.started_at_ns,
                    COALESCE(rs.order_index, 0), l.output_tokens
                 FROM llm_calls l
                 INNER JOIN selected_observed_latency_runs r ON r.run_id = l.run_id
                 LEFT JOIN run_steps rs ON rs.llm_call_id = l.llm_call_id
                 ORDER BY l.run_id, l.started_at_ns, COALESCE(rs.order_index, 0)",
            )?;
            llm_stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        LlmCall {
                            llm_call_id: row.get(1)?,
                            started_at_ns: row.get(2)?,
                            order_index: row.get(3)?,
                            output_tokens: row.get(4)?,
                        },
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };

        let mut update_stmt = self.conn.prepare(
            "UPDATE llm_calls
             SET observed_response_delay_ns = ?2,
                 observed_output_tps = ?3,
                 observed_trigger_step_id = ?4,
                 observed_trigger_step_type = ?5,
                 observed_latency_quality = ?6
             WHERE llm_call_id = ?1",
        )?;

        for (run_id, llm_call) in llm_calls {
            let trigger = triggers_by_run.get(&run_id).and_then(|triggers| {
                let index = triggers
                    .partition_point(|trigger| {
                        trigger.trigger_at_ns < llm_call.started_at_ns
                            || (trigger.trigger_at_ns == llm_call.started_at_ns
                                && trigger.order_index < llm_call.order_index)
                    })
                    .checked_sub(1)?;
                triggers.get(index)
            });

            let Some(trigger) = trigger else {
                update_stmt.execute(params![
                    llm_call.llm_call_id,
                    Option::<i64>::None,
                    Option::<f64>::None,
                    Option::<String>::None,
                    Option::<String>::None,
                    "missing",
                ])?;
                continue;
            };

            let delay_ns = llm_call.started_at_ns.saturating_sub(trigger.trigger_at_ns);
            let quality = if delay_ns < NS_PER_SECOND {
                "subsecond"
            } else if delay_ns > OUTLIER_NS {
                "outlier"
            } else {
                "good"
            };
            let observed_output_tps = if quality == "good" && llm_call.output_tokens > 0 {
                Some(llm_call.output_tokens as f64 / (delay_ns as f64 / NS_PER_SECOND as f64))
            } else {
                None
            };

            update_stmt.execute(params![
                llm_call.llm_call_id,
                delay_ns,
                observed_output_tps,
                trigger.step_id,
                trigger.step_type,
                quality,
            ])?;
        }

        Ok(())
    }

    fn backfill_observed_llm_latency(&self) -> Result<()> {
        let run_ids = self
            .conn
            .prepare(
                "SELECT DISTINCT run_id
                 FROM llm_calls
                 WHERE observed_latency_quality IS NULL",
            )?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        self.refresh_observed_llm_latency_for_runs(run_ids.iter().map(String::as_str))
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

    fn table_has_column(&self, table: &str, column: &str) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM pragma_table_info({}) WHERE name = ?1",
                sql_string(table)
            ),
            params![column],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    fn recreate_rollups_if_legacy(&self) -> Result<()> {
        if self.table_has_column("usage_source_rollups", "profile_id")? {
            return Ok(());
        }

        self.conn.execute_batch(
            "DROP TABLE IF EXISTS usage_source_rollups;
             DROP TABLE IF EXISTS usage_model_rollups;
             DROP TABLE IF EXISTS usage_tool_rollups;
             DROP TABLE IF EXISTS usage_tool_model_rollups;",
        )?;
        self.conn
            .execute_batch(SCHEMA_SQL)
            .context("recreate profile-aware usage rollups")?;
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
        self.record_import_source_for_profile(
            source,
            source_kind,
            root_path,
            "local",
            "local_device",
        )
    }

    pub fn record_import_source_for_profile(
        &self,
        source: &str,
        source_kind: &str,
        root_path: &Path,
        profile_id: &str,
        device_id: &str,
    ) -> Result<String> {
        let root_path = root_path.to_string_lossy().to_string();
        let root_path_hash = stable_hash(&root_path);
        let source_id = source_id_for_profile(profile_id, source, &root_path_hash);
        let now = now_ns();

        self.conn.execute(
            "INSERT INTO import_sources (
                source_id, profile_id, device_id, source, source_kind, root_path, root_path_hash,
                enabled, last_scan_ns, parser_version
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, ?8, ?9)
             ON CONFLICT(source_id) DO UPDATE SET
                profile_id = excluded.profile_id,
                device_id = excluded.device_id,
                source_kind = excluded.source_kind,
                root_path = excluded.root_path,
                root_path_hash = excluded.root_path_hash,
                last_scan_ns = excluded.last_scan_ns,
                parser_version = excluded.parser_version",
            params![
                source_id,
                profile_id,
                device_id,
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
                file_id, profile_id, device_id, source_id, path, path_hash, size_bytes, modified_ns,
                fingerprint, parser_version, status, last_import_ns
             )
             VALUES (
                ?1,
                COALESCE((SELECT profile_id FROM import_sources WHERE source_id = ?2), 'local'),
                COALESCE((SELECT device_id FROM import_sources WHERE source_id = ?2), 'local_device'),
                ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10
             )
             ON CONFLICT(file_id) DO UPDATE SET
                profile_id = excluded.profile_id,
                device_id = excluded.device_id,
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
        let parser_version = IMPORT_PARSER_VERSION;

        let existing = self
            .conn
            .query_row(
                "SELECT fingerprint, parser_version, status, size_bytes, last_offset,
                    event_count, warning_count, metadata_json
                 FROM import_files
                 WHERE file_id = ?1",
                params![file_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, Option<String>>(7)?,
                    ))
                },
            )
            .optional()?;

        let previous_size_bytes = existing
            .as_ref()
            .map(|(_, _, _, size_bytes, _, _, _, _)| (*size_bytes).max(0) as u64);
        let previous_last_offset =
            existing
                .as_ref()
                .and_then(|(_, _, _, _, last_offset, _, _, _)| {
                    last_offset.map(|offset| offset.max(0) as u64)
                });
        let previous_event_count = existing
            .as_ref()
            .map(|(_, _, _, _, _, event_count, _, _)| (*event_count).max(0) as usize)
            .unwrap_or_default();
        let previous_warning_count = existing
            .as_ref()
            .map(|(_, _, _, _, _, _, warning_count, _)| (*warning_count).max(0) as usize)
            .unwrap_or_default();
        let previous_metadata_json = existing
            .as_ref()
            .and_then(|(_, _, _, _, _, _, _, metadata_json)| metadata_json.clone());

        let can_resume =
            existing
                .as_ref()
                .is_some_and(|(_, existing_parser_version, status, _, _, _, _, _)| {
                    existing_parser_version.as_deref() == Some(parser_version)
                        && matches!(status.as_str(), "imported" | "partial")
                });

        if let Some((existing_fingerprint, _, _, _, _, _, _, _)) = existing
            && existing_fingerprint == fingerprint
            && can_resume
        {
            return Ok(PreparedImportFile {
                file_id,
                should_import: false,
                can_resume,
                previous_size_bytes,
                last_offset: previous_last_offset,
                event_count: previous_event_count,
                warning_count: previous_warning_count,
                metadata_json: previous_metadata_json,
            });
        }

        self.conn.execute(
            "INSERT INTO import_files (
                file_id, profile_id, device_id, source_id, path, path_hash, size_bytes, modified_ns,
                fingerprint, parser_version, status, last_import_ns
             )
             VALUES (
                ?1,
                COALESCE((SELECT profile_id FROM import_sources WHERE source_id = ?2), 'local'),
                COALESCE((SELECT device_id FROM import_sources WHERE source_id = ?2), 'local_device'),
                ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending', ?9
             )
             ON CONFLICT(file_id) DO UPDATE SET
                profile_id = excluded.profile_id,
                device_id = excluded.device_id,
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
            can_resume,
            previous_size_bytes,
            last_offset: previous_last_offset,
            event_count: previous_event_count,
            warning_count: previous_warning_count,
            metadata_json: previous_metadata_json,
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

    pub fn update_import_file_metadata(&self, file_id: &str, metadata_json: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE import_files SET metadata_json = ?2 WHERE file_id = ?1",
            params![file_id, metadata_json],
        )?;
        Ok(())
    }

    pub fn latest_import_source_scan_ns(&self, source: &str) -> Result<Option<i64>> {
        self.latest_import_source_scan_ns_for_profile(source, "local")
    }

    pub fn latest_import_source_scan_ns_for_profile(
        &self,
        source: &str,
        profile_id: &str,
    ) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT MAX(last_scan_ns) FROM import_sources WHERE source = ?1 AND profile_id = ?2",
                params![source, profile_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten())
    }
}

fn set_shared_sqlite_modes(path: &Path) {
    if !path.starts_with(Path::new("/Users/Shared/Shirabe")) {
        return;
    }

    for candidate in [
        path.to_path_buf(),
        PathBuf::from(format!("{}-wal", path.display())),
        PathBuf::from(format!("{}-shm", path.display())),
    ] {
        let Ok(metadata) = fs::metadata(&candidate) else {
            continue;
        };
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o666);
        let _ = fs::set_permissions(&candidate, permissions);
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

fn env_hostname() -> Option<String> {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|value| !value.is_empty())
}

fn source_id_for_profile(profile_id: &str, source: &str, root_path_hash: &str) -> String {
    if profile_id == "local" {
        format!("{source}:{root_path_hash}")
    } else {
        format!("{profile_id}:{source}:{root_path_hash}")
    }
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

CREATE TABLE IF NOT EXISTS devices (
    device_id TEXT PRIMARY KEY,
    device_label TEXT NOT NULL,
    hostname TEXT,
    created_at_ns INTEGER NOT NULL,
    last_seen_ns INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS profiles (
    profile_id TEXT PRIMARY KEY,
    device_id TEXT NOT NULL,
    profile_label TEXT NOT NULL,
    macos_uid TEXT,
    macos_username TEXT NOT NULL,
    created_at_ns INTEGER NOT NULL,
    last_seen_ns INTEGER NOT NULL,
    FOREIGN KEY (device_id) REFERENCES devices(device_id)
);
CREATE INDEX IF NOT EXISTS idx_profiles_device ON profiles(device_id);

CREATE TABLE IF NOT EXISTS import_sources (
    source_id TEXT PRIMARY KEY,
    profile_id TEXT NOT NULL DEFAULT 'local',
    device_id TEXT NOT NULL DEFAULT 'local_device',
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
CREATE INDEX IF NOT EXISTS idx_import_sources_profile ON import_sources(profile_id, source);

CREATE TABLE IF NOT EXISTS import_files (
    file_id TEXT PRIMARY KEY,
    profile_id TEXT NOT NULL DEFAULT 'local',
    device_id TEXT NOT NULL DEFAULT 'local_device',
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
CREATE INDEX IF NOT EXISTS idx_import_files_profile_source ON import_files(profile_id, source_id);
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
    profile_id TEXT NOT NULL DEFAULT 'local',
    device_id TEXT NOT NULL DEFAULT 'local_device',
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
CREATE INDEX IF NOT EXISTS idx_sessions_profile_seen ON sessions(profile_id, last_seen_ns);
CREATE INDEX IF NOT EXISTS idx_sessions_source_seen ON sessions(source, last_seen_ns);
CREATE INDEX IF NOT EXISTS idx_sessions_last_seen ON sessions(last_seen_ns);

CREATE TABLE IF NOT EXISTS runs (
    run_id TEXT PRIMARY KEY,
    profile_id TEXT NOT NULL DEFAULT 'local',
    device_id TEXT NOT NULL DEFAULT 'local_device',
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
CREATE INDEX IF NOT EXISTS idx_runs_profile_started ON runs(profile_id, started_at_ns);
CREATE INDEX IF NOT EXISTS idx_runs_started ON runs(started_at_ns);
CREATE INDEX IF NOT EXISTS idx_runs_source_started ON runs(source, started_at_ns);
CREATE INDEX IF NOT EXISTS idx_runs_source_day ON runs(source, started_day);
CREATE INDEX IF NOT EXISTS idx_runs_source_month ON runs(source, started_month);
CREATE INDEX IF NOT EXISTS idx_runs_session ON runs(session_id);
CREATE INDEX IF NOT EXISTS idx_runs_session_started ON runs(session_id, started_at_ns);

CREATE TABLE IF NOT EXISTS turns (
    turn_id TEXT PRIMARY KEY,
    profile_id TEXT NOT NULL DEFAULT 'local',
    device_id TEXT NOT NULL DEFAULT 'local_device',
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
CREATE INDEX IF NOT EXISTS idx_turns_profile_started ON turns(profile_id, started_at_ns);
CREATE INDEX IF NOT EXISTS idx_turns_run ON turns(run_id);
CREATE INDEX IF NOT EXISTS idx_turns_session ON turns(session_id);
CREATE INDEX IF NOT EXISTS idx_turns_session_started ON turns(session_id, started_at_ns);
CREATE INDEX IF NOT EXISTS idx_turns_started ON turns(started_at_ns);
CREATE INDEX IF NOT EXISTS idx_turns_source_started ON turns(source, started_at_ns);
CREATE INDEX IF NOT EXISTS idx_turns_source_day ON turns(source, started_day);
CREATE INDEX IF NOT EXISTS idx_turns_source_month ON turns(source, started_month);

CREATE TABLE IF NOT EXISTS run_steps (
    step_id TEXT PRIMARY KEY,
    profile_id TEXT NOT NULL DEFAULT 'local',
    device_id TEXT NOT NULL DEFAULT 'local_device',
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
CREATE INDEX IF NOT EXISTS idx_run_steps_profile_started ON run_steps(profile_id, started_at_ns);

CREATE TABLE IF NOT EXISTS llm_calls (
    llm_call_id TEXT PRIMARY KEY,
    profile_id TEXT NOT NULL DEFAULT 'local',
    device_id TEXT NOT NULL DEFAULT 'local_device',
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
    observed_response_delay_ns INTEGER,
    observed_output_tps REAL,
    observed_trigger_step_id TEXT,
    observed_trigger_step_type TEXT,
    observed_latency_quality TEXT,
    trace_id TEXT,
    span_id TEXT,
    metadata_json TEXT,
    FOREIGN KEY (run_id) REFERENCES runs(run_id)
);
CREATE INDEX IF NOT EXISTS idx_llm_calls_profile_started ON llm_calls(profile_id, started_at_ns);
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
    profile_id TEXT NOT NULL DEFAULT 'local',
    device_id TEXT NOT NULL DEFAULT 'local_device',
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
CREATE INDEX IF NOT EXISTS idx_tool_calls_profile_started ON tool_calls(profile_id, started_at_ns);
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
    profile_id TEXT NOT NULL DEFAULT 'local',
    device_id TEXT NOT NULL DEFAULT 'local_device',
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
CREATE INDEX IF NOT EXISTS idx_run_signals_profile ON run_signals(profile_id, created_at_ns);

CREATE TABLE IF NOT EXISTS skill_events (
    skill_event_id TEXT PRIMARY KEY,
    profile_id TEXT NOT NULL DEFAULT 'local',
    device_id TEXT NOT NULL DEFAULT 'local_device',
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
CREATE INDEX IF NOT EXISTS idx_skill_events_profile_time
ON skill_events(profile_id, occurred_at_ns);
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
    profile_id TEXT NOT NULL DEFAULT 'local',
    device_id TEXT NOT NULL DEFAULT 'local_device',
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
    PRIMARY KEY (bucket, bucket_key, profile_id, source)
);
CREATE INDEX IF NOT EXISTS idx_usage_source_rollups_lookup
ON usage_source_rollups(bucket, bucket_key, profile_id, source);

CREATE TABLE IF NOT EXISTS usage_model_rollups (
    bucket TEXT NOT NULL,
    bucket_key TEXT NOT NULL,
    profile_id TEXT NOT NULL DEFAULT 'local',
    device_id TEXT NOT NULL DEFAULT 'local_device',
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
    PRIMARY KEY (bucket, bucket_key, profile_id, source, model)
);
CREATE INDEX IF NOT EXISTS idx_usage_model_rollups_lookup
ON usage_model_rollups(bucket, bucket_key, profile_id, source, model);

CREATE TABLE IF NOT EXISTS usage_tool_rollups (
    bucket TEXT NOT NULL,
    bucket_key TEXT NOT NULL,
    profile_id TEXT NOT NULL DEFAULT 'local',
    device_id TEXT NOT NULL DEFAULT 'local_device',
    source TEXT NOT NULL,
    tool_name TEXT NOT NULL,
    calls INTEGER NOT NULL DEFAULT 0,
    success_calls INTEGER NOT NULL DEFAULT 0,
    failed_calls INTEGER NOT NULL DEFAULT 0,
    updated_at_ns INTEGER NOT NULL,
    PRIMARY KEY (bucket, bucket_key, profile_id, source, tool_name)
);
CREATE INDEX IF NOT EXISTS idx_usage_tool_rollups_lookup
ON usage_tool_rollups(bucket, bucket_key, profile_id, source, tool_name);

CREATE TABLE IF NOT EXISTS usage_tool_model_rollups (
    bucket TEXT NOT NULL,
    bucket_key TEXT NOT NULL,
    profile_id TEXT NOT NULL DEFAULT 'local',
    device_id TEXT NOT NULL DEFAULT 'local_device',
    source TEXT NOT NULL,
    model TEXT NOT NULL,
    tool_name TEXT NOT NULL,
    calls INTEGER NOT NULL DEFAULT 0,
    success_calls INTEGER NOT NULL DEFAULT 0,
    failed_calls INTEGER NOT NULL DEFAULT 0,
    updated_at_ns INTEGER NOT NULL,
    PRIMARY KEY (bucket, bucket_key, profile_id, source, model, tool_name)
);
CREATE INDEX IF NOT EXISTS idx_usage_tool_model_rollups_lookup
ON usage_tool_model_rollups(bucket, bucket_key, profile_id, source, model, tool_name);
"#;
