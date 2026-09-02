use serde_json::{Map, Value, json};

pub const MANAGED_CATALOG_RELATIVE_PATH: &str = "model-catalogs/codex-switch-models.json";

/// Returns a merged Codex model catalog for the supported relay model families.
///
/// Model IDs returned by relay `/models` are not enough for Codex to determine context,
/// reasoning, and tool capabilities. Keep this deliberately scoped to families that the
/// application supports rather than guessing for every arbitrary relay model.
pub fn merge_supported_model(
    existing: Option<&[u8]>,
    model: &str,
) -> Result<Option<Vec<u8>>, String> {
    let Some(entry) = supported_entry(model) else {
        return Ok(None);
    };

    let mut root = match existing {
        Some(bytes) => serde_json::from_slice::<Value>(bytes)
            .map_err(|error| format!("managed model catalog is invalid JSON: {error}"))?,
        None => json!({ "models": [] }),
    };
    let object = root
        .as_object_mut()
        .ok_or_else(|| "managed model catalog root must be an object".to_owned())?;
    let models = object
        .entry("models")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| "managed model catalog models must be an array".to_owned())?;

    let slug = entry["slug"]
        .as_str()
        .expect("supported catalog entries always contain a slug");
    if let Some(index) = models
        .iter()
        .position(|candidate| candidate.get("slug").and_then(Value::as_str) == Some(slug))
    {
        models[index] = entry;
    } else {
        models.push(entry);
    }

    serde_json::to_vec_pretty(&root)
        .map(Some)
        .map_err(|error| format!("could not serialize managed model catalog: {error}"))
}

pub fn is_supported_model(model: &str) -> bool {
    supported_entry(model).is_some()
}

fn supported_entry(model: &str) -> Option<Value> {
    let normalized = model.trim();
    let lower = normalized.to_ascii_lowercase();
    if lower.starts_with("glm-") {
        return Some(entry(
            normalized,
            "Z.ai relay model",
            "max",
            json!([
                { "effort": "low", "description": "Light reasoning" },
                { "effort": "high", "description": "Enhanced reasoning" },
                { "effort": "max", "description": "Deep reasoning" }
            ]),
            1_048_576,
            95,
        ));
    }
    if lower.starts_with("deepseek-") && lower != "deepseek-v4-pro" {
        return Some(entry(
            normalized,
            "DeepSeek relay model",
            "high",
            json!([
                { "effort": "low", "description": "Light reasoning" },
                { "effort": "medium", "description": "Balanced reasoning" },
                { "effort": "high", "description": "Enhanced reasoning" }
            ]),
            128_000,
            90,
        ));
    }
    if lower.starts_with("qwen3") && !lower.contains("-vl-") {
        return Some(entry(
            normalized,
            "Qwen relay model",
            "medium",
            json!([
                { "effort": "low", "description": "Light reasoning" },
                { "effort": "medium", "description": "Balanced reasoning" },
                { "effort": "high", "description": "Enhanced reasoning" },
                { "effort": "xhigh", "description": "Extra high reasoning" }
            ]),
            500_000,
            95,
        ));
    }
    None
}

fn entry(
    slug: &str,
    description: &str,
    default_reasoning_level: &str,
    supported_reasoning_levels: Value,
    context_window: u64,
    effective_context_window_percent: u64,
) -> Value {
    let mut value = Map::new();
    value.insert("slug".to_owned(), json!(slug));
    value.insert("display_name".to_owned(), json!(slug));
    value.insert("description".to_owned(), json!(description));
    value.insert(
        "default_reasoning_level".to_owned(),
        json!(default_reasoning_level),
    );
    value.insert(
        "supported_reasoning_levels".to_owned(),
        supported_reasoning_levels,
    );
    value.insert("shell_type".to_owned(), json!("shell_command"));
    value.insert("visibility".to_owned(), json!("list"));
    value.insert("supported_in_api".to_owned(), json!(true));
    value.insert("priority".to_owned(), json!(0));
    value.insert("base_instructions".to_owned(), json!(""));
    value.insert("supports_reasoning_summaries".to_owned(), json!(true));
    value.insert("default_reasoning_summary".to_owned(), json!("none"));
    value.insert("support_verbosity".to_owned(), json!(false));
    value.insert("apply_patch_tool_type".to_owned(), json!("freeform"));
    value.insert(
        "truncation_policy".to_owned(),
        json!({ "mode": "bytes", "limit": 10_000 }),
    );
    value.insert("context_window".to_owned(), json!(context_window));
    value.insert("max_context_window".to_owned(), json!(context_window));
    value.insert(
        "effective_context_window_percent".to_owned(),
        json!(effective_context_window_percent),
    );
    value.insert("supports_parallel_tool_calls".to_owned(), json!(true));
    value.insert("experimental_supported_tools".to_owned(), json!([]));
    value.insert("input_modalities".to_owned(), json!(["text"]));
    Value::Object(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supports_only_the_intended_text_model_families() {
        assert!(is_supported_model("glm-5.3"));
        assert!(is_supported_model("deepseek-v4-flash"));
        assert!(is_supported_model("qwen3.8-max"));
        assert!(!is_supported_model("kimi-k3"));
        assert!(!is_supported_model("deepseek-v4-pro"));
        assert!(!is_supported_model("qwen3-vl-plus"));
        assert!(!is_supported_model("qwen-image-max"));
    }

    #[test]
    fn merges_without_removing_existing_models() {
        let catalog =
            merge_supported_model(Some(br#"{"models":[{"slug":"gpt-5.6-sol"}]}"#), "glm-5.3")
                .unwrap()
                .unwrap();
        let parsed: Value = serde_json::from_slice(&catalog).unwrap();
        let slugs: Vec<&str> = parsed["models"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|entry| entry["slug"].as_str())
            .collect();
        assert_eq!(slugs, vec!["gpt-5.6-sol", "glm-5.3"]);
    }
}
