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
  model_summaries: ModelSummary[];
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
  started_at_ns: number;
  ended_at_ns: number | null;
  order_index: number | null;
  input_tokens: number;
  output_tokens: number;
  cost_usd: number;
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
};

export type ToolCall = {
  tool_call_id: string;
  tool_name: string;
  status: string;
  error_type: string | null;
};
