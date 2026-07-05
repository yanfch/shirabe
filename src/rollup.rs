use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;
use std::time::Instant;

use crate::db::{Database, now_ns};

const LLM_COST_SQL: &str = "CASE
    WHEN l.total_cost_usd > 0 THEN l.total_cost_usd
    ELSE
        (l.input_tokens * COALESCE(mp.input_cost_per_token, 0)) +
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
    insert_source_rollups(conn, "day", "r.started_day", "t.started_day")?;
    insert_source_rollups(conn, "month", "r.started_month", "t.started_month")?;
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
    if rollups == 0 && events > 0 {
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
                COUNT(DISTINCT r.session_id) AS sessions,
                COALESCE(SUM(r.llm_call_count), 0) AS llm_calls,
                COALESCE(SUM(r.tool_call_count), 0) AS tool_calls,
                COALESCE(SUM(r.failed_tool_count), 0) AS failed_tool_calls,
                COALESCE(SUM(r.input_tokens), 0) AS input_tokens,
                COALESCE(SUM(r.output_tokens), 0) AS output_tokens,
                COALESCE(SUM(r.cache_read_tokens), 0) AS cache_read_tokens,
                COALESCE(SUM(r.cache_write_tokens), 0) AS cache_write_tokens,
                COALESCE(SUM(r.total_cost_usd), 0) AS total_cost_usd
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
        source_keys AS (
            SELECT profile_id, device_id, source, bucket_key FROM run_rollups
            UNION
            SELECT profile_id, device_id, source, bucket_key FROM turn_rollups
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
            COALESCE(rr.llm_calls, 0),
            COALESCE(rr.tool_calls, 0),
            COALESCE(rr.failed_tool_calls, 0),
            COALESCE(rr.input_tokens, 0),
            COALESCE(rr.output_tokens, 0),
            COALESCE(rr.cache_read_tokens, 0),
            COALESCE(rr.cache_write_tokens, 0),
            COALESCE(rr.total_cost_usd, 0),
            ?2
        FROM source_keys sk
        LEFT JOIN run_rollups rr
            ON rr.profile_id = sk.profile_id AND rr.source = sk.source AND rr.bucket_key = sk.bucket_key
        LEFT JOIN turn_rollups tr
            ON tr.profile_id = sk.profile_id AND tr.source = sk.source AND tr.bucket_key = sk.bucket_key"
    );
    conn.execute(&sql, (bucket, now))?;
    Ok(())
}

fn insert_model_rollups(conn: &Connection, bucket: &str, bucket_expr: &str) -> Result<()> {
    let now = now_ns();
    let sql = format!(
        "INSERT INTO usage_model_rollups (
            bucket, bucket_key, profile_id, device_id, source, model, sessions, runs, turns, llm_calls,
            input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
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
