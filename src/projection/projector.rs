use std::collections::HashSet;

use anyhow::Result;
use rusqlite::{Connection, Params, params};
use serde_json::{Map, Value, json};

use crate::{
    db::{Database, now_ns, stable_hash},
    projection::ids,
};

use super::event::{NormalizedEvent, OperationStatus, OperationType};

pub struct Projector<'a> {
    conn: &'a Connection,
}

#[derive(Default)]
pub struct ProjectionCache {
    sessions: HashSet<String>,
    runs: HashSet<String>,
    turns: HashSet<String>,
}

#[derive(Debug, Clone)]
pub struct ProjectionResult {
    pub session_id: Option<String>,
    pub run_id: String,
    pub turn_id: Option<String>,
    pub step_id: String,
    pub llm_call_id: Option<String>,
    pub tool_call_id: Option<String>,
}

impl<'a> Projector<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self {
            conn: db.connection(),
        }
    }

    pub fn project(&self, event: &NormalizedEvent) -> Result<ProjectionResult> {
        let result = self.project_inner(event, None, true, true, true, true)?;
        self.refresh_run_summary(&result.run_id)?;
        Ok(result)
    }

    pub fn project_without_summary(&self, event: &NormalizedEvent) -> Result<ProjectionResult> {
        self.project_inner(event, None, true, true, true, true)
    }

    pub fn project_with_cache(
        &self,
        event: &NormalizedEvent,
        cache: &mut ProjectionCache,
    ) -> Result<ProjectionResult> {
        self.project_inner(event, Some(cache), true, false, true, false)
    }

    fn project_inner(
        &self,
        event: &NormalizedEvent,
        mut cache: Option<&mut ProjectionCache>,
        emit_signals: bool,
        store_step_metadata: bool,
        store_call_steps: bool,
        store_call_metadata: bool,
    ) -> Result<ProjectionResult> {
        let session_id = event
            .session
            .as_ref()
            .map(|session| ids::session_id(&event.source, &session.external_id));
        let run_id = ids::run_id(&event.source, &event.run.external_id);
        let turn_id = event
            .turn
            .as_ref()
            .map(|turn| ids::turn_id(&event.source, &turn.external_id));

        let should_upsert_session = if let Some(session_id) = session_id.as_ref() {
            match cache.as_mut() {
                Some(cache) => cache.sessions.insert(session_id.clone()),
                None => true,
            }
        } else {
            false
        };
        if should_upsert_session {
            self.upsert_session(event, session_id.as_deref())?;
        }

        let should_upsert_run = match cache.as_mut() {
            Some(cache) => cache.runs.insert(run_id.clone()),
            None => true,
        };
        if should_upsert_run {
            self.upsert_run(event, &run_id, session_id.as_deref())?;
        }

        let should_upsert_turn = if let Some(turn_id) = turn_id.as_ref() {
            match cache.as_mut() {
                Some(cache) => cache.turns.insert(turn_id.clone()),
                None => true,
            }
        } else {
            false
        };
        if should_upsert_turn {
            self.upsert_turn(event, &run_id, session_id.as_deref(), turn_id.as_deref())?;
        }

        self.upsert_skill_events(event, &run_id, session_id.as_deref(), turn_id.as_deref())?;

        let llm_call_id = if event.operation.operation_type == OperationType::LlmCall {
            let id = ids::event_scoped_id(
                &event.source,
                "llm",
                event.source_event_id.as_deref(),
                &run_id,
            );
            self.upsert_llm_call(
                event,
                &run_id,
                session_id.as_deref(),
                turn_id.as_deref(),
                &id,
                store_call_metadata,
            )?;
            if emit_signals {
                self.upsert_llm_signals(event, &run_id, &id)?;
            }
            Some(id)
        } else {
            None
        };

        let tool_call_id = if event.operation.operation_type == OperationType::ToolCall {
            let id = ids::event_scoped_id(
                &event.source,
                "tool",
                event.source_event_id.as_deref(),
                &run_id,
            );
            self.upsert_tool_call(
                event,
                &run_id,
                session_id.as_deref(),
                turn_id.as_deref(),
                &id,
                store_call_metadata,
            )?;
            if emit_signals {
                self.upsert_tool_signals(event, &run_id, &id)?;
            }
            Some(id)
        } else {
            None
        };

        let step_id = ids::event_scoped_id(
            &event.source,
            "step",
            event.source_event_id.as_deref(),
            &format!("{}:{}", run_id, event.operation.name),
        );
        if store_call_steps || (llm_call_id.is_none() && tool_call_id.is_none()) {
            self.upsert_run_step(
                event,
                &run_id,
                session_id.as_deref(),
                turn_id.as_deref(),
                &step_id,
                llm_call_id.as_deref(),
                tool_call_id.as_deref(),
                store_step_metadata,
            )?;
        }

        Ok(ProjectionResult {
            session_id,
            run_id,
            turn_id,
            step_id,
            llm_call_id,
            tool_call_id,
        })
    }

    pub fn refresh_run_summaries<'b, I>(&self, run_ids: I) -> Result<()>
    where
        I: IntoIterator<Item = &'b str>,
    {
        for run_id in run_ids {
            self.refresh_run_summary(run_id)?;
        }
        Ok(())
    }

    fn upsert_skill_events(
        &self,
        event: &NormalizedEvent,
        run_id: &str,
        session_id: Option<&str>,
        turn_id: Option<&str>,
    ) -> Result<()> {
        for (index, skill_event) in event.skill_events.iter().enumerate() {
            let source_event_id = event.source_event_id.as_deref().unwrap_or_default();
            let identity = format!(
                "{}:{}:{}:{}:{}",
                run_id,
                source_event_id,
                skill_event.skill_name,
                skill_event.event_type.as_str(),
                index
            );
            let skill_event_id = format!("{}:skill:{}", event.source, stable_hash(&identity));
            let metadata_json = skill_event
                .metadata
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?;

            execute_cached(
                self.conn,
                "INSERT INTO skill_events (
                    skill_event_id, source, skill_name, event_type, confidence,
                    session_id, run_id, turn_id, source_event_id, source_ref,
                    occurred_at_ns, occurred_day, occurred_month, metadata_json
                 )
                 VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                    date(?11 / 1000000000, 'unixepoch', 'localtime'),
                    strftime('%Y-%m', ?11 / 1000000000, 'unixepoch', 'localtime'),
                    ?12
                 )
                 ON CONFLICT(skill_event_id) DO UPDATE SET
                    confidence = excluded.confidence,
                    session_id = COALESCE(excluded.session_id, skill_events.session_id),
                    run_id = excluded.run_id,
                    turn_id = COALESCE(excluded.turn_id, skill_events.turn_id),
                    source_ref = COALESCE(excluded.source_ref, skill_events.source_ref),
                    occurred_at_ns = excluded.occurred_at_ns,
                    occurred_day = excluded.occurred_day,
                    occurred_month = excluded.occurred_month,
                    metadata_json = COALESCE(excluded.metadata_json, skill_events.metadata_json)",
                params![
                    skill_event_id,
                    event.source,
                    skill_event.skill_name,
                    skill_event.event_type.as_str(),
                    skill_event.confidence,
                    session_id,
                    run_id,
                    turn_id,
                    event.source_event_id,
                    event.source_ref,
                    event.occurred_at_ns,
                    metadata_json
                ],
            )?;
        }

        Ok(())
    }

    pub fn refresh_session_summaries<'b, I>(&self, session_ids: I) -> Result<()>
    where
        I: IntoIterator<Item = &'b str>,
    {
        for session_id in session_ids {
            self.refresh_session_summary(session_id)?;
        }
        Ok(())
    }

    fn upsert_session(&self, event: &NormalizedEvent, session_id: Option<&str>) -> Result<()> {
        let Some(session_id) = session_id else {
            return Ok(());
        };
        let session = event
            .session
            .as_ref()
            .expect("session exists with session_id");
        let metadata_json = metadata_json(event)?;

        execute_cached(
            self.conn,
            "INSERT INTO sessions (
                session_id, source, kind, title, external_id, project_id, cwd,
                first_seen_ns, last_seen_ns, metadata_json
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(session_id) DO UPDATE SET
                title = COALESCE(excluded.title, sessions.title),
                project_id = COALESCE(excluded.project_id, sessions.project_id),
                cwd = COALESCE(excluded.cwd, sessions.cwd),
                first_seen_ns = MIN(sessions.first_seen_ns, excluded.first_seen_ns),
                last_seen_ns = MAX(sessions.last_seen_ns, excluded.last_seen_ns),
                metadata_json = COALESCE(excluded.metadata_json, sessions.metadata_json)",
            params![
                session_id,
                event.source,
                session.kind,
                session.title,
                session.external_id,
                event.project_id,
                event.cwd,
                event.occurred_at_ns,
                event.occurred_at_ns,
                metadata_json
            ],
        )?;

        Ok(())
    }

    fn upsert_run(
        &self,
        event: &NormalizedEvent,
        run_id: &str,
        session_id: Option<&str>,
    ) -> Result<()> {
        let metadata_json = metadata_json(event)?;

        execute_cached(
            self.conn,
            "INSERT INTO runs (
                run_id, source, kind, title, status, trace_id, root_span_id,
                session_id, external_id, cwd, project_id, started_at_ns, ended_at_ns,
                started_day, started_month, duration_ns, input_tokens, output_tokens,
                cache_read_tokens, cache_write_tokens, total_cost_usd,
                max_context_window_percent, metadata_json
             )
             VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                date(?12 / 1000000000, 'unixepoch', 'localtime'),
                strftime('%Y-%m', ?12 / 1000000000, 'unixepoch', 'localtime'),
                ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21
             )
             ON CONFLICT(run_id) DO UPDATE SET
                title = COALESCE(excluded.title, runs.title),
                status = excluded.status,
                trace_id = COALESCE(excluded.trace_id, runs.trace_id),
                root_span_id = COALESCE(excluded.root_span_id, runs.root_span_id),
                session_id = COALESCE(excluded.session_id, runs.session_id),
                cwd = COALESCE(excluded.cwd, runs.cwd),
                project_id = COALESCE(excluded.project_id, runs.project_id),
                started_day = CASE WHEN excluded.started_at_ns <= runs.started_at_ns THEN excluded.started_day ELSE runs.started_day END,
                started_month = CASE WHEN excluded.started_at_ns <= runs.started_at_ns THEN excluded.started_month ELSE runs.started_month END,
                started_at_ns = MIN(runs.started_at_ns, excluded.started_at_ns),
                ended_at_ns = COALESCE(excluded.ended_at_ns, runs.ended_at_ns),
                duration_ns = COALESCE(excluded.duration_ns, runs.duration_ns),
                input_tokens = MAX(runs.input_tokens, excluded.input_tokens),
                output_tokens = MAX(runs.output_tokens, excluded.output_tokens),
                cache_read_tokens = MAX(runs.cache_read_tokens, excluded.cache_read_tokens),
                cache_write_tokens = MAX(runs.cache_write_tokens, excluded.cache_write_tokens),
                total_cost_usd = MAX(runs.total_cost_usd, excluded.total_cost_usd),
                max_context_window_percent = MAX(runs.max_context_window_percent, excluded.max_context_window_percent),
                metadata_json = COALESCE(excluded.metadata_json, runs.metadata_json)",
            params![
                run_id,
                event.source,
                event.run.kind,
                event.run.title,
                event.operation.status.as_str(),
                event.trace.trace_id,
                event.trace.root_span_id,
                session_id,
                event.run.external_id,
                event.cwd,
                event.project_id,
                event.operation.started_at_ns,
                event.operation.ended_at_ns,
                event.operation.duration_ns,
                event.usage.input_tokens,
                event.usage.output_tokens,
                event.usage.cache_read_tokens,
                event.usage.cache_write_tokens,
                event.usage.total_cost_usd,
                event.usage.context_window_percent,
                metadata_json
            ],
        )?;

        Ok(())
    }

    fn upsert_turn(
        &self,
        event: &NormalizedEvent,
        run_id: &str,
        session_id: Option<&str>,
        turn_id: Option<&str>,
    ) -> Result<()> {
        let Some(turn_id) = turn_id else {
            return Ok(());
        };
        let turn = event.turn.as_ref().expect("turn exists with turn_id");
        let metadata_json = metadata_json(event)?;

        execute_cached(
            self.conn,
            "INSERT INTO turns (
                turn_id, run_id, session_id, source, turn_index, role, status,
                started_at_ns, started_day, started_month, ended_at_ns, duration_ns, input_tokens,
                output_tokens, trace_id, span_id, metadata_json
             )
             VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
                date(?8 / 1000000000, 'unixepoch', 'localtime'),
                strftime('%Y-%m', ?8 / 1000000000, 'unixepoch', 'localtime'),
                ?9, ?10, ?11, ?12, ?13, ?14, ?15
             )
             ON CONFLICT(turn_id) DO UPDATE SET
                turn_index = COALESCE(excluded.turn_index, turns.turn_index),
                role = COALESCE(excluded.role, turns.role),
                status = COALESCE(excluded.status, turns.status),
                started_day = CASE WHEN excluded.started_at_ns <= turns.started_at_ns THEN excluded.started_day ELSE turns.started_day END,
                started_month = CASE WHEN excluded.started_at_ns <= turns.started_at_ns THEN excluded.started_month ELSE turns.started_month END,
                ended_at_ns = COALESCE(excluded.ended_at_ns, turns.ended_at_ns),
                duration_ns = COALESCE(excluded.duration_ns, turns.duration_ns),
                input_tokens = MAX(turns.input_tokens, excluded.input_tokens),
                output_tokens = MAX(turns.output_tokens, excluded.output_tokens),
                metadata_json = COALESCE(excluded.metadata_json, turns.metadata_json)",
            params![
                turn_id,
                run_id,
                session_id,
                event.source,
                turn.index,
                turn.role,
                event.operation.status.as_str(),
                event.operation.started_at_ns,
                event.operation.ended_at_ns,
                event.operation.duration_ns,
                event.usage.input_tokens,
                event.usage.output_tokens,
                event.trace.trace_id,
                event.trace.span_id,
                metadata_json
            ],
        )?;

        Ok(())
    }

    fn upsert_run_step(
        &self,
        event: &NormalizedEvent,
        run_id: &str,
        session_id: Option<&str>,
        turn_id: Option<&str>,
        step_id: &str,
        llm_call_id: Option<&str>,
        tool_call_id: Option<&str>,
        store_metadata: bool,
    ) -> Result<()> {
        let metadata_json = if store_metadata {
            metadata_json(event)?
        } else {
            None
        };

        execute_cached(
            self.conn,
            "INSERT INTO run_steps (
                step_id, run_id, session_id, turn_id, source, source_event_id,
                source_ref, step_type, name, status, error_type, started_at_ns,
                ended_at_ns, duration_ns, order_index, llm_call_id, tool_call_id,
                trace_id, span_id, input_tokens, output_tokens,
                estimated_wasted_tokens, cost_usd, metadata_json
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, 0, ?22, ?23)
             ON CONFLICT(step_id) DO UPDATE SET
                session_id = COALESCE(excluded.session_id, run_steps.session_id),
                turn_id = COALESCE(excluded.turn_id, run_steps.turn_id),
                status = excluded.status,
                error_type = COALESCE(excluded.error_type, run_steps.error_type),
                ended_at_ns = COALESCE(excluded.ended_at_ns, run_steps.ended_at_ns),
                duration_ns = COALESCE(excluded.duration_ns, run_steps.duration_ns),
                llm_call_id = COALESCE(excluded.llm_call_id, run_steps.llm_call_id),
                tool_call_id = COALESCE(excluded.tool_call_id, run_steps.tool_call_id),
                input_tokens = MAX(run_steps.input_tokens, excluded.input_tokens),
                output_tokens = MAX(run_steps.output_tokens, excluded.output_tokens),
                cost_usd = MAX(run_steps.cost_usd, excluded.cost_usd),
                metadata_json = COALESCE(excluded.metadata_json, run_steps.metadata_json)",
            params![
                step_id,
                run_id,
                session_id,
                turn_id,
                event.source,
                event.source_event_id,
                event.source_ref,
                event.operation.operation_type.as_step_type(),
                event.operation.name,
                event.operation.status.as_str(),
                event.operation.error_type,
                event.operation.started_at_ns,
                event.operation.ended_at_ns,
                event.operation.duration_ns,
                event.operation.order_index,
                llm_call_id,
                tool_call_id,
                event.trace.trace_id,
                event.trace.span_id,
                event.usage.input_tokens,
                event.usage.output_tokens,
                event.usage.total_cost_usd,
                metadata_json
            ],
        )?;

        Ok(())
    }

    fn upsert_llm_call(
        &self,
        event: &NormalizedEvent,
        run_id: &str,
        session_id: Option<&str>,
        turn_id: Option<&str>,
        llm_call_id: &str,
        store_metadata: bool,
    ) -> Result<()> {
        let metadata_json = if store_metadata {
            metadata_json(event)?
        } else {
            None
        };

        execute_cached(
            self.conn,
            "INSERT INTO llm_calls (
                llm_call_id, run_id, session_id, turn_id, source, provider,
                model, operation, status, error_type, input_tokens, output_tokens,
                reasoning_tokens, uncached_input_tokens, cache_read_tokens,
                cache_write_tokens, cache_ratio, model_context_window,
                context_window_percent, total_cost_usd, pricing_status,
                cost_confidence, started_at_ns, ended_at_ns, duration_ns,
                started_day, started_month, trace_id, span_id, metadata_json
             )
             VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25,
                date(?23 / 1000000000, 'unixepoch', 'localtime'),
                strftime('%Y-%m', ?23 / 1000000000, 'unixepoch', 'localtime'),
                ?26, ?27, ?28
             )
             ON CONFLICT(llm_call_id) DO UPDATE SET
                session_id = COALESCE(excluded.session_id, llm_calls.session_id),
                turn_id = COALESCE(excluded.turn_id, llm_calls.turn_id),
                provider = COALESCE(excluded.provider, llm_calls.provider),
                model = COALESCE(excluded.model, llm_calls.model),
                operation = COALESCE(excluded.operation, llm_calls.operation),
                status = excluded.status,
                error_type = COALESCE(excluded.error_type, llm_calls.error_type),
                input_tokens = excluded.input_tokens,
                output_tokens = excluded.output_tokens,
                reasoning_tokens = excluded.reasoning_tokens,
                uncached_input_tokens = excluded.uncached_input_tokens,
                cache_read_tokens = excluded.cache_read_tokens,
                cache_write_tokens = excluded.cache_write_tokens,
                cache_ratio = excluded.cache_ratio,
                model_context_window = COALESCE(excluded.model_context_window, llm_calls.model_context_window),
                context_window_percent = excluded.context_window_percent,
                total_cost_usd = excluded.total_cost_usd,
                pricing_status = COALESCE(excluded.pricing_status, llm_calls.pricing_status),
                cost_confidence = COALESCE(excluded.cost_confidence, llm_calls.cost_confidence),
                started_day = excluded.started_day,
                started_month = excluded.started_month,
                trace_id = COALESCE(excluded.trace_id, llm_calls.trace_id),
                span_id = COALESCE(excluded.span_id, llm_calls.span_id),
                ended_at_ns = COALESCE(excluded.ended_at_ns, llm_calls.ended_at_ns),
                duration_ns = COALESCE(excluded.duration_ns, llm_calls.duration_ns),
                metadata_json = COALESCE(excluded.metadata_json, llm_calls.metadata_json)",
            params![
                llm_call_id,
                run_id,
                session_id,
                turn_id,
                event.source,
                event.usage.provider,
                event.usage.model,
                event.operation.name,
                event.operation.status.as_str(),
                event.operation.error_type,
                event.usage.input_tokens,
                event.usage.output_tokens,
                event.usage.reasoning_tokens,
                event.usage.uncached_input_tokens,
                event.usage.cache_read_tokens,
                event.usage.cache_write_tokens,
                cache_ratio(event.usage.input_tokens, event.usage.cache_read_tokens),
                event.usage.model_context_window,
                event.usage.context_window_percent,
                event.usage.total_cost_usd,
                event.usage.pricing_status,
                event.usage.cost_confidence,
                event.operation.started_at_ns,
                event.operation.ended_at_ns,
                event.operation.duration_ns,
                event.trace.trace_id,
                event.trace.span_id,
                metadata_json
            ],
        )?;

        Ok(())
    }

    fn upsert_tool_call(
        &self,
        event: &NormalizedEvent,
        run_id: &str,
        session_id: Option<&str>,
        turn_id: Option<&str>,
        tool_call_id: &str,
        store_metadata: bool,
    ) -> Result<()> {
        let metadata_json = if store_metadata {
            metadata_json(event)?
        } else {
            None
        };
        let output_bytes = metadata_i64(event, "output_bytes");

        execute_cached(
            self.conn,
            "INSERT INTO tool_calls (
                tool_call_id, run_id, session_id, turn_id, source, tool_name,
                status, error_type, output_bytes, started_at_ns, ended_at_ns, duration_ns,
                started_day, started_month, trace_id, span_id, metadata_json
             )
             VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                date(?10 / 1000000000, 'unixepoch', 'localtime'),
                strftime('%Y-%m', ?10 / 1000000000, 'unixepoch', 'localtime'),
                ?13, ?14, ?15
             )
             ON CONFLICT(tool_call_id) DO UPDATE SET
                session_id = COALESCE(excluded.session_id, tool_calls.session_id),
                turn_id = COALESCE(excluded.turn_id, tool_calls.turn_id),
                status = excluded.status,
                error_type = COALESCE(excluded.error_type, tool_calls.error_type),
                output_bytes = COALESCE(excluded.output_bytes, tool_calls.output_bytes),
                started_day = excluded.started_day,
                started_month = excluded.started_month,
                trace_id = COALESCE(excluded.trace_id, tool_calls.trace_id),
                span_id = COALESCE(excluded.span_id, tool_calls.span_id),
                ended_at_ns = COALESCE(excluded.ended_at_ns, tool_calls.ended_at_ns),
                duration_ns = COALESCE(excluded.duration_ns, tool_calls.duration_ns),
                metadata_json = COALESCE(excluded.metadata_json, tool_calls.metadata_json)",
            params![
                tool_call_id,
                run_id,
                session_id,
                turn_id,
                event.source,
                event.operation.name,
                event.operation.status.as_str(),
                event.operation.error_type,
                output_bytes,
                event.operation.started_at_ns,
                event.operation.ended_at_ns,
                event.operation.duration_ns,
                event.trace.trace_id,
                event.trace.span_id,
                metadata_json
            ],
        )?;

        Ok(())
    }

    fn refresh_run_summary(&self, run_id: &str) -> Result<()> {
        execute_cached(
            self.conn,
            "UPDATE runs SET
                llm_call_count = (SELECT COUNT(*) FROM llm_calls WHERE run_id = ?1),
                tool_call_count = (SELECT COUNT(*) FROM tool_calls WHERE run_id = ?1),
                failed_tool_count = (SELECT COUNT(*) FROM tool_calls WHERE run_id = ?1 AND status = 'failed'),
                error_count = (
                    SELECT COUNT(*) FROM run_steps
                    WHERE run_id = ?1 AND status = 'failed'
                ),
                input_tokens = COALESCE((SELECT SUM(input_tokens) FROM llm_calls WHERE run_id = ?1), 0),
                output_tokens = COALESCE((SELECT SUM(output_tokens) FROM llm_calls WHERE run_id = ?1), 0),
                cache_read_tokens = COALESCE((SELECT SUM(cache_read_tokens) FROM llm_calls WHERE run_id = ?1), 0),
                cache_write_tokens = COALESCE((SELECT SUM(cache_write_tokens) FROM llm_calls WHERE run_id = ?1), 0),
                total_cost_usd = COALESCE((SELECT SUM(total_cost_usd) FROM llm_calls WHERE run_id = ?1), 0)
             WHERE run_id = ?1",
            params![run_id],
        )?;
        execute_cached(
            self.conn,
            "WITH bounds AS (
                SELECT
                    MIN(started_at_ns) AS started_at_ns,
                    MAX(COALESCE(ended_at_ns, started_at_ns)) AS ended_at_ns
                FROM (
                    SELECT started_at_ns, ended_at_ns FROM run_steps WHERE run_id = ?1
                    UNION ALL
                    SELECT started_at_ns, ended_at_ns FROM llm_calls WHERE run_id = ?1
                    UNION ALL
                    SELECT started_at_ns, ended_at_ns FROM tool_calls WHERE run_id = ?1
                )
             )
             UPDATE runs SET
                started_at_ns = COALESCE((SELECT started_at_ns FROM bounds), started_at_ns),
                started_day = COALESCE(date((SELECT started_at_ns FROM bounds) / 1000000000, 'unixepoch', 'localtime'), started_day),
                started_month = COALESCE(strftime('%Y-%m', (SELECT started_at_ns FROM bounds) / 1000000000, 'unixepoch', 'localtime'), started_month),
                ended_at_ns = COALESCE((SELECT ended_at_ns FROM bounds), ended_at_ns),
                duration_ns = CASE
                    WHEN (SELECT started_at_ns FROM bounds) IS NOT NULL
                     AND (SELECT ended_at_ns FROM bounds) IS NOT NULL
                    THEN MAX(0, (SELECT ended_at_ns FROM bounds) - (SELECT started_at_ns FROM bounds))
                    ELSE duration_ns
                END
             WHERE run_id = ?1",
            params![run_id],
        )?;
        execute_cached(
            self.conn,
            "UPDATE runs SET
                signal_count = (SELECT COUNT(*) FROM run_signals WHERE run_id = ?1),
                primary_signal = (
                    SELECT signal_type FROM run_signals
                    WHERE run_id = ?1
                    ORDER BY CASE severity WHEN 'high' THEN 3 WHEN 'medium' THEN 2 ELSE 1 END DESC,
                             created_at_ns DESC
                    LIMIT 1
                )
             WHERE run_id = ?1",
            params![run_id],
        )?;

        Ok(())
    }

    fn refresh_session_summary(&self, session_id: &str) -> Result<()> {
        execute_cached(
            self.conn,
            "UPDATE sessions SET
                run_count = (SELECT COUNT(*) FROM runs WHERE session_id = ?1),
                turn_count = (SELECT COUNT(*) FROM turns WHERE session_id = ?1),
                first_seen_ns = MIN(
                    first_seen_ns,
                    COALESCE((SELECT MIN(started_at_ns) FROM runs WHERE session_id = ?1), first_seen_ns)
                ),
                last_seen_ns = MAX(
                    last_seen_ns,
                    COALESCE((SELECT MAX(COALESCE(ended_at_ns, started_at_ns)) FROM runs WHERE session_id = ?1), last_seen_ns)
                )
             WHERE session_id = ?1",
            params![session_id],
        )?;
        Ok(())
    }

    fn upsert_llm_signals(
        &self,
        event: &NormalizedEvent,
        run_id: &str,
        llm_call_id: &str,
    ) -> Result<()> {
        if let Some(context_window_percent) = event.usage.context_window_percent {
            let severity = if context_window_percent >= 0.80 {
                Some("high")
            } else if context_window_percent >= 0.65 {
                Some("medium")
            } else {
                None
            };

            if let Some(severity) = severity {
                self.upsert_signal(
                    run_id,
                    &event.source,
                    "context_bloat",
                    severity,
                    "Context is near the model window",
                    &json!({
                        "llm_call_id": llm_call_id,
                        "context_window_percent": context_window_percent,
                        "model_context_window": event.usage.model_context_window,
                        "input_tokens": event.usage.input_tokens
                    }),
                    "Start a fresh run or summarize context before continuing.",
                )?;
            }
        }

        if let Some(cache_ratio) =
            cache_ratio(event.usage.input_tokens, event.usage.cache_read_tokens)
        {
            let severity = if event.usage.input_tokens >= 50_000 && cache_ratio < 0.20 {
                Some("high")
            } else if event.usage.input_tokens >= 20_000 && cache_ratio < 0.30 {
                Some("medium")
            } else {
                None
            };

            if let Some(severity) = severity {
                self.upsert_signal(
                    run_id,
                    &event.source,
                    "low_cache_reuse",
                    severity,
                    "Large prompt with low cache reuse",
                    &json!({
                        "llm_call_id": llm_call_id,
                        "input_tokens": event.usage.input_tokens,
                        "cache_read_tokens": event.usage.cache_read_tokens,
                        "cache_ratio": cache_ratio
                    }),
                    "Check whether project context or prompts are changing too often.",
                )?;
            }
        }

        let reasoning_ratio = if event.usage.output_tokens > 0 {
            Some(event.usage.reasoning_tokens as f64 / event.usage.output_tokens as f64)
        } else {
            None
        };
        if let Some(reasoning_ratio) = reasoning_ratio {
            let severity = if event.usage.reasoning_tokens >= 15_000 && reasoning_ratio >= 3.0 {
                Some("high")
            } else if event.usage.reasoning_tokens >= 5_000 && reasoning_ratio >= 2.0 {
                Some("medium")
            } else {
                None
            };

            if let Some(severity) = severity {
                self.upsert_signal(
                    run_id,
                    &event.source,
                    "reasoning_spike",
                    severity,
                    "Reasoning tokens are high relative to output",
                    &json!({
                        "llm_call_id": llm_call_id,
                        "reasoning_tokens": event.usage.reasoning_tokens,
                        "output_tokens": event.usage.output_tokens,
                        "reasoning_output_ratio": reasoning_ratio
                    }),
                    "Use lower reasoning effort for routine work or split the task.",
                )?;
            }
        }

        Ok(())
    }

    fn upsert_tool_signals(
        &self,
        event: &NormalizedEvent,
        run_id: &str,
        tool_call_id: &str,
    ) -> Result<()> {
        if event.operation.status != OperationStatus::Failed {
            return Ok(());
        }

        self.upsert_signal(
            run_id,
            &event.source,
            "tool_failure",
            "medium",
            "Tool call failed",
            &json!({
                "tool_call_id": tool_call_id,
                "tool_name": event.operation.name,
                "error_type": event.operation.error_type,
            }),
            "Review the tool input and retry pattern for this run.",
        )
    }

    fn upsert_signal(
        &self,
        run_id: &str,
        source: &str,
        signal_type: &str,
        severity: &str,
        title: &str,
        evidence: &Value,
        suggestion: &str,
    ) -> Result<()> {
        let signal_id = format!(
            "{source}:signal:{}",
            stable_hash(&format!("{run_id}:{signal_type}:{evidence}"))
        );
        let evidence_json = serde_json::to_string(evidence)?;

        execute_cached(
            self.conn,
            "INSERT INTO run_signals (
                signal_id, run_id, source, signal_type, severity, title,
                evidence_json, suggestion, created_at_ns
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(signal_id) DO UPDATE SET
                severity = excluded.severity,
                title = excluded.title,
                evidence_json = excluded.evidence_json,
                suggestion = excluded.suggestion",
            params![
                signal_id,
                run_id,
                source,
                signal_type,
                severity,
                title,
                evidence_json,
                suggestion,
                now_ns()
            ],
        )?;

        Ok(())
    }
}

