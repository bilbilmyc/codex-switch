use serde_json::{Map, Value, json};

pub const MANAGED_CATALOG_RELATIVE_PATH: &str = "model-catalogs/codex-switch-models.json";

pub const DEFAULT_CONTEXT_WINDOW: u64 = 128_000;

const REASONING_LEVELS: [(&str, &str); 8] = [
    ("none", "No reasoning"),
    ("minimal", "Minimal reasoning"),
    ("low", "Light reasoning"),
    ("medium", "Balanced reasoning"),
    ("high", "Enhanced reasoning"),
    ("xhigh", "Extra high reasoning"),
    ("max", "Maximum reasoning"),
    ("ultra", "Ultra reasoning"),
];

pub fn has_selectable_capabilities(entry: &Value) -> bool {
    entry["input_modalities"]
        .as_array()
        .is_some_and(|modalities| {
            ["text", "image"]
                .iter()
                .all(|modality| modalities.iter().any(|value| value == modality))
        })
        && entry["supported_reasoning_levels"]
            .as_array()
            .is_some_and(|levels| {
                REASONING_LEVELS
                    .iter()
                    .all(|(effort, _)| levels.iter().any(|level| level["effort"] == *effort))
            })
}

#[derive(Debug)]
pub struct CatalogUpdate {
    pub contents: Vec<u8>,
    pub warning: Option<String>,
}

#[cfg(test)]
pub fn merge_supported_model(
    existing: Option<&[u8]>,
    model: &str,
) -> Result<Option<Vec<u8>>, String> {
    merge_model_catalog(existing, None, model).map(|update| update.map(|value| value.contents))
}

pub fn merge_model_catalog(
    existing: Option<&[u8]>,
    cached: Option<&[u8]>,
    model: &str,
) -> Result<Option<CatalogUpdate>, String> {
    merge_model_catalog_with_window(existing, cached, model, DEFAULT_CONTEXT_WINDOW)
}

pub fn merge_model_catalog_with_window(
    existing: Option<&[u8]>,
    cached: Option<&[u8]>,
    model: &str,
    context_window: u64,
) -> Result<Option<CatalogUpdate>, String> {
    let cached_root = cached.and_then(|bytes| serde_json::from_slice::<Value>(bytes).ok());
    let mut root = match existing {
        Some(bytes) => serde_json::from_slice::<Value>(bytes)
            .map_err(|error| format!("managed model catalog is invalid JSON: {error}"))?,
        None => json!({ "models": cached_root.as_ref()
            .and_then(|root| root.get("models"))
            .and_then(Value::as_array)
            .map(|models| models.iter().filter(|candidate| {
                candidate["slug"].as_str().is_some_and(|slug| usable_entry(candidate, slug))
            }).cloned().collect::<Vec<_>>())
            .unwrap_or_default() }),
    };
    let object = root
        .as_object_mut()
        .ok_or_else(|| "managed model catalog root must be an object".to_owned())?;
    let models = object
        .entry("models")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| "managed model catalog models must be an array".to_owned())?;

    let slug = model.trim();
    let existing_entry = models
        .iter()
        .find(|candidate| usable_entry(candidate, slug));
    let cached_entry = cached_root
        .as_ref()
        .and_then(|root| root.get("models"))
        .and_then(Value::as_array)
        .and_then(|models| {
            models
                .iter()
                .find(|candidate| usable_entry(candidate, slug))
        });
    let selected = existing_entry
        .filter(|candidate| !is_generated_entry(candidate))
        .or(cached_entry)
        .or(existing_entry)
        .cloned()
        .or_else(|| supported_entry(slug));
    let Some(mut selected) = selected else {
        return Ok(None);
    };
    if is_generated_entry(&selected) {
        selected["description"] = json!("User-configured relay model");
        selected["support_verbosity"] = json!(true);
    }
    expand_selectable_capabilities(&mut selected);
    selected["context_window"] = json!(context_window);
    selected["max_context_window"] = json!(context_window);
    selected["effective_context_window_percent"] = json!(100);
    selected["visibility"] = json!("list");
    if let Some(index) = models
        .iter()
        .position(|candidate| candidate.get("slug").and_then(Value::as_str) == Some(slug))
    {
        models[index] = selected;
    } else {
        models.push(selected);
    }

    serde_json::to_vec_pretty(&root)
        .map(|contents| {
            Some(CatalogUpdate {
                contents,
                warning: None,
            })
        })
        .map_err(|error| format!("could not serialize managed model catalog: {error}"))
}

