use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use std::time::Instant;

use crate::db::{Database, now_ns};

const USAGE_ROLLUP_VERSION_KEY: &str = "usage_rollup_version";
const USAGE_ROLLUP_VERSION: &str = "2";

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

#[derive(Debug, Serialize)]
pub struct RollupRefreshReport {
    pub status: &'static str,
    pub source_rollups: i64,
    pub model_rollups: i64,
    pub tool_rollups: i64,
    pub tool_model_rollups: i64,
    pub timings: Vec<RollupTiming>,
}

#[derive(Debug, Serialize)]
pub struct RollupTiming {
    pub stage: &'static str,
    pub elapsed_ms: u128,
}

pub fn refresh_all(db: &Database) -> Result<RollupRefreshReport> {
    let tx = db.begin_batch()?;
    let conn = db.connection();
    let mut timings = Vec::new();

    let started = Instant::now();
    clear_rollups(conn)?;
    timings.push(RollupTiming {
        stage: "clear",
        elapsed_ms: started.elapsed().as_millis(),
    });

    let started = Instant::now();
    insert_source_rollups(
        conn,
        "day",
        "r.started_day",
        "t.started_day",
        "l.started_day",
        "tc.started_day",
    )?;
    insert_source_rollups(
        conn,
        "month",
        "r.started_month",
        "t.started_month",
        "l.started_month",
        "tc.started_month",
    )?;
    timings.push(RollupTiming {
        stage: "source_rollups",
        elapsed_ms: started.elapsed().as_millis(),
    });

    let started = Instant::now();
    insert_model_rollups(conn, "day", "l.started_day")?;
    insert_model_rollups(conn, "month", "l.started_month")?;
    timings.push(RollupTiming {
        stage: "model_rollups",
        elapsed_ms: started.elapsed().as_millis(),
    });

    let started = Instant::now();
    insert_tool_rollups(conn, "day", "tc.started_day")?;
    insert_tool_rollups(conn, "month", "tc.started_month")?;
    timings.push(RollupTiming {
        stage: "tool_rollups",
        elapsed_ms: started.elapsed().as_millis(),
    });

    let started = Instant::now();
    insert_tool_model_rollups(conn, "day", "tc.started_day")?;
    insert_tool_model_rollups(conn, "month", "tc.started_month")?;
    timings.push(RollupTiming {
        stage: "tool_model_rollups",
        elapsed_ms: started.elapsed().as_millis(),
    });

    conn.execute(
        "INSERT INTO meta (key, value, updated_at_ns)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET
            value = excluded.value,
            updated_at_ns = excluded.updated_at_ns",
        params![USAGE_ROLLUP_VERSION_KEY, USAGE_ROLLUP_VERSION, now_ns()],
    )?;

    let report = RollupRefreshReport {
        status: "ok",
        source_rollups: count(conn, "usage_source_rollups")?,
        model_rollups: count(conn, "usage_model_rollups")?,
        tool_rollups: count(conn, "usage_tool_rollups")?,
        tool_model_rollups: count(conn, "usage_tool_model_rollups")?,
        timings,
    };
    tx.commit()?;
    Ok(report)
}

pub fn refresh_if_missing(db: &Database) -> Result<Option<RollupRefreshReport>> {
    let conn = db.connection();
    let rollups = count(conn, "usage_source_rollups")?;
    let events = count(conn, "llm_calls")? + count(conn, "tool_calls")?;
    let version = conn
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![USAGE_ROLLUP_VERSION_KEY],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if events > 0 && (rollups == 0 || version.as_deref() != Some(USAGE_ROLLUP_VERSION)) {
        return refresh_all(db).map(Some);
    }

    Ok(None)
}

fn clear_rollups(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "DELETE FROM usage_source_rollups;
         DELETE FROM usage_model_rollups;
         DELETE FROM usage_tool_rollups;
         DELETE FROM usage_tool_model_rollups;",
    )?;
    Ok(())
}

