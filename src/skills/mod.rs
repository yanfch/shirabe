use std::collections::HashSet;

use serde_json::{Value, json};

use crate::projection::event::{SkillEventHint, SkillEventType};

pub mod claude;
pub mod codex;
pub mod pi;

fn hint(
    skill_name: String,
    event_type: SkillEventType,
    confidence: f64,
    extractor: &'static str,
) -> SkillEventHint {
    SkillEventHint {
        skill_name,
        event_type,
        confidence,
        metadata: Some(json!({ "extractor": extractor })),
    }
}

fn loaded(skill_name: String, confidence: f64, extractor: &'static str) -> SkillEventHint {
    hint(skill_name, SkillEventType::Loaded, confidence, extractor)
}

fn invoked(skill_name: String, confidence: f64, extractor: &'static str) -> SkillEventHint {
    hint(skill_name, SkillEventType::Invoked, confidence, extractor)
}

fn attributed(skill_name: String, confidence: f64, extractor: &'static str) -> SkillEventHint {
    hint(
        skill_name,
        SkillEventType::Attributed,
        confidence,
        extractor,
    )
}

fn extract_skill_names_from_skill_md_paths(text: &str) -> Vec<String> {
    if !text.contains("SKILL.md") {
        return Vec::new();
    }

    let mut names = Vec::new();
    let mut seen = HashSet::new();
    let mut cursor = 0usize;
    while let Some(relative_index) = text[cursor..].find("SKILL.md") {
        let marker_index = cursor + relative_index;
        if let Some(name) = skill_name_before_marker(&text[..marker_index])
            && seen.insert(name.clone())
        {
            names.push(name);
        }
        cursor = marker_index.saturating_add("SKILL.md".len());
    }

    names
}

fn skill_name_before_marker(prefix: &str) -> Option<String> {
    let prefix = prefix.trim_end_matches(['/', '\\']);
    let candidate = prefix
        .rsplit(|ch: char| {
            matches!(
                ch,
                '/' | '\\' | '"' | '\'' | '`' | '<' | '>' | '=' | ':' | ',' | ';'
            ) || ch.is_whitespace()
        })
        .next()
        .unwrap_or_default();

    valid_skill_name(candidate).then(|| candidate.to_string())
}

fn extract_skill_names_from_skill_tags(text: &str) -> Vec<String> {
    if !text.contains("<skill") {
        return Vec::new();
    }

    let mut names = Vec::new();
    let mut seen = HashSet::new();
    let mut cursor = 0usize;
    while let Some(relative_index) = text[cursor..].find("<skill") {
        let tag_start = cursor + relative_index;
        let tag_end = text[tag_start..]
            .find('>')
            .map(|index| tag_start + index)
            .unwrap_or(text.len());
        let tag = &text[tag_start..tag_end];
        if let Some(name) = attr_value(tag, "name")
            && valid_skill_name(&name)
            && seen.insert(name.clone())
        {
            names.push(name);
        }
        cursor = tag_end.saturating_add(1);
    }

    names
}

fn attr_value(tag: &str, attr: &str) -> Option<String> {
    let needle = format!("{attr}=");
    let index = tag.find(&needle)? + needle.len();
    let rest = &tag[index..];
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let value = &rest[quote.len_utf8()..];
    let end = value.find(quote)?;
    Some(value[..end].to_string())
}

fn collect_text_values(value: &Value, values: &mut Vec<String>) {
    match value {
        Value::String(value) => values.push(value.clone()),
        Value::Array(items) => {
            for item in items {
                collect_text_values(item, values);
            }
        }
        Value::Object(object) => {
            for key in ["text", "content"] {
                if let Some(value) = object.get(key) {
                    collect_text_values(value, values);
                }
            }
        }
        _ => {}
    }
}

fn string_field<'a>(payload: &'a serde_json::Map<String, Value>, key: &str) -> Option<&'a str> {
    payload.get(key)?.as_str()
}

fn valid_skill_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value != "."
        && value != ".."
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ':' | '@'))
}

#[cfg(test)]
mod tests {
    use super::{extract_skill_names_from_skill_md_paths, extract_skill_names_from_skill_tags};

    #[test]
    fn extracts_skill_names_from_paths_without_wildcard_noise() {
        let names = extract_skill_names_from_skill_md_paths(
            r#"sed -n '1,80p' /Users/me/.codex/skills/.system/imagegen/SKILL.md && rg '*/SKILL.md'"#,
        );

        assert_eq!(names, vec!["imagegen"]);
    }

    #[test]
    fn extracts_expanded_skill_tags() {
        let names = extract_skill_names_from_skill_tags(
            r#"<skill name="fantuan-price-diff" location="/tmp/SKILL.md">body</skill>"#,
        );

        assert_eq!(names, vec!["fantuan-price-diff"]);
    }
}
