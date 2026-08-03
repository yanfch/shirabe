use std::collections::HashMap;

use anyhow::{Result, bail};
use serde::{Deserialize, Deserializer};

use crate::{
    db::stable_hash,
    projection::event::{
        EntityHint, NormalizedEvent, Operation, OperationStatus, OperationType, TraceContext, Usage,
    },
};

use super::ImportIdentity;

#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct ParsedThread {
    pub(crate) thread_id: String,
    pub(crate) updated_at_ns: i64,
    pub(crate) events: Vec<NormalizedEvent>,
    pub(crate) superseded_source_event_ids: Vec<String>,
    pub(crate) split_total_mismatch_count: usize,
    pub(crate) used_ledger: bool,
    pub(crate) aggregate_only_count: usize,
    pub(crate) malformed_ledger_count: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Thread {
    #[serde(deserialize_with = "narrow")]
    id: Option<String>,
    #[serde(default, deserialize_with = "narrow")]
    updated_at: Option<String>,
    #[serde(default)]
    messages: Vec<Message>,
    #[serde(default)]
    usage_ledger: LedgerInput,
}

#[derive(Deserialize)]
struct Message {
    #[serde(default)]
    role: Option<String>,
    #[serde(default, rename = "messageId", deserialize_with = "narrow")]
    message_id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default, deserialize_with = "narrow")]
    timestamp: Option<String>,
    #[serde(default)]
    usage: Option<CurrentUsage>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CurrentUsage {
    #[serde(default)]
    model: Option<String>,
    #[serde(default, deserialize_with = "narrow")]
    timestamp: Option<String>,
    #[serde(default)]
    input_tokens: Option<i64>,
    #[serde(default)]
    output_tokens: Option<i64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<i64>,
    #[serde(default)]
    cache_read_input_tokens: Option<i64>,
    #[serde(default)]
    total_input_tokens: Option<i64>,
    #[serde(default)]
    total_tokens: Option<i64>,
    #[serde(default)]
    max_input_tokens: Option<i64>,
}

#[derive(Deserialize)]
struct Ledger {
    events: Vec<LenientLedgerEvent>,
}

