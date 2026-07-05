use crate::db::stable_hash;

pub fn session_id(profile_id: &str, source: &str, external_id: &str) -> String {
    scoped_id(profile_id, source, external_id)
}

pub fn run_id(profile_id: &str, source: &str, external_id: &str) -> String {
    scoped_id(profile_id, source, external_id)
}

pub fn turn_id(profile_id: &str, source: &str, external_id: &str) -> String {
    scoped_id(profile_id, source, external_id)
}

pub fn event_scoped_id(
    profile_id: &str,
    source: &str,
    kind: &str,
    source_event_id: Option<&str>,
    fallback: &str,
) -> String {
    let stable_part = source_event_id.unwrap_or(fallback);
    if profile_id == "local" {
        format!("{source}:{kind}:{}", stable_hash(stable_part))
    } else {
        format!("{profile_id}:{source}:{kind}:{}", stable_hash(stable_part))
    }
}

fn scoped_id(profile_id: &str, source: &str, external_id: &str) -> String {
    if profile_id == "local" {
        format!("{source}:{external_id}")
    } else {
        format!("{profile_id}:{source}:{external_id}")
    }
}
