use anyhow::{Context, Result, bail};
use rusqlite::params;
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::db::{Database, now_ns, stable_hash};

pub const LITELLM_PRICING_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";

#[derive(Debug, Serialize)]
pub struct PricingRefreshReport {
    pub status: &'static str,
    pub source_url: String,
    pub pricing_source_id: String,
    pub fetched_at_ns: i64,
    pub model_count: usize,
    pub alias_count: usize,
    pub byte_count: usize,
}

pub async fn refresh_litellm_pricing(
    db: &Database,
    source_url: &str,
) -> Result<PricingRefreshReport> {
    let raw_json = reqwest::get(source_url)
        .await
        .with_context(|| format!("fetch pricing from {source_url}"))?
        .error_for_status()
        .with_context(|| format!("pricing source returned error for {source_url}"))?
        .text()
        .await
        .context("read pricing response")?;

    cache_litellm_pricing(db, source_url, &raw_json)
}

fn cache_litellm_pricing(
    db: &Database,
    source_url: &str,
    raw_json: &str,
) -> Result<PricingRefreshReport> {
    let value: Value = serde_json::from_str(raw_json).context("parse pricing json")?;
    let Value::Object(models) = value else {
        bail!("pricing json root must be an object");
    };

    let fetched_at_ns = now_ns();
    let pricing_source_id = format!("litellm:{}", stable_hash(source_url));
    let mut model_count = 0usize;
    let mut alias_count = 0usize;

    db.connection().execute(
        "INSERT INTO pricing_sources (
            pricing_source_id, source_url, fetched_at_ns, model_count, raw_json, metadata_json
         )
         VALUES (?1, ?2, ?3, 0, ?4, ?5)
         ON CONFLICT(pricing_source_id) DO UPDATE SET
            source_url = excluded.source_url,
            fetched_at_ns = excluded.fetched_at_ns,
            raw_json = excluded.raw_json,
            metadata_json = excluded.metadata_json",
        params![
            pricing_source_id,
            source_url,
            fetched_at_ns,
            raw_json,
            serde_json::to_string(&json!({ "format": "litellm_model_prices" }))?
        ],
    )?;

    db.connection().execute(
        "DELETE FROM model_prices WHERE source_id = ?1",
        params![pricing_source_id],
    )?;

    for (model_name, spec) in models {
        if model_name == "sample_spec" {
            continue;
        }
        let Value::Object(spec) = spec else {
            continue;
        };

        let provider = string_field(&spec, "litellm_provider");
        let input_cost_per_token = number_field(&spec, "input_cost_per_token");
        let output_cost_per_token = number_field(&spec, "output_cost_per_token");
        let cache_read_input_token_cost = number_field(&spec, "cache_read_input_token_cost");
        let cache_creation_input_token_cost =
            number_field(&spec, "cache_creation_input_token_cost");

        if input_cost_per_token.is_none()
            && output_cost_per_token.is_none()
            && cache_read_input_token_cost.is_none()
            && cache_creation_input_token_cost.is_none()
        {
            continue;
        }

        let metadata = compact_metadata(&spec);
        insert_model_price(
            db,
            &model_name,
            &pricing_source_id,
            provider.as_deref(),
            input_cost_per_token,
            output_cost_per_token,
            cache_read_input_token_cost,
            cache_creation_input_token_cost,
            fetched_at_ns,
            &metadata,
            true,
        )?;
        model_count += 1;

        for alias in model_aliases(&model_name) {
            if alias == model_name {
                continue;
            }
            if insert_model_price(
                db,
                &alias,
                &pricing_source_id,
                provider.as_deref(),
                input_cost_per_token,
                output_cost_per_token,
                cache_read_input_token_cost,
                cache_creation_input_token_cost,
                fetched_at_ns,
                &alias_metadata(&metadata, &model_name),
                false,
            )? {
                alias_count += 1;
            }
        }
    }

    db.connection().execute(
        "UPDATE pricing_sources SET model_count = ?2 WHERE pricing_source_id = ?1",
        params![pricing_source_id, model_count.min(i64::MAX as usize) as i64],
    )?;

    Ok(PricingRefreshReport {
        status: "ok",
        source_url: source_url.to_string(),
        pricing_source_id,
        fetched_at_ns,
        model_count,
        alias_count,
        byte_count: raw_json.len(),
    })
}

fn insert_model_price(
    db: &Database,
    model_name: &str,
    pricing_source_id: &str,
    provider: Option<&str>,
    input_cost_per_token: Option<f64>,
    output_cost_per_token: Option<f64>,
    cache_read_input_token_cost: Option<f64>,
    cache_creation_input_token_cost: Option<f64>,
    updated_at_ns: i64,
    metadata: &Value,
    replace_existing: bool,
) -> Result<bool> {
    let conflict = if replace_existing {
        "DO UPDATE SET
            source_id = excluded.source_id,
            provider = excluded.provider,
            input_cost_per_token = excluded.input_cost_per_token,
            output_cost_per_token = excluded.output_cost_per_token,
            cache_read_input_token_cost = excluded.cache_read_input_token_cost,
            cache_creation_input_token_cost = excluded.cache_creation_input_token_cost,
            updated_at_ns = excluded.updated_at_ns,
            metadata_json = excluded.metadata_json"
    } else {
        "DO NOTHING"
    };
    let sql = format!(
        "INSERT INTO model_prices (
            model_name, source_id, provider, input_cost_per_token,
            output_cost_per_token, cache_read_input_token_cost,
            cache_creation_input_token_cost, updated_at_ns, metadata_json
         )
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(model_name) {conflict}"
    );
    let changed = db.connection().execute(
        &sql,
        params![
            model_name,
            pricing_source_id,
            provider,
            input_cost_per_token,
            output_cost_per_token,
            cache_read_input_token_cost,
            cache_creation_input_token_cost,
            updated_at_ns,
            serde_json::to_string(metadata)?
        ],
    )?;
    Ok(changed > 0)
}

