export type Overview = {
  status: string;
  totals: Totals;
  imports: ImportSource[];
  top_signals: SignalSummary[];
  source_usage: SourceUsageSummary[];
  recent_sessions: SessionSummary[];
  recent_runs: Run[];
  tool_summaries: ToolSummary[];
  tool_failures: ToolFailureSummary[];
  skill_summaries: SkillSummary[];
  model_summaries: ModelSummary[];
};

export type Usage = {
  status: string;
  filters: UsageFilters;
  usage_grain: "day" | "month";
  source_options: string[];
  model_options: string[];
  summary: UsageSummary;
  buckets: UsageBucketSummary[];
  source_usage: SourceUsageSummary[];
  recent_sessions: SessionSummary[];
  recent_runs: Run[];
  tool_summaries: ToolSummary[];
  tool_failures: ToolFailureSummary[];
  skill_summaries: SkillSummary[];
  model_summaries: ModelSummary[];
};

export type MenubarUsage = {
  status: string;
  range: "today" | "7d" | "30d" | string;
  summary: UsageSummary;
  trend: UsageBucketSummary[];
  source_usage: SourceUsageSummary[];
  recent_runs: Run[];
  latest_run: Run | null;
  last_updated_at_ns: number | null;
  sync: SyncStatus;
};

export type UsageGrain = "auto" | "day" | "month";

export type UsageFilters = {
  preset: string;
  range: string;
  from: string | null;
  to: string | null;
  grain: UsageGrain;
  source: string | null;
  model: string | null;
};

export type SyncStatus = {
  status: string;
  state: string;
  phase: string;
  started_at_ns: number | null;
  finished_at_ns: number | null;
  duration_ms: number | null;
  sources: SyncSourceReport[];
  rollup: SyncRollupReport | null;
  error: string | null;
};

export type SyncSourceReport = {
  source: string;
  status: string;
  files_seen: number;
  files_imported: number;
  files_skipped: number;
  events_projected: number;
  source_bytes_scanned: number;
  elapsed_ms: number;
  error: string | null;
};

export type SyncRollupReport = {
  status: string;
  source_rollups: number;
  model_rollups: number;
  tool_rollups: number;
  tool_model_rollups: number;
  elapsed_ms: number;
};

export type Totals = {
  import_sources: number;
  import_files: number;
  imported_files: number;
  partial_files: number;
  failed_files: number;
  sessions: number;
  runs: number;
  turns: number;
  run_steps: number;
  llm_calls: number;
  tool_calls: number;
  failed_tool_calls: number;
  signals: number;
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
  total_cost_usd: number;
};

export type UsageSummary = {
  sessions: number;
  runs: number;
  turns: number;
  llm_calls: number;
  tool_calls: number;
  failed_tool_calls: number;
  input_tokens: number;
  output_tokens: number;
  total_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
  cache_hit_rate: number | null;
  tool_failure_rate: number | null;
  total_cost_usd: number;
};

export type UsageBucketSummary = {
  bucket_key: string;
  date: string;
  bucket_count: number;
  sessions: number;
  runs: number;
  turns: number;
  llm_calls: number;
  tool_calls: number;
  failed_tool_calls: number;
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
  total_cost_usd: number;
};

export type SourceUsageSummary = {
  source: string;
  sessions: number;
  runs: number;
  turns: number;
  llm_calls: number;
  tool_calls: number;
  failed_tool_calls: number;
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
  total_cost_usd: number;
};

export type SessionSummary = {
  session_id: string;
  source: string;
  kind: string;
  title: string | null;
  first_seen_ns: number;
  last_seen_ns: number;
  runs: number;
  turns: number;
  llm_calls: number;
  tool_calls: number;
  failed_tool_calls: number;
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
  total_cost_usd: number;
  models: string[];
};

export type ToolSummary = {
  tool_name: string;
  calls: number;
  success_calls: number;
  failed_calls: number;
  failure_rate: number;
  last_called_at_ns: number;
};

export type ToolFailureSummary = {
  source: string;
  tool_name: string;
  calls: number;
  success_calls: number;
  failed_calls: number;
  failure_rate: number;
};