fn execute_cached<P>(conn: &Connection, sql: &str, params: P) -> Result<usize>
where
    P: Params,
{
    let mut statement = conn.prepare_cached(sql)?;
    statement.execute(params).map_err(Into::into)
}

fn metadata_json(event: &NormalizedEvent) -> Result<Option<String>> {
    let mut metadata = match event.operation.metadata.as_ref() {
        Some(Value::Object(object)) => object.clone(),
        Some(value) => {
            let mut object = Map::new();
            object.insert("operation_metadata".to_string(), value.clone());
            object
        }
        None => Map::new(),
    };

    metadata.insert("source_kind".to_string(), json!(event.source_kind));
    metadata.insert("observed_at_ns".to_string(), json!(event.observed_at_ns));
    metadata.insert("confidence".to_string(), json!(event.confidence));

    if let Some(parent_span_id) = event.trace.parent_span_id.as_ref() {
        metadata.insert("parent_span_id".to_string(), json!(parent_span_id));
    }

    if metadata.is_empty() {
        return Ok(None);
    }

    serde_json::to_string(&metadata)
        .map(Some)
        .map_err(Into::into)
}

fn metadata_i64(event: &NormalizedEvent, key: &str) -> Option<i64> {
    event
        .operation
        .metadata
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|metadata| metadata.get(key))
        .and_then(Value::as_i64)
}

