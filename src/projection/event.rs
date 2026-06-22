use serde_json::Value;

#[derive(Debug, Clone)]
pub struct NormalizedEvent {
    pub source: String,
    pub source_kind: String,
    pub source_event_id: Option<String>,
    pub observed_at_ns: i64,
    pub occurred_at_ns: i64,
    pub source_ref: Option<String>,
    pub cwd: Option<String>,
    pub project_id: Option<String>,
    pub trace: TraceContext,
    pub session: Option<EntityHint>,
    pub run: EntityHint,
    pub turn: Option<TurnHint>,
    pub operation: Operation,
    pub usage: Usage,
    pub confidence: f64,
}

#[derive(Debug, Clone, Default)]
pub struct TraceContext {
    pub trace_id: Option<String>,
    pub span_id: Option<String>,
    pub parent_span_id: Option<String>,
    pub root_span_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EntityHint {
    pub external_id: String,
    pub kind: String,
    pub title: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TurnHint {
    pub external_id: String,
    pub index: Option<i64>,
    pub role: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Operation {
    pub operation_type: OperationType,
    pub name: String,
    pub status: OperationStatus,
    pub error_type: Option<String>,
    pub started_at_ns: i64,
    pub ended_at_ns: Option<i64>,
    pub duration_ns: Option<i64>,
    pub order_index: Option<i64>,
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationType {
    LlmCall,
    ToolCall,
    Workflow,
    User,
    System,
    Approval,
    Human,
    Repair,
    Phase,
    Agent,
}

impl OperationType {
    pub fn as_step_type(self) -> &'static str {
        match self {
            Self::LlmCall => "llm",
            Self::ToolCall => "tool",
            Self::Workflow => "workflow",
            Self::User => "user",
            Self::System => "system",
            Self::Approval => "approval",
            Self::Human => "human",
            Self::Repair => "repair",
            Self::Phase => "phase",
            Self::Agent => "agent",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationStatus {
    Running,
    Success,
    Failed,
    Skipped,
    FromCache,
}

impl OperationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
            Self::FromCache => "from_cache",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_tokens: i64,
    pub uncached_input_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub total_cost_usd: f64,
    pub pricing_status: Option<String>,
    pub cost_confidence: Option<String>,
    pub model_context_window: Option<i64>,
    pub context_window_percent: Option<f64>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SourceCapabilities {
    pub has_trace_context: bool,
    pub has_session_id: bool,
    pub has_run_boundary: bool,
    pub has_turn_boundary: bool,
    pub has_llm_usage: bool,
    pub has_tool_events: bool,
    pub has_cache_tokens: bool,
    pub has_reasoning_tokens: bool,
    pub has_cost: bool,
    pub has_content_refs: bool,
    pub has_project_context: bool,
}
