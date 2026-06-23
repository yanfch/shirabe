use serde_json::{Map, Value};

use crate::projection::event::SkillEventHint;

use super::{
    collect_text_values, extract_skill_names_from_skill_md_paths,
    extract_skill_names_from_skill_tags, invoked, loaded,
};

pub fn skill_events_from_user_message(message: &Map<String, Value>) -> Vec<SkillEventHint> {
    let Some(content) = message.get("content") else {
        return Vec::new();
    };

    let mut fragments = Vec::new();
    collect_text_values(content, &mut fragments);
    fragments
        .iter()
        .flat_map(|fragment| extract_skill_names_from_skill_tags(fragment))
        .map(|name| invoked(name, 0.9, "pi_expanded_skill_tag"))
        .collect()
}

pub fn skill_events_from_tool_call(
    item: &Map<String, Value>,
    tool_name: &str,
) -> Vec<SkillEventHint> {
    if tool_name != "read" {
        return Vec::new();
    }

    let Some(arguments) = item.get("arguments") else {
        return Vec::new();
    };
    let text = match arguments {
        Value::String(value) => value.clone(),
        _ => arguments.to_string(),
    };
    if !text.contains("SKILL.md") {
        return Vec::new();
    }

    extract_skill_names_from_skill_md_paths(&text)
        .into_iter()
        .map(|name| loaded(name, 0.75, "pi_read_tool_arguments"))
        .collect()
}
