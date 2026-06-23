use serde_json::{Map, Value};

use crate::projection::event::SkillEventHint;

use super::{extract_skill_names_from_skill_md_paths, loaded};

pub fn skill_events_from_function_call(payload: &Map<String, Value>) -> Vec<SkillEventHint> {
    let Some(arguments) = payload.get("arguments").and_then(Value::as_str) else {
        return Vec::new();
    };
    if !arguments.contains("SKILL.md") {
        return Vec::new();
    }

    extract_skill_names_from_skill_md_paths(arguments)
        .into_iter()
        .map(|name| loaded(name, 0.75, "codex_function_call_arguments"))
        .collect()
}
