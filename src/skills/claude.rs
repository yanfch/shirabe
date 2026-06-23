use serde_json::{Map, Value};

use crate::projection::event::SkillEventHint;

use super::{attributed, invoked, string_field, valid_skill_name};

pub fn skill_events_from_tool_use(
    item: &Map<String, Value>,
    tool_name: &str,
) -> Vec<SkillEventHint> {
    if tool_name != "Skill" {
        return Vec::new();
    }

    let Some(input) = item.get("input").and_then(Value::as_object) else {
        return Vec::new();
    };
    let Some(skill_name) = string_field(input, "skill").or_else(|| string_field(input, "name"))
    else {
        return Vec::new();
    };
    if !valid_skill_name(skill_name) {
        return Vec::new();
    }

    vec![invoked(
        skill_name.to_string(),
        1.0,
        "claude_skill_tool_use",
    )]
}

pub fn attributed_skill_events(
    object: &Map<String, Value>,
    message: &Map<String, Value>,
) -> Vec<SkillEventHint> {
    let Some(skill_name) = skill_name_from_value(object.get("attributionSkill"))
        .or_else(|| skill_name_from_value(message.get("attributionSkill")))
    else {
        return Vec::new();
    };

    vec![attributed(skill_name, 1.0, "claude_attribution_skill")]
}

fn skill_name_from_value(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(value) if valid_skill_name(value) => Some(value.clone()),
        Value::Object(object) => {
            let value = string_field(object, "name").or_else(|| string_field(object, "skill"))?;
            valid_skill_name(value).then(|| value.to_string())
        }
        _ => None,
    }
}