#[cfg(test)]
fn is_supported_model(model: &str) -> bool {
    supported_entry(model).is_some()
}

fn usable_entry(candidate: &Value, slug: &str) -> bool {
    candidate["slug"].as_str() == Some(slug)
        && candidate["supported_in_api"].as_bool() == Some(true)
        && candidate["context_window"]
            .as_u64()
            .is_some_and(|size| size > 0)
        && candidate["supported_reasoning_levels"].is_array()
        && candidate["default_reasoning_level"].is_string()
        && candidate["shell_type"].is_string()
        && candidate["base_instructions"].is_string()
        && candidate["input_modalities"]
            .as_array()
            .is_some_and(|modalities| {
                modalities
                    .iter()
                    .any(|modality| modality.as_str() == Some("text"))
            })
}

fn is_generated_entry(candidate: &Value) -> bool {
    matches!(
        candidate["description"].as_str(),
        Some(
            "User-configured relay model"
                | "GPT relay compatibility profile (unverified conservative limits)"
                | "GPT-6 Astra relay compatibility profile (conservative limits)"
        )
    )
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
            DEFAULT_CONTEXT_WINDOW,
            100,
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
            DEFAULT_CONTEXT_WINDOW,
            100,
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
            DEFAULT_CONTEXT_WINDOW,
            100,
        ));
    }
    if normalized.is_empty() {
        return None;
    }
    Some(entry(
        normalized,
        "User-configured relay model",
        "medium",
        json!([
            { "effort": "low", "description": "Light reasoning" },
            { "effort": "medium", "description": "Balanced reasoning" },
            { "effort": "high", "description": "Enhanced reasoning" }
        ]),
        DEFAULT_CONTEXT_WINDOW,
        100,
    ))
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
    value.insert("support_verbosity".to_owned(), json!(true));
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
    value.insert("input_modalities".to_owned(), json!(["text", "image"]));
    let mut entry = Value::Object(value);
    expand_selectable_capabilities(&mut entry);
    entry
}