fn cache_ratio(input_tokens: i64, cache_read_tokens: i64) -> Option<f64> {
    if input_tokens <= 0 {
        return None;
    }

    Some(cache_read_tokens as f64 / input_tokens as f64)
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use anyhow::Result;

    use crate::{
        db::Database,
        projection::event::{
            EntityHint, NormalizedEvent, Operation, OperationStatus, OperationType, TraceContext,
            TurnHint, Usage,
        },
    };

    use super::Projector;

    #[test]
    fn projects_llm_event_idempotently() -> Result<()> {
        let db_path = temp_db_path("projects_llm_event_idempotently");
        let db = Database::open(&db_path)?;
        db.migrate()?;

        let event = NormalizedEvent {
            source: "kanade".to_string(),
            source_kind: "otlp_jsonl_file".to_string(),
            source_event_id: Some("span-1".to_string()),
            observed_at_ns: 100,
            occurred_at_ns: 100,
            source_ref: Some("trace.jsonl:1".to_string()),
            cwd: Some("/tmp/project".to_string()),
            project_id: Some("project-a".to_string()),
            trace: TraceContext {
                trace_id: Some("trace-1".to_string()),
                span_id: Some("span-1".to_string()),
                parent_span_id: None,
                root_span_id: Some("root-1".to_string()),
            },
            session: Some(EntityHint {
                external_id: "session-1".to_string(),
                kind: "workflow".to_string(),
                title: Some("Import test".to_string()),
            }),
            run: EntityHint {
                external_id: "run-1".to_string(),
                kind: "workflow".to_string(),
                title: Some("Run test".to_string()),
            },
            turn: Some(TurnHint {
                external_id: "turn-1".to_string(),
                index: Some(0),
                role: Some("user".to_string()),
            }),
            operation: Operation {
                operation_type: OperationType::LlmCall,
                name: "chat.completions".to_string(),
                status: OperationStatus::Success,
                error_type: None,
                started_at_ns: 100,
                ended_at_ns: Some(200),
                duration_ns: Some(100),
                order_index: Some(0),
                metadata: None,
            },
            usage: Usage {
                provider: Some("openai".to_string()),
                model: Some("gpt-test".to_string()),
                input_tokens: 180000,
                output_tokens: 200,
                reasoning_tokens: 50,
                uncached_input_tokens: 175000,
                cache_read_tokens: 5000,
                cache_write_tokens: 0,
                total_cost_usd: 0.01,
                pricing_status: Some("estimated".to_string()),
                cost_confidence: Some("estimated".to_string()),
                model_context_window: Some(200000),
                context_window_percent: Some(0.9),
            },
            skill_events: Vec::new(),
            confidence: 1.0,
        };

        let projector = Projector::new(&db);
        let first = projector.project(&event)?;
        let second = projector.project(&event)?;

        assert_eq!(first.run_id, "kanade:run-1");
        assert_eq!(first.run_id, second.run_id);
        assert_eq!(first.session_id.as_deref(), Some("kanade:session-1"));
        assert_eq!(first.turn_id.as_deref(), Some("kanade:turn-1"));
        assert!(first.step_id.starts_with("kanade:step:"));
        assert!(
            first
                .llm_call_id
                .as_deref()
                .is_some_and(|id| id.starts_with("kanade:llm:"))
        );
        assert!(first.tool_call_id.is_none());
        assert_eq!(count(&db, "runs")?, 1);
        assert_eq!(count(&db, "sessions")?, 1);
        assert_eq!(count(&db, "turns")?, 1);
        assert_eq!(count(&db, "run_steps")?, 1);
        assert_eq!(count(&db, "llm_calls")?, 1);
        assert_eq!(count(&db, "run_signals")?, 2);

        let _ = fs::remove_file(db_path);
        Ok(())
    }

    #[test]
    fn operation_enums_map_to_storage_strings() {
        assert_eq!(OperationType::LlmCall.as_step_type(), "llm");
        assert_eq!(OperationType::ToolCall.as_step_type(), "tool");
        assert_eq!(OperationType::Workflow.as_step_type(), "workflow");
        assert_eq!(OperationType::User.as_step_type(), "user");
        assert_eq!(OperationType::System.as_step_type(), "system");
        assert_eq!(OperationType::Approval.as_step_type(), "approval");
        assert_eq!(OperationType::Human.as_step_type(), "human");
        assert_eq!(OperationType::Repair.as_step_type(), "repair");
        assert_eq!(OperationType::Phase.as_step_type(), "phase");
        assert_eq!(OperationType::Agent.as_step_type(), "agent");

        assert_eq!(OperationStatus::Running.as_str(), "running");
        assert_eq!(OperationStatus::Success.as_str(), "success");
        assert_eq!(OperationStatus::Failed.as_str(), "failed");
        assert_eq!(OperationStatus::Skipped.as_str(), "skipped");
        assert_eq!(OperationStatus::FromCache.as_str(), "from_cache");
    }

    #[test]
    fn source_capabilities_default_to_unknown_false() {
        let capabilities = super::super::event::SourceCapabilities::default();

        assert!(!capabilities.has_trace_context);
        assert!(!capabilities.has_session_id);
        assert!(!capabilities.has_run_boundary);
        assert!(!capabilities.has_turn_boundary);
        assert!(!capabilities.has_llm_usage);
        assert!(!capabilities.has_tool_events);
        assert!(!capabilities.has_cache_tokens);
        assert!(!capabilities.has_reasoning_tokens);
        assert!(!capabilities.has_cost);
        assert!(!capabilities.has_content_refs);
        assert!(!capabilities.has_project_context);
    }

    fn count(db: &Database, table: &str) -> Result<i64> {
        let sql = format!("SELECT COUNT(*) FROM {table}");
        Ok(db.connection().query_row(&sql, [], |row| row.get(0))?)
    }

    fn temp_db_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("shirabe-{name}-{}.sqlite", std::process::id()))
    }
}