fn insert_source_rollups(
    conn: &Connection,
    bucket: &str,
    run_bucket_expr: &str,
    turn_bucket_expr: &str,
    llm_bucket_expr: &str,
    tool_bucket_expr: &str,
) -> Result<()> {
    let now = now_ns();
    let sql = format!(
        "INSERT INTO usage_source_rollups (
            bucket, bucket_key, profile_id, device_id, source, sessions, runs, turns, llm_calls,
            tool_calls, failed_tool_calls, input_tokens, output_tokens,
            cache_read_tokens, cache_write_tokens, total_cost_usd, updated_at_ns
        )
        WITH run_rollups AS (
            SELECT
                r.profile_id,
                r.device_id,
                r.source,
                {run_bucket_expr} AS bucket_key,
                COUNT(*) AS runs,
                COUNT(DISTINCT r.session_id) AS sessions
            FROM runs r
            GROUP BY r.profile_id, r.device_id, r.source, bucket_key
            HAVING bucket_key IS NOT NULL
        ),
        turn_rollups AS (
            SELECT
                t.profile_id,
                t.device_id,
                t.source,
                {turn_bucket_expr} AS bucket_key,
                COUNT(*) AS turns
            FROM turns t
            GROUP BY t.profile_id, t.device_id, t.source, bucket_key
            HAVING bucket_key IS NOT NULL
        ),
        llm_rollups AS (
            SELECT
                l.profile_id,
                l.device_id,
                l.source,
                {llm_bucket_expr} AS bucket_key,
                COUNT(*) AS llm_calls,
                COALESCE(SUM(l.input_tokens), 0) AS input_tokens,
                COALESCE(SUM(l.output_tokens), 0) AS output_tokens,
                COALESCE(SUM(l.cache_read_tokens), 0) AS cache_read_tokens,
                COALESCE(SUM(l.cache_write_tokens), 0) AS cache_write_tokens,
                COALESCE(SUM({LLM_COST_SQL}), 0) AS total_cost_usd
            FROM llm_calls l
            LEFT JOIN model_prices mp ON mp.model_name = l.model
            GROUP BY l.profile_id, l.device_id, l.source, bucket_key
            HAVING bucket_key IS NOT NULL
        ),
        tool_rollups AS (
            SELECT
                tc.profile_id,
                tc.device_id,
                tc.source,
                {tool_bucket_expr} AS bucket_key,
                COUNT(*) AS tool_calls,
                COALESCE(SUM(CASE WHEN tc.status = 'failed' THEN 1 ELSE 0 END), 0) AS failed_tool_calls
            FROM tool_calls tc
            GROUP BY tc.profile_id, tc.device_id, tc.source, bucket_key
            HAVING bucket_key IS NOT NULL
        ),
        source_keys AS (
            SELECT profile_id, device_id, source, bucket_key FROM run_rollups
            UNION
            SELECT profile_id, device_id, source, bucket_key FROM turn_rollups
            UNION
            SELECT profile_id, device_id, source, bucket_key FROM llm_rollups
            UNION
            SELECT profile_id, device_id, source, bucket_key FROM tool_rollups
        )
        SELECT
            ?1,
            sk.bucket_key,
            sk.profile_id,
            sk.device_id,
            sk.source,
            COALESCE(rr.sessions, 0),
            COALESCE(rr.runs, 0),
            COALESCE(tr.turns, 0),
            COALESCE(lr.llm_calls, 0),
            COALESCE(tlr.tool_calls, 0),
            COALESCE(tlr.failed_tool_calls, 0),
            COALESCE(lr.input_tokens, 0),
            COALESCE(lr.output_tokens, 0),
            COALESCE(lr.cache_read_tokens, 0),
            COALESCE(lr.cache_write_tokens, 0),
            COALESCE(lr.total_cost_usd, 0),
            ?2
        FROM source_keys sk
        LEFT JOIN run_rollups rr
            ON rr.profile_id = sk.profile_id AND rr.device_id = sk.device_id AND rr.source = sk.source AND rr.bucket_key = sk.bucket_key
        LEFT JOIN turn_rollups tr
            ON tr.profile_id = sk.profile_id AND tr.device_id = sk.device_id AND tr.source = sk.source AND tr.bucket_key = sk.bucket_key
        LEFT JOIN llm_rollups lr
            ON lr.profile_id = sk.profile_id AND lr.device_id = sk.device_id AND lr.source = sk.source AND lr.bucket_key = sk.bucket_key
        LEFT JOIN tool_rollups tlr
            ON tlr.profile_id = sk.profile_id AND tlr.device_id = sk.device_id AND tlr.source = sk.source AND tlr.bucket_key = sk.bucket_key"
    );
    conn.execute(&sql, (bucket, now))?;
    Ok(())
}

fn insert_model_rollups(conn: &Connection, bucket: &str, bucket_expr: &str) -> Result<()> {
    let now = now_ns();
    let sql = format!(
        "INSERT INTO usage_model_rollups (
            bucket, bucket_key, profile_id, device_id, source, model, sessions, runs, turns, llm_calls,
            input_tokens, uncached_input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
            total_cost_usd, updated_at_ns
        )
        SELECT
            ?1,
            {bucket_expr} AS bucket_key,
            l.profile_id,
            l.device_id,
            l.source,
            COALESCE(NULLIF(l.model, ''), 'unknown') AS model,
            COUNT(DISTINCT l.session_id),
            COUNT(DISTINCT l.run_id),
            COUNT(DISTINCT l.turn_id),
            COUNT(*),
            COALESCE(SUM(l.input_tokens), 0),
            COALESCE(SUM(l.uncached_input_tokens), 0),
            COALESCE(SUM(l.output_tokens), 0),
            COALESCE(SUM(l.cache_read_tokens), 0),
            COALESCE(SUM(l.cache_write_tokens), 0),
            COALESCE(SUM({LLM_COST_SQL}), 0),
            ?2
        FROM llm_calls l
        LEFT JOIN model_prices mp ON mp.model_name = l.model
        GROUP BY bucket_key, l.profile_id, l.device_id, l.source, COALESCE(NULLIF(l.model, ''), 'unknown')
        HAVING bucket_key IS NOT NULL"
    );
    conn.execute(&sql, (bucket, now))?;
    Ok(())
}