fn expand_selectable_capabilities(entry: &mut Value) {
    let modalities = entry["input_modalities"].as_array_mut().unwrap();
    for modality in ["text", "image"] {
        if !modalities.iter().any(|value| value == modality) {
            modalities.push(json!(modality));
        }
    }
    let existing = entry["supported_reasoning_levels"].as_array().unwrap();
    let mut levels = Vec::new();
    for (effort, description) in REASONING_LEVELS {
        levels.push(
            existing
                .iter()
                .find(|level| level["effort"] == effort)
                .cloned()
                .unwrap_or_else(|| json!({"effort": effort, "description": description})),
        );
    }
    // Preserve provider-specific levels and descriptions alongside the standard choices.
    for level in existing {
        if !levels
            .iter()
            .any(|candidate| candidate["effort"] == level["effort"])
        {
            levels.push(level.clone());
        }
    }
    entry["supported_reasoning_levels"] = json!(levels);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_models_offer_images_and_every_reasoning_effort() {
        for model in ["gpt-99-test", "glm-5.3", "deepseek-v4-flash", "qwen3.8-max"] {
            let update = merge_model_catalog(None, None, model).unwrap().unwrap();
            let catalog: Value = serde_json::from_slice(&update.contents).unwrap();
            let selected = &catalog["models"][0];
            assert_eq!(
                selected["input_modalities"],
                json!(["text", "image"]),
                "{model}"
            );
            let efforts: Vec<_> = selected["supported_reasoning_levels"]
                .as_array()
                .unwrap()
                .iter()
                .map(|level| level["effort"].as_str().unwrap())
                .collect();
            assert_eq!(
                efforts,
                [
                    "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"
                ],
                "{model}"
            );
        }
    }

    #[test]
    fn applying_updates_restricted_existing_and_cached_capabilities() {
        for description in [
            "GPT relay compatibility profile (unverified conservative limits)",
            "User-configured relay model",
            "Z.ai relay model",
            "Provider metadata",
        ] {
            let mut model = supported_entry("gpt-99-test").unwrap();
            model["description"] = json!(description);
            model["input_modalities"] = json!(["text"]);
            model["supported_reasoning_levels"] = json!([
                {"effort": "high", "description": "Provider high"},
                {"effort": "custom", "description": "Provider custom"}
            ]);
            model["base_instructions"] = json!("Keep provider instructions");
            let bytes = serde_json::to_vec(&json!({"models": [model]})).unwrap();
            for (existing, cached) in [
                (Some(bytes.as_slice()), None),
                (None, Some(bytes.as_slice())),
            ] {
                let update = merge_model_catalog(existing, cached, "gpt-99-test")
                    .unwrap()
                    .unwrap();
                let catalog: Value = serde_json::from_slice(&update.contents).unwrap();
                let selected = &catalog["models"][0];
                assert!(
                    selected["input_modalities"]
                        .as_array()
                        .unwrap()
                        .contains(&json!("image")),
                    "{description}"
                );
                let levels = selected["supported_reasoning_levels"].as_array().unwrap();
                for effort in [
                    "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra", "custom",
                ] {
                    assert!(
                        levels.iter().any(|level| level["effort"] == effort),
                        "{description}: {effort}"
                    );
                }
                assert_eq!(selected["base_instructions"], "Keep provider instructions");
                assert_eq!(
                    levels
                        .iter()
                        .find(|level| level["effort"] == "high")
                        .unwrap()["description"],
                    "Provider high"
                );
            }
        }
    }

    fn known_catalog(model: &str) -> Vec<u8> {
        let mut known = supported_entry("glm-5.3").unwrap();
        known["slug"] = json!(model);
        known["display_name"] = json!(model);
        known["base_instructions"] = json!("Preserve these instructions");
        known["context_window"] = json!(1_048_576);
        serde_json::to_vec(&json!({"models": [known]})).unwrap()
    }

    #[test]
    fn exact_cached_metadata_registers_models_without_a_family_rule() {
        let cache = known_catalog("future-agent-test");
        let update = merge_model_catalog(None, Some(&cache), "future-agent-test")
            .unwrap()
            .unwrap();
        assert!(update.warning.is_none());
        let parsed: Value = serde_json::from_slice(&update.contents).unwrap();
        assert_eq!(
            parsed["models"][0]["context_window"],
            DEFAULT_CONTEXT_WINDOW
        );
        assert_eq!(
            parsed["models"][0]["base_instructions"],
            "Preserve these instructions"
        );
    }

    #[test]
    fn cached_metadata_upgrades_generated_entries_without_duplicates() {
        let first = merge_model_catalog(None, None, "gpt-99-test")
            .unwrap()
            .unwrap();
        assert!(first.warning.is_none());
        let cache = known_catalog("gpt-99-test");
        let second = merge_model_catalog(Some(&first.contents), Some(&cache), "gpt-99-test")
            .unwrap()
            .unwrap();
        assert!(second.warning.is_none());
        let parsed: Value = serde_json::from_slice(&second.contents).unwrap();
        assert_eq!(parsed["models"].as_array().unwrap().len(), 1);
        assert_eq!(
            parsed["models"][0]["context_window"],
            DEFAULT_CONTEXT_WINDOW
        );
        assert_eq!(
            parsed["models"][0]["base_instructions"],
            "Preserve these instructions"
        );
    }

    #[test]
    fn existing_metadata_takes_priority_over_the_cache() {
        let existing = known_catalog("gpt-99-test");
        let mut cache: Value = serde_json::from_slice(&existing).unwrap();
        cache["models"][0]["context_window"] = json!(256_000);
        let bytes = serde_json::to_vec(&cache).unwrap();
        let update = merge_model_catalog(Some(&existing), Some(&bytes), "gpt-99-test")
            .unwrap()
            .unwrap();
        let parsed: Value = serde_json::from_slice(&update.contents).unwrap();
        assert_eq!(
            parsed["models"][0]["context_window"],
            DEFAULT_CONTEXT_WINDOW
        );
    }

    #[test]
    fn repeated_apply_is_stable_without_compatibility_warnings() {
        let first = merge_model_catalog(None, None, "gpt-99-test")
            .unwrap()
            .unwrap();
        let second = merge_model_catalog(Some(&first.contents), None, "gpt-99-test")
            .unwrap()
            .unwrap();
        assert!(second.warning.is_none());
        assert_eq!(first.contents, second.contents);
    }

    #[test]
    fn unknown_models_do_not_borrow_metadata_from_a_similar_slug() {
        let cache = known_catalog("gpt-99-test");
        let update = merge_model_catalog(None, Some(&cache), "gpt-99-test-mini")
            .unwrap()
            .unwrap();
        assert!(update.warning.is_none());
        let parsed: Value = serde_json::from_slice(&update.contents).unwrap();
        assert_eq!(parsed["models"].as_array().unwrap().len(), 2);
        assert_eq!(parsed["models"][1]["context_window"], 128_000);
    }

    #[test]
    fn malformed_optional_cache_does_not_block_but_malformed_catalog_does() {
        assert!(
            merge_model_catalog(None, Some(b"broken"), "gpt-99-test")
                .unwrap()
                .unwrap()
                .warning
                .is_none()
        );
        assert!(merge_model_catalog(Some(b"broken"), None, "gpt-99-test").is_err());
    }

    #[test]
    fn model_ids_alone_are_not_capability_metadata() {
        let cache = br#"{"models":[{"slug":"gpt-99-test"}]}"#;
        assert!(
            merge_model_catalog(None, Some(cache), "gpt-99-test")
                .unwrap()
                .unwrap()
                .warning
                .is_none()
        );
    }

    #[test]
    fn explicit_model_ids_are_registered_without_name_based_restrictions() {
        for model in [
            "gpt-image-99",
            "gpt-99-test-image",
            "gpt-99-test-audio-preview",
            "gpt-99-test-realtime",
            "gpt-99-test-transcribe",
            "gpt-99-test-search",
            "gpt-99-test-embedding",
            "gpt-99-test-tts",
            "gpt-99-test-video",
            "unrecognized-model",
        ] {
            assert!(
                merge_model_catalog(None, None, model).unwrap().is_some(),
                "{model}"
            );
        }
    }

    #[test]
    fn future_gpt_text_models_do_not_need_a_version_specific_branch() {
        for model in ["gpt-99-test", "gpt-99.1-test-codex", "gpt-100-test-mini"] {
            assert!(
                merge_supported_model(None, model).unwrap().is_some(),
                "{model}"
            );
        }
    }

    #[test]
    fn non_context_capabilities_are_preserved() {
        let mut known = supported_entry("glm-5.3").unwrap();
        known["context_window"] = json!(256_000);
        known["input_modalities"] = json!(["text", "image"]);
        let existing = serde_json::to_vec(&json!({"models": [known.clone()]})).unwrap();
        let merged = merge_supported_model(Some(&existing), "glm-5.3")
            .unwrap()
            .unwrap();
        let parsed: Value = serde_json::from_slice(&merged).unwrap();
        known["context_window"] = json!(DEFAULT_CONTEXT_WINDOW);
        assert_eq!(parsed["models"][0], known);
    }

    #[test]
    fn accepts_explicit_models_without_a_family_allowlist() {
        assert!(is_supported_model("gpt-6-astra"));
        assert!(is_supported_model("glm-5.3"));
        assert!(is_supported_model("deepseek-v4-flash"));
        assert!(is_supported_model("qwen3.8-max"));
        assert!(is_supported_model("kimi-k3"));
        assert!(is_supported_model("deepseek-v4-pro"));
        assert!(is_supported_model("qwen3-vl-plus"));
        assert!(is_supported_model("qwen-image-max"));
        assert!(!is_supported_model(""));
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
