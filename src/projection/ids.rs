use crate::db::stable_hash;

pub fn session_id(source: &str, external_id: &str) -> String {
    format!("{source}:{external_id}")
}

pub fn run_id(source: &str, external_id: &str) -> String {
    format!("{source}:{external_id}")
}

pub fn turn_id(source: &str, external_id: &str) -> String {
    format!("{source}:{external_id}")
}

pub fn event_scoped_id(
    source: &str,
    kind: &str,
    source_event_id: Option<&str>,
    fallback: &str,
) -> String {
    let stable_part = source_event_id.unwrap_or(fallback);
    format!("{source}:{kind}:{}", stable_hash(stable_part))
}