fn string_field(object: &Map<String, Value>, key: &str) -> Option<String> {
    object.get(key)?.as_str().map(str::to_string)
}

fn number_field(object: &Map<String, Value>, key: &str) -> Option<f64> {
    object.get(key)?.as_f64()
}

fn compact_metadata(spec: &Map<String, Value>) -> Value {
    json!({
        "mode": spec.get("mode"),
        "max_input_tokens": spec.get("max_input_tokens"),
        "max_output_tokens": spec.get("max_output_tokens"),
        "max_tokens": spec.get("max_tokens")
    })
}

fn alias_metadata(metadata: &Value, canonical_model: &str) -> Value {
    json!({
        "alias_for": canonical_model,
        "pricing": metadata
    })
}

fn model_aliases(model_name: &str) -> Vec<String> {
    let mut aliases = Vec::new();

    if let Some(last_segment) = model_name.rsplit('/').next()
        && last_segment != model_name
    {
        aliases.push(last_segment.to_string());
    }

    let base_name = model_name.rsplit('/').next().unwrap_or(model_name);
    if base_name.starts_with("claude-") {
        aliases.push(format!("{base_name}-thinking"));
    }
    if let Some(alias) = claude_dot_version_alias(base_name) {
        aliases.push(alias.clone());
        aliases.push(format!("{alias}-thinking"));
    }

    if model_name.ends_with("-codex") {
        aliases.push(format!("{model_name}-spark"));
    }

    dedupe(aliases)
}

fn claude_dot_version_alias(model_name: &str) -> Option<String> {
    if !model_name.starts_with("claude-") {
        return None;
    }

    let mut parts: Vec<&str> = model_name.split('-').collect();
    let minor = parts.pop()?;
    let major = parts.pop()?;
    if major.len() == 1
        && minor.len() == 1
        && major.chars().all(|value| value.is_ascii_digit())
        && minor.chars().all(|value| value.is_ascii_digit())
    {
        Some(format!("{}-{major}.{minor}", parts.join("-")))
    } else {
        None
    }
}

fn dedupe(values: Vec<String>) -> Vec<String> {
    let mut unique = Vec::new();
    for value in values {
        if !unique.contains(&value) {
            unique.push(value);
        }
    }
    unique
}

#[cfg(test)]
mod tests {
    use anyhow::Result;

    use crate::db::Database;

    use super::cache_litellm_pricing;

    #[test]
    fn caches_litellm_pricing() -> Result<()> {
        let db_path =
            std::env::temp_dir().join(format!("shirabe-pricing-{}.sqlite", std::process::id()));
        let db = Database::open(&db_path)?;
        db.migrate()?;

        let report = cache_litellm_pricing(
            &db,
            "https://example.test/pricing.json",
            r#"{
                "sample_spec": {},
                "gpt-test": {
                    "litellm_provider": "openai",
                    "input_cost_per_token": 0.000001,
                    "output_cost_per_token": 0.000002,
                    "cache_read_input_token_cost": 0.0000001,
                    "max_input_tokens": 128000
                }
            }"#,
        )?;

        assert_eq!(report.model_count, 1);
        assert_eq!(report.alias_count, 0);
        let count: i64 =
            db.connection()
                .query_row("SELECT COUNT(*) FROM model_prices", [], |row| row.get(0))?;
        assert_eq!(count, 1);

        let _ = std::fs::remove_file(db_path);
        Ok(())
    }

    #[test]
    fn caches_pricing_aliases() -> Result<()> {
        let db_path = std::env::temp_dir().join(format!(
            "shirabe-pricing-aliases-{}.sqlite",
            std::process::id()
        ));
        let db = Database::open(&db_path)?;
        db.migrate()?;

        let report = cache_litellm_pricing(
            &db,
            "https://example.test/pricing.json",
            r#"{
                "gpt-5.3-codex": {
                    "litellm_provider": "openai",
                    "input_cost_per_token": 0.000001,
                    "output_cost_per_token": 0.000002
                },
                "openrouter/xiaomi/mimo-v2.5-pro": {
                    "litellm_provider": "openrouter",
                    "input_cost_per_token": 0.000003,
                    "output_cost_per_token": 0.000004
                },
                "claude-sonnet-4-6": {
                    "litellm_provider": "anthropic",
                    "input_cost_per_token": 0.000005,
                    "output_cost_per_token": 0.000006
                }
            }"#,
        )?;

        assert_eq!(report.model_count, 3);
        assert!(report.alias_count >= 3);
        for model in [
            "gpt-5.3-codex-spark",
            "mimo-v2.5-pro",
            "claude-sonnet-4.6",
            "claude-sonnet-4.6-thinking",
        ] {
            let count: i64 = db.connection().query_row(
                "SELECT COUNT(*) FROM model_prices WHERE model_name = ?1",
                [model],
                |row| row.get(0),
            )?;
            assert_eq!(count, 1, "missing alias {model}");
        }

        let _ = std::fs::remove_file(db_path);
        Ok(())
    }
}