fn insert_tool_rollups(conn: &Connection, bucket: &str, bucket_expr: &str) -> Result<()> {
    let now = now_ns();
    let sql = format!(
        "INSERT INTO usage_tool_rollups (
            bucket, bucket_key, profile_id, device_id, source, tool_name, calls, success_calls,
            failed_calls, updated_at_ns
        )
        SELECT
            ?1,
            {bucket_expr} AS bucket_key,
            tc.profile_id,
            tc.device_id,
            tc.source,
            tc.tool_name,
            COUNT(*),
            COALESCE(SUM(CASE WHEN tc.status = 'success' THEN 1 ELSE 0 END), 0),
            COALESCE(SUM(CASE WHEN tc.status = 'failed' THEN 1 ELSE 0 END), 0),
            ?2
        FROM tool_calls tc
        GROUP BY bucket_key, tc.profile_id, tc.device_id, tc.source, tc.tool_name
        HAVING bucket_key IS NOT NULL"
    );
    conn.execute(&sql, (bucket, now))?;
    Ok(())
}

fn insert_tool_model_rollups(conn: &Connection, bucket: &str, bucket_expr: &str) -> Result<()> {
    let now = now_ns();
    let sql = format!(
        "INSERT INTO usage_tool_model_rollups (
            bucket, bucket_key, profile_id, device_id, source, model, tool_name, calls, success_calls,
            failed_calls, updated_at_ns
        )
        WITH run_models AS (
            SELECT DISTINCT
                run_id,
                profile_id,
                device_id,
                source,
                COALESCE(NULLIF(model, ''), 'unknown') AS model
            FROM llm_calls
        )
        SELECT
            ?1,
            {bucket_expr} AS bucket_key,
            tc.profile_id,
            tc.device_id,
            tc.source,
            rm.model,
            tc.tool_name,
            COUNT(*),
            COALESCE(SUM(CASE WHEN tc.status = 'success' THEN 1 ELSE 0 END), 0),
            COALESCE(SUM(CASE WHEN tc.status = 'failed' THEN 1 ELSE 0 END), 0),
            ?2
        FROM tool_calls tc
        JOIN run_models rm ON rm.run_id = tc.run_id AND rm.profile_id = tc.profile_id AND rm.source = tc.source
        GROUP BY bucket_key, tc.profile_id, tc.device_id, tc.source, rm.model, tc.tool_name
        HAVING bucket_key IS NOT NULL"
    );
    conn.execute(&sql, (bucket, now))?;
    Ok(())
}

fn count(conn: &Connection, table: &str) -> Result<i64> {
    let sql = format!("SELECT COUNT(*) FROM {table}");
    Ok(conn.query_row(&sql, [], |row| row.get(0))?)
}

#[cfg(test)]
mod tests {
    use std::{fs, time::SystemTime};

    use anyhow::Result;

    use super::refresh_all;
    use crate::db::Database;

    #[test]
    fn source_rollup_attributes_llm_usage_by_call_time() -> Result<()> {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_nanos();
        let db_path = std::env::temp_dir().join(format!(
            "shirabe-source-rollup-call-time-{}-{unique}.sqlite",
            std::process::id()
        ));

        {
            let db = Database::open(&db_path)?;
            db.migrate()?;
            db.connection().execute_batch(
                "INSERT INTO sessions (
                    session_id, profile_id, device_id, source, kind,
                    first_seen_ns, last_seen_ns
                 ) VALUES (
                    'amp:session', 'profile', 'device', 'amp', 'amp_thread', 1, 2
                 );
                 INSERT INTO runs (
                    run_id, profile_id, device_id, source, kind, status, session_id,
                    started_at_ns, started_day, started_month
                 ) VALUES (
                    'amp:run', 'profile', 'device', 'amp', 'amp_thread', 'success',
                    'amp:session', 1, '2026-08-04', '2026-08'
                 );
                 INSERT INTO llm_calls (
                    llm_call_id, profile_id, device_id, run_id, session_id, source,
                    model, status, output_tokens, cache_read_tokens, cache_write_tokens,
                    total_cost_usd, started_at_ns, started_day, started_month
                 ) VALUES (
                    'amp:call', 'profile', 'device', 'amp:run', 'amp:session', 'amp',
                    'gpt-5.6-sol', 'success', 25, 1000, 200, 0.5,
                    2, '2026-08-05', '2026-08'
                 );",
            )?;

            refresh_all(&db)?;

            let actual = db.connection().query_row(
                "SELECT llm_calls, output_tokens, cache_read_tokens, cache_write_tokens
                 FROM usage_source_rollups
                 WHERE bucket = 'day' AND bucket_key = '2026-08-05'
                   AND profile_id = 'profile' AND device_id = 'device' AND source = 'amp'",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )?;
            assert_eq!(actual, (1, 25, 1000, 200));
        }

        let _ = fs::remove_file(db_path);
        Ok(())
    }
}