#[derive(Default, Deserialize)]
#[serde(untagged)]
enum LedgerInput {
    Ledger(Ledger),
    Null(()),
    Malformed(serde::de::IgnoredAny),
    #[default]
    Absent,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum LenientLedgerEvent {
    Event(LedgerEvent),
    Malformed(serde::de::IgnoredAny),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LedgerEvent {
    #[serde(default, deserialize_with = "narrow")]
    id: Option<String>,
    #[serde(default, deserialize_with = "narrow")]
    timestamp: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    tokens: Option<LedgerTokens>,
    #[serde(default, deserialize_with = "narrow")]
    to_message_id: Option<String>,
}

#[derive(Deserialize)]
struct LedgerTokens {
    #[serde(default)]
    input: Option<i64>,
    #[serde(default)]
    output: Option<i64>,
    #[serde(default)]
    total: Option<i64>,
}

fn narrow<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Option<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Scalar {
        String(String),
        Integer(u64),
        Other(serde::de::IgnoredAny),
    }
    Ok(match Scalar::deserialize(d)? {
        Scalar::String(value) => Some(value),
        Scalar::Integer(value) => Some(value.to_string()),
        Scalar::Other(_) => None,
    })
}

#[allow(dead_code)]
pub(crate) fn parse_thread(
    json: &str,
    source_kind: &str,
    identity: &ImportIdentity,
) -> Result<ParsedThread> {
    let thread: Thread = serde_json::from_str(json)?;
    let thread_id = thread.id.filter(|id| valid_thread_id(id));
    let Some(thread_id) = thread_id else {
        bail!("invalid Amp thread id")
    };
    let updated_at_ns = thread
        .updated_at
        .as_deref()
        .and_then(timestamp_ns)
        .unwrap_or_default();

    let mut diagnostics = Diagnostics::default();
    let mut message_events = Vec::new();
    let mut cache = HashMap::new();
    for (index, message) in thread.messages.iter().enumerate() {
        let Some(usage) = message
            .usage
            .as_ref()
            .filter(|_| message.role.as_deref() == Some("assistant"))
        else {
            continue;
        };
        if let Some(id) = message.message_id.as_ref() {
            let cache_write = usage.cache_creation_input_tokens.unwrap_or(0);
            let cache_read = usage.cache_read_input_tokens.unwrap_or(0);
            if cache_write >= 0 && cache_read >= 0 {
                cache.insert(id.clone(), (cache_write, cache_read));
            }
        }
        let Some(model) = usage
            .model
            .as_deref()
            .or(message.model.as_deref())
            .filter(|v| !v.is_empty())
        else {
            continue;
        };
        let Some(occurred) = usage
            .timestamp
            .as_deref()
            .or(message.timestamp.as_deref())
            .and_then(timestamp_ns)
        else {
            continue;
        };
        let Some(mapped) = map_current(usage, &mut diagnostics) else {
            continue;
        };
        let identity_fallback = message.message_id.is_none();
        let id = message
            .message_id
            .clone()
            .unwrap_or_else(|| stable_hash(&format!("{thread_id}:{occurred}:{index}:{model}")));
        message_events.push(make_event(
            identity,
            source_kind,
            &thread_id,
            "message",
            &id,
            occurred,
            model,
            mapped,
            index,
            identity_fallback,
        ));
    }

    let mut ledger_events = Vec::new();
    let mut malformed_ledger_count =
        usize::from(matches!(thread.usage_ledger, LedgerInput::Malformed(_)));
    if let LedgerInput::Ledger(ledger) = thread.usage_ledger {
        for (index, item) in ledger.events.iter().enumerate() {
            let LenientLedgerEvent::Event(item) = item else {
                malformed_ledger_count += 1;
                continue;
            };
            let Some(tokens) = item.tokens.as_ref() else {
                continue;
            };
            let values = [
                tokens.input.unwrap_or(0),
                tokens.output.unwrap_or(0),
                tokens.total.unwrap_or(0),
            ];
            if values.iter().any(|v| *v < 0) {
                continue;
            }
            let Some(model) = item.model.as_deref().filter(|v| !v.is_empty()) else {
                continue;
            };
            let Some(occurred) = item.timestamp.as_deref().and_then(timestamp_ns) else {
                continue;
            };
            let input = tokens
                .input
                .or_else(|| {
                    tokens
                        .total
                        .map(|t| t.saturating_sub(tokens.output.unwrap_or(0)))
                })
                .unwrap_or(0);
            let output = tokens.output.unwrap_or(0);
            let (cache_write, cache_read) = item
                .to_message_id
                .as_ref()
                .and_then(|id| cache.get(id))
                .copied()
                .unwrap_or_default();
            if input == 0 && output == 0 && cache_write == 0 && cache_read == 0 {
                continue;
            }
            let identity_fallback = item.id.is_none();
            let id = item
                .id
                .clone()
                .unwrap_or_else(|| stable_hash(&format!("{thread_id}:{occurred}:{index}:{model}")));
            ledger_events.push(make_event(
                identity,
                source_kind,
                &thread_id,
                "ledger",
                &id,
                occurred,
                model,
                Mapped {
                    input,
                    output,
                    cache_write,
                    cache_read,
                    context: None,
                    confidence: 0.9,
                    aggregate_fallback: false,
                },
                index,
                identity_fallback,
            ));
        }
    }
    let used_ledger = !ledger_events.is_empty();
    let superseded_source_event_ids = if used_ledger {
        message_events
            .iter()
            .filter_map(|e| e.source_event_id.clone())
            .collect()
    } else {
        Vec::new()
    };
    Ok(ParsedThread {
        thread_id,
        updated_at_ns,
        events: if used_ledger {
            ledger_events
        } else {
            message_events
        },
        superseded_source_event_ids,
        split_total_mismatch_count: diagnostics.mismatches,
        aggregate_only_count: diagnostics.aggregates,
        malformed_ledger_count,
        used_ledger,
    })
}

#[derive(Default)]
struct Diagnostics {
    mismatches: usize,
    aggregates: usize,
}
struct Mapped {
    input: i64,
    output: i64,
    cache_write: i64,
    cache_read: i64,
    context: Option<i64>,
    confidence: f64,
    aggregate_fallback: bool,
}

fn map_current(value: &CurrentUsage, diagnostics: &mut Diagnostics) -> Option<Mapped> {
    let supplied = [
        value.input_tokens,
        value.output_tokens,
        value.cache_creation_input_tokens,
        value.cache_read_input_tokens,
        value.total_input_tokens,
        value.total_tokens,
        value.max_input_tokens,
    ];
    if supplied.iter().flatten().any(|v| *v < 0) {
        return None;
    }
    let split_present = value.input_tokens.is_some()
        || value.cache_creation_input_tokens.is_some()
        || value.cache_read_input_tokens.is_some();
    let (input, cache_write, cache_read, confidence, aggregate_fallback) = if split_present {
        let values = (
            value.input_tokens.unwrap_or(0),
            value.cache_creation_input_tokens.unwrap_or(0),
            value.cache_read_input_tokens.unwrap_or(0),
        );
        if value
            .total_input_tokens
            .is_some_and(|total| total != values.0 + values.1 + values.2)
        {
            diagnostics.mismatches += 1;
        }
        (values.0, values.1, values.2, 0.9, false)
    } else {
        let aggregate = value.total_input_tokens.or(value.total_tokens).unwrap_or(0);
        if aggregate > 0 {
            diagnostics.aggregates += 1;
        }
        (aggregate, 0, 0, 0.6, aggregate > 0)
    };
    let output = value.output_tokens.unwrap_or(0);
    (input != 0 || output != 0 || cache_write != 0 || cache_read != 0).then_some(Mapped {
        input,
        output,
        cache_write,
        cache_read,
        context: value.max_input_tokens,
        confidence,
        aggregate_fallback,
    })
}

fn make_event(
    identity: &ImportIdentity,
    source_kind: &str,
    thread: &str,
    schema: &str,
    id: &str,
    occurred: i64,
    model: &str,
    mapped: Mapped,
    index: usize,
    identity_fallback: bool,
) -> NormalizedEvent {
    let external_id = format!("amp:{thread}:{schema}:{id}");
    NormalizedEvent {
        profile_id: identity.profile_id.clone(),
        device_id: identity.device_id.clone(),
        source: "amp".into(),
        source_kind: source_kind.into(),
        source_event_id: Some(external_id.clone()),
        observed_at_ns: occurred,
        occurred_at_ns: occurred,
        source_ref: None,
        cwd: None,
        project_id: None,
        trace: TraceContext::default(),
        session: Some(EntityHint {
            external_id: thread.into(),
            kind: "amp_thread".into(),
            title: None,
        }),
        run: EntityHint {
            external_id: thread.into(),
            kind: "amp_thread".into(),
            title: None,
        },
        turn: None,
        operation: Operation {
            operation_type: OperationType::LlmCall,
            name: "llm_call".into(),
            status: OperationStatus::Success,
            error_type: None,
            started_at_ns: occurred,
            ended_at_ns: Some(occurred),
            duration_ns: None,
            order_index: Some(index as i64),
            metadata: match (identity_fallback, mapped.aggregate_fallback) {
                (false, false) => None,
                (true, false) => Some(serde_json::json!({ "identity_fallback": true })),
                (false, true) => Some(serde_json::json!({ "aggregate_fallback": true })),
                (true, true) => Some(serde_json::json!({
                    "identity_fallback": true,
                    "aggregate_fallback": true
                })),
            },
        },
        usage: Usage {
            provider: provider(model).map(str::to_string),
            model: Some(model.into()),
            input_tokens: mapped.input,
            output_tokens: mapped.output,
            uncached_input_tokens: mapped.input,
            cache_read_tokens: mapped.cache_read,
            cache_write_tokens: mapped.cache_write,
            pricing_status: Some("estimated_from_model_prices".into()),
            cost_confidence: Some("estimated".into()),
            model_context_window: mapped.context,
            ..Usage::default()
        },
        skill_events: Vec::new(),
        confidence: if identity_fallback {
            mapped.confidence * 0.8
        } else {
            mapped.confidence
        },
    }
}

fn provider(model: &str) -> Option<&'static str> {
    let lower = model.to_ascii_lowercase();
    if lower.starts_with("claude") {
        Some("anthropic")
    } else if lower.starts_with("gpt")
        || lower.starts_with('o')
            && lower[1..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit())
    {
        Some("openai")
    } else {
        None
    }
}
fn valid_thread_id(id: &str) -> bool {
    id.starts_with("T-") && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}