export type ModelSummary = {
  model: string;
  calls: number;
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  total_cost_usd: number;
};

export type SkillSummary = {
  source: string;
  skill_name: string;
  loaded_count: number;
  invoked_count: number;
  attributed_count: number;
  sessions: number;
  runs: number;
  last_used_at_ns: number;
  confidence: number;
};

export type ImportSource = {
  source_id: string;
  source: string;
  source_kind: string;
  root_path: string;
  file_count: number;
  event_count: number;
  warning_count: number;
  source_bytes: number;
};

export type SignalSummary = {
  signal_type: string;
  severity: string;
  count: number;
};

export type Run = {
  run_id: string;
  source: string;
  kind: string;
  title: string | null;
  status: string;
  session_id: string | null;
  started_at_ns: number;
  ended_at_ns: number | null;
  duration_ns: number | null;
  llm_call_count: number;
  tool_call_count: number;
  failed_tool_count: number;
  signal_count: number;
  primary_signal: string | null;
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  total_cost_usd: number;
};

export type RunDetail = {
  status: string;
  run: Run;
  signals: RunSignal[];
  timeline: RunStep[];
  llm_calls: LlmCall[];
  tool_calls: ToolCall[];
};

export type SessionList = {
  status: string;
  sessions: SessionSummary[];
};

export type SessionDetail = {
  status: string;
  session: SessionSummary;
  runs: Run[];
  turns: TurnSummary[];
  timeline: SessionTimelineEvent[];
  llm_calls: LlmCall[];
  tool_calls: ToolCall[];
  skill_events: SkillEventRecord[];
  tool_summaries: ToolSummary[];
  skill_summaries: SkillSummary[];
  model_summaries: ModelSummary[];
};

export type TurnSummary = {
  turn_id: string;
  run_id: string;
  session_id: string | null;
  source: string;
  turn_index: number | null;
  role: string | null;
  status: string | null;
  started_at_ns: number;
  ended_at_ns: number | null;
  duration_ns: number | null;
  llm_calls: number;
  tool_calls: number;
  failed_tool_calls: number;
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  total_cost_usd: number;
  models: string[];
};

export type SkillEventRecord = {
  skill_event_id: string;
  source: string;
  skill_name: string;
  event_type: string;
  confidence: number;
  session_id: string | null;
  run_id: string;
  turn_id: string | null;
  source_event_id: string | null;
  source_ref: string | null;
  occurred_at_ns: number;
  metadata_json: string | null;
};

export type SessionTimelineEvent = {
  event_id: string;
  event_type: "llm" | "tool" | "skill" | string;
  run_id: string;
  turn_id: string | null;
  label: string;
  status: string;
  error_type: string | null;
  started_at_ns: number;
  ended_at_ns: number | null;
  duration_ns: number | null;
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  total_cost_usd: number;
  model: string | null;
  tool_name: string | null;
  skill_name: string | null;
  skill_event_type: string | null;
};

export type RunSignal = {
  signal_id: string;
  signal_type: string;
  severity: string;
  title: string;
  evidence_json: string | null;
  suggestion: string | null;
};

export type RunStep = {
  step_id: string;
  step_type: string;
  name: string;
  status: string;
  error_type: string | null;
  source_event_id: string | null;
  source_ref: string | null;
  started_at_ns: number;
  ended_at_ns: number | null;
  duration_ns: number | null;
  order_index: number | null;
  llm_call_id: string | null;
  tool_call_id: string | null;
  input_tokens: number;
  output_tokens: number;
  cost_usd: number;
  metadata_json: string | null;
};

export type LlmCall = {
  llm_call_id: string;
  model: string | null;
  operation: string | null;
  status: string;
  input_tokens: number;
  output_tokens: number;
  reasoning_tokens: number;
  cache_read_tokens: number;
  cache_ratio: number | null;
  context_window_percent: number | null;
  total_cost_usd: number;
  started_at_ns: number;
  ended_at_ns: number | null;
  duration_ns: number | null;
};

export type ToolCall = {
  tool_call_id: string;
  tool_name: string;
  status: string;
  error_type: string | null;
};