fn timestamp_ns(value: &str) -> Option<i64> {
    value
        .parse::<u64>()
        .ok()
        .and_then(|v| i64::try_from(v).ok())
        .map(|v| v.saturating_mul(1_000_000))
        .or_else(|| parse_rfc3339_ns(value))
}
fn parse_rfc3339_ns(value: &str) -> Option<i64> {
    let date_time = value.strip_suffix('Z')?;
    let (date, time) = date_time.split_once('T')?;
    let mut d = date.split('-');
    let year = d.next()?.parse::<i64>().ok()?;
    let month = d.next()?.parse::<i64>().ok()?;
    let day = d.next()?.parse::<i64>().ok()?;
    let (clock, fraction) = time.split_once('.').unwrap_or((time, ""));
    let mut t = clock.split(':');
    let hour = t.next()?.parse::<i64>().ok()?;
    let minute = t.next()?.parse::<i64>().ok()?;
    let second = t.next()?.parse::<i64>().ok()?;
    let nanos = fraction
        .chars()
        .take(9)
        .collect::<String>()
        .parse::<i64>()
        .unwrap_or_default()
        * 10_i64.pow(9_u32.saturating_sub(fraction.len().min(9) as u32));
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mp = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let days = era * 146097 + (yoe * 365 + yoe / 4 - yoe / 100 + doy) - 719468;
    Some(
        days * 86_400_000_000_000
            + hour * 3_600_000_000_000
            + minute * 60_000_000_000
            + second * 1_000_000_000
            + nanos,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(json: &str) -> Result<ParsedThread> {
        parse_thread(json, "amp_thread_json", &ImportIdentity::new("p", "d"))
    }
    fn wrap(extra: &str, messages: &str) -> String {
        format!(
            r#"{{"id":"T-test-1","updatedAt":"2026-01-01T00:00:03Z","messages":{messages}{extra}}}"#
        )
    }

    #[test]
    fn current_exact_buckets_and_private_data_is_absent() {
        let private = "PRIVATE_SENTINEL";
        let json = wrap(
            "",
            &format!(
                r#"[{{"role":"assistant","messageId":"m-1","content":"{private}","thinking":{{"x":"{private}"}},"model":"fallback","usage":{{"model":"claude-sonnet","timestamp":1767225602000,"inputTokens":10,"outputTokens":5,"cacheCreationInputTokens":3,"cacheReadInputTokens":2,"totalInputTokens":15,"maxInputTokens":200000}}}}]"#
            ),
        );
        let parsed = parse(&json).unwrap();
        let event = &parsed.events[0];
        assert_eq!(
            (
                event.usage.input_tokens,
                event.usage.uncached_input_tokens,
                event.usage.output_tokens,
                event.usage.cache_write_tokens,
                event.usage.cache_read_tokens
            ),
            (10, 10, 5, 3, 2)
        );
        assert_eq!(event.usage.provider.as_deref(), Some("anthropic"));
        assert_eq!(event.usage.model_context_window, Some(200000));
        assert_eq!(
            event.source_event_id.as_deref(),
            Some("amp:T-test-1:message:m-1")
        );
        assert!(!serde_json::to_string(event).unwrap().contains(private));
        assert!(!format!("{parsed:?}").contains(private));
    }
    #[test]
    fn empty_and_malformed_ledger_fall_back() {
        for ledger in [
            r#","usageLedger":{"events":[]}"#,
            r#","usageLedger":"bad""#,
            r#","usageLedger":{"events":"bad"}"#,
        ] {
            let p=parse(&wrap(ledger,r#"[{"role":"assistant","messageId":1,"model":"gpt-4","timestamp":"2026-01-01T00:00:00Z","usage":{"inputTokens":1}}]"#)).unwrap();
            assert_eq!(p.events.len(), 1);
            assert!(!p.used_ledger);
        }
    }
    #[test]
    fn malformed_whole_ledger_is_diagnosed_without_retaining_private_data() {
        let private = "PRIVATE_LEDGER_SENTINEL";
        let p = parse(&wrap(
            &format!(r#","usageLedger":{{"private":"{private}"}}"#),
            r#"[{"role":"assistant","messageId":"m","model":"x","timestamp":1,"usage":{"inputTokens":1}}]"#,
        ))
        .unwrap();
        assert_eq!(p.malformed_ledger_count, 1);
        assert!(!p.used_ledger);
        assert_eq!(p.events.len(), 1);
        assert!(!format!("{p:?}").contains(private));
    }
    #[test]
    fn ledger_joins_cache_suppresses_and_canonicalizes_ids() {
        let p=parse(&wrap(r#", "usageLedger":{"events":[{"id":9,"timestamp":"1767225603000","model":"claude-x","tokens":{"input":12,"output":4,"total":16},"toMessageId":"7"}]}"#,r#"[{"role":"assistant","messageId":7,"usage":{"cacheCreationInputTokens":1,"cacheReadInputTokens":2}},{"role":"assistant","messageId":8,"model":"claude-x","timestamp":1767225602000,"usage":{"inputTokens":10}}]"#)).unwrap();
        assert!(p.used_ledger);
        assert_eq!(p.events.len(), 1);
        assert_eq!(
            p.events[0].source_event_id.as_deref(),
            Some("amp:T-test-1:ledger:9")
        );
        assert_eq!(
            (
                p.events[0].usage.cache_write_tokens,
                p.events[0].usage.cache_read_tokens
            ),
            (1, 2)
        );
        assert_eq!(
            p.superseded_source_event_ids,
            vec!["amp:T-test-1:message:8"]
        );
    }
    #[test]
    fn mismatch_and_aggregate_confidence() {
        let p=parse(&wrap("",r#"[{"role":"assistant","model":"x","timestamp":1,"usage":{"inputTokens":2,"cacheReadInputTokens":3,"totalInputTokens":99}},{"role":"assistant","model":"x","timestamp":2,"usage":{"totalTokens":8}}]"#)).unwrap();
        assert_eq!(p.split_total_mismatch_count, 1);
        assert_eq!(p.aggregate_only_count, 1);
        assert_eq!(p.events[0].usage.input_tokens, 2);
        assert_eq!(
            p.events[0].operation.metadata,
            Some(serde_json::json!({"identity_fallback": true}))
        );
        assert_eq!(
            p.events[1].operation.metadata,
            Some(serde_json::json!({
                "aggregate_fallback": true,
                "identity_fallback": true
            }))
        );
        assert!(p.events[1].confidence < p.events[0].confidence);
    }
    #[test]
    fn aggregate_fallback_with_explicit_identity_is_bounded_metadata() {
        let p = parse(&wrap(
            "",
            r#"[{"role":"assistant","messageId":"m","model":"x","timestamp":1,"usage":{"totalInputTokens":8}}]"#,
        ))
        .unwrap();
        assert_eq!(
            p.events[0].operation.metadata,
            Some(serde_json::json!({"aggregate_fallback": true}))
        );
        assert_eq!(p.events[0].confidence, 0.6);
    }
    #[test]
    fn skips_zero_negative_and_non_assistant() {
        let p=parse(&wrap("",r#"[{"role":"assistant","model":"x","timestamp":1,"usage":{"inputTokens":0}},{"role":"assistant","model":"x","timestamp":1,"usage":{"inputTokens":-1}},{"role":"user","model":"x","timestamp":1,"usage":{"inputTokens":1}}]"#)).unwrap();
        assert!(p.events.is_empty());
    }
    #[test]
    fn missing_id_fallback_is_deterministic() {
        let json = wrap(
            "",
            r#"[{"role":"assistant","model":"x","timestamp":1,"usage":{"inputTokens":1}}]"#,
        );
        let fallback = parse(&json).unwrap();
        let explicit = parse(&wrap("", r#"[{"role":"assistant","messageId":"m","model":"x","timestamp":1,"usage":{"inputTokens":1}}]"#)).unwrap();
        assert_eq!(
            fallback.events[0].source_event_id,
            parse(&json).unwrap().events[0].source_event_id
        );
        assert_eq!(
            fallback.events[0].operation.metadata,
            Some(serde_json::json!({"identity_fallback": true}))
        );
        assert!(fallback.events[0].confidence < explicit.events[0].confidence);
        assert_eq!(explicit.events[0].operation.metadata, None);
    }
    #[test]
    fn numeric_and_string_top_level_message_ids_are_identical() {
        let numeric = parse(&wrap("", r#"[{"role":"assistant","messageId":7,"model":"x","timestamp":1,"usage":{"inputTokens":1}}]"#)).unwrap();
        let string = parse(&wrap("", r#"[{"role":"assistant","messageId":"7","model":"x","timestamp":1,"usage":{"inputTokens":1}}]"#)).unwrap();
        assert_eq!(
            numeric.events[0].source_event_id,
            string.events[0].source_event_id
        );
        assert_eq!(
            numeric.events[0].source_event_id.as_deref(),
            Some("amp:T-test-1:message:7")
        );
    }
    #[test]
    fn missing_ledger_id_fallback_is_deterministic() {
        let json = wrap(
            r#", "usageLedger":{"events":[{"timestamp":2,"model":"x","tokens":{"input":1}}]}"#,
            "[]",
        );
        let fallback = parse(&json).unwrap();
        let explicit = parse(&wrap(r#", "usageLedger":{"events":[{"id":"l","timestamp":2,"model":"x","tokens":{"input":1}}]}"#, "[]")).unwrap();
        assert_eq!(
            fallback.events[0].source_event_id,
            parse(&json).unwrap().events[0].source_event_id
        );
        assert_eq!(
            fallback.events[0].operation.metadata,
            Some(serde_json::json!({"identity_fallback": true}))
        );
        assert!(fallback.events[0].confidence < explicit.events[0].confidence);
        assert_eq!(explicit.events[0].operation.metadata, None);
    }
    #[test]
    fn ledger_skips_negative_and_all_zero_records() {
        let p = parse(&wrap(r#", "usageLedger":{"events":[{"id":"negative","timestamp":1,"model":"x","tokens":{"input":-1}},{"id":"zero","timestamp":1,"model":"x","tokens":{"input":0,"output":0,"total":0}},{"id":"usable","timestamp":1,"model":"x","tokens":{"input":2}}]}"#, "[]")).unwrap();
        assert_eq!(p.events.len(), 1);
        assert_eq!(
            p.events[0].source_event_id.as_deref(),
            Some("amp:T-test-1:ledger:usable")
        );
    }
    #[test]
    fn malformed_ledger_element_does_not_hide_usable_events() {
        let private = "PRIVATE_ELEMENT_SENTINEL";
        let p = parse(&wrap(&format!(r#", "usageLedger":{{"events":[{{"timestamp":{{"private":"{private}"}},"tokens":{{"input":"bad"}}}}, {{"id":"usable","timestamp":1,"model":"x","tokens":{{"input":2}}}}]}}"#), "[]")).unwrap();
        assert!(p.used_ledger);
        assert_eq!(p.malformed_ledger_count, 1);
        assert_eq!(p.events.len(), 1);
        assert_eq!(
            p.events[0].source_event_id.as_deref(),
            Some("amp:T-test-1:ledger:usable")
        );
        assert!(!format!("{p:?}").contains(private));
    }
    #[test]
    fn rejects_invalid_thread_id_without_leaking_nested_private_values() {
        assert!(parse(r#"{"id":"bad/private","messages":[]}"#).is_err());
        let private = "PRIVATE_SENTINEL";
        let json = format!(
            r#"{{"id":"T-safe","updatedAt":{{"x":"{private}"}},"messages":[{{"role":"assistant","messageId":{{"x":"{private}"}},"model":"x","timestamp":1,"usage":{{"inputTokens":1}}}}],"usageLedger":{{"events":[{{"id":1,"timestamp":2,"model":"x","tokens":{{"input":1}},"toMessageId":{{"x":"{private}"}}}}]}}}}"#
        );
        let result = parse(&json);
        assert!(!format!("{result:?}").contains(private));
        assert_eq!(result.unwrap().events.len(), 1);
    }
}
