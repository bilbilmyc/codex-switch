use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue};
use sha2::{Digest, Sha256};
use thiserror::Error;
use toml_edit::{DocumentMut, Item, Table, Value};

use crate::domain::{
    Activation, ApiKey, ApiKeyError, DomainError, Profile, validate_base_url, validate_text,
};

pub const TOOL_PROVIDER_ID: &str = "codex_switch";
pub const RESPONSES_WIRE_API: &str = "responses";

const TOOL_PROVIDER_KEYS: [&str; 4] = ["name", "base_url", "wire_api", "requires_openai_auth"];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolProviderProjection {
    pub name: String,
    pub base_url: String,
    pub wire_api: String,
    pub requires_openai_auth: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigProjection {
    pub model_provider: Option<String>,
    pub model: Option<String>,
    pub review_model: Option<String>,
    pub tool_provider: Option<ToolProviderProjection>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelevantProjection {
    pub config: ConfigProjection,
    pub auth_api_key_sha256: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PatchResult {
    pub contents: String,
    pub projection: ConfigProjection,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedProvider {
    pub provider_id: String,
    pub name: String,
    pub base_url: String,
    pub model: String,
    pub review_model: Option<String>,
}

pub fn inspect_codex_config(raw: &str) -> Result<ConfigProjection, CodexConfigError> {
    let document = parse_document(raw)?;
    inspect_document(&document)
}

pub fn patch_codex_config(
    raw: &str,
    activation: &Activation,
) -> Result<PatchResult, CodexConfigError> {
    activation.validate()?;
    let mut document = parse_document(raw)?;
    inspect_tool_provider(&document)?;

    let root = document.as_table_mut();
    set_string_preserving_decor(root, "model_provider", TOOL_PROVIDER_ID)?;
    set_string_preserving_decor(root, "model", &activation.model)?;
    if let Some(review_model) = &activation.review_model {
        set_string_preserving_decor(root, "review_model", review_model)?;
    } else {
        ensure_optional_string(root, "review_model", "review_model")?;
        root.remove("review_model");
    }

    let providers = provider_registry_mut(root)?;
    if !providers.contains_key(TOOL_PROVIDER_ID) {
        providers.insert(TOOL_PROVIDER_ID, Item::Table(Table::new()));
    }
    let provider = providers
        .get_mut(TOOL_PROVIDER_ID)
        .and_then(Item::as_table_mut)
        .ok_or_else(|| type_error("model_providers.codex_switch", "a table"))?;

    set_string_preserving_decor(provider, "name", &activation.provider_name)?;
    set_string_preserving_decor(provider, "base_url", &activation.base_url)?;
    set_string_preserving_decor(provider, "wire_api", RESPONSES_WIRE_API)?;
    set_bool_preserving_decor(provider, "requires_openai_auth", true)?;

    let contents = document.to_string();
    let projection = inspect_codex_config(&contents)?;
    Ok(PatchResult {
        contents,
        projection,
    })
}

pub fn import_current_provider(raw: &str) -> Result<ImportedProvider, CodexConfigError> {
    let document = parse_document(raw)?;
    let root = document.as_table();
    let provider_id = required_string(root, "model_provider", "model_provider")?.to_owned();
    let model = required_string(root, "model", "model")?.to_owned();
    let review_model = optional_string(root, "review_model", "review_model")?.map(str::to_owned);

    let providers = root
        .get("model_providers")
        .and_then(Item::as_table)
        .ok_or_else(|| CodexConfigError::ProviderNotDefined(provider_id.clone()))?;
    let provider = providers
        .get(&provider_id)
        .and_then(Item::as_table)
        .ok_or_else(|| CodexConfigError::ProviderNotDefined(provider_id.clone()))?;

    let unsupported_keys: Vec<String> = provider
        .iter()
        .filter(|(key, _)| !is_tool_provider_key(key))
        .map(|(key, _)| key.to_owned())
        .collect();
    if !unsupported_keys.is_empty() {
        return Err(CodexConfigError::UnsupportedProviderFields {
            provider_id,
            fields: unsupported_keys,
        });
    }

    let name = required_string(provider, "name", "model_providers.<current>.name")?.to_owned();
    let base_url =
        required_string(provider, "base_url", "model_providers.<current>.base_url")?.to_owned();
    let wire_api = optional_string(provider, "wire_api", "model_providers.<current>.wire_api")?
        .unwrap_or(RESPONSES_WIRE_API);
    if wire_api != RESPONSES_WIRE_API {
        return Err(CodexConfigError::UnsupportedWireApi(wire_api.to_owned()));
    }
    let requires_auth = optional_bool(
        provider,
        "requires_openai_auth",
        "model_providers.<current>.requires_openai_auth",
    )?
    .unwrap_or(false);
    if !requires_auth {
        return Err(CodexConfigError::ProviderDoesNotUseOpenAiAuth);
    }

    validate_text("provider name", &name)?;
    validate_text("model", &model)?;
    if let Some(review_model) = &review_model {
        validate_text("review model", review_model)?;
    }
    validate_base_url(&base_url)?;

    Ok(ImportedProvider {
        provider_id,
        name,
        base_url,
        model,
        review_model,
    })
}

pub fn import_current_profile(
    config_raw: &str,
    auth_raw: Option<&[u8]>,
) -> Result<Profile, CodexConfigError> {
    let imported = import_current_provider(config_raw)?;
    let api_key = inspect_auth_api_key(auth_raw)?.ok_or(CodexConfigError::MissingApiKey)?;
    Profile::new(
        imported.name,
        imported.base_url,
        api_key,
        imported.model,
        imported.review_model,
    )
    .map_err(CodexConfigError::from)
}

pub fn inspect_auth_api_key(raw: Option<&[u8]>) -> Result<Option<ApiKey>, CodexConfigError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let value: JsonValue = serde_json::from_slice(raw)?;
    let object = value
        .as_object()
        .ok_or(CodexConfigError::AuthRootNotObject)?;
    match object.get("OPENAI_API_KEY") {
        None | Some(JsonValue::Null) => Ok(None),
        Some(JsonValue::String(value)) => ApiKey::new(value.clone())
            .map(Some)
            .map_err(CodexConfigError::InvalidApiKey),
        Some(_) => Err(type_error("auth.json.OPENAI_API_KEY", "a string")),
    }
}

pub fn patch_auth_json(raw: Option<&[u8]>, api_key: &ApiKey) -> Result<Vec<u8>, CodexConfigError> {
    let mut object = match raw {
        None => JsonMap::new(),
        Some(raw) => {
            let value: JsonValue = serde_json::from_slice(raw)?;
            value
                .as_object()
                .cloned()
                .ok_or(CodexConfigError::AuthRootNotObject)?
        }
    };
    if object
        .get("OPENAI_API_KEY")
        .is_some_and(|value| !value.is_string() && !value.is_null())
    {
        return Err(type_error("auth.json.OPENAI_API_KEY", "a string"));
    }
    object.insert(
        "OPENAI_API_KEY".to_owned(),
        JsonValue::String(api_key.expose_secret().to_owned()),
    );

    let mut bytes = serde_json::to_vec_pretty(&JsonValue::Object(object))?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn relevant_projection(
    config_raw: &str,
    auth_raw: Option<&[u8]>,
) -> Result<RelevantProjection, CodexConfigError> {
    let config = inspect_codex_config(config_raw)?;
    let auth_api_key_sha256 = inspect_auth_api_key(auth_raw)?
        .as_ref()
        .map(|key| sha256_hex(key.expose_secret().as_bytes()));
    Ok(RelevantProjection {
        config,
        auth_api_key_sha256,
    })
}

pub fn relevant_fingerprint(
    config_raw: &str,
    auth_raw: Option<&[u8]>,
) -> Result<String, CodexConfigError> {
    let projection = relevant_projection(config_raw, auth_raw)?;
    let canonical = serde_json::to_vec(&projection)?;
    Ok(sha256_hex(&canonical))
}

fn parse_document(raw: &str) -> Result<DocumentMut, CodexConfigError> {
    raw.parse::<DocumentMut>()
        .map_err(CodexConfigError::InvalidToml)
}

fn inspect_document(document: &DocumentMut) -> Result<ConfigProjection, CodexConfigError> {
    let root = document.as_table();
    Ok(ConfigProjection {
        model_provider: optional_string(root, "model_provider", "model_provider")?
            .map(str::to_owned),
        model: optional_string(root, "model", "model")?.map(str::to_owned),
        review_model: optional_string(root, "review_model", "review_model")?.map(str::to_owned),
        tool_provider: inspect_tool_provider(document)?,
    })
}

fn inspect_tool_provider(
    document: &DocumentMut,
) -> Result<Option<ToolProviderProjection>, CodexConfigError> {
    let Some(providers_item) = document.as_table().get("model_providers") else {
        return Ok(None);
    };
    let providers = providers_item
        .as_table()
        .ok_or_else(|| type_error("model_providers", "a table"))?;
    let Some(provider_item) = providers.get(TOOL_PROVIDER_ID) else {
        return Ok(None);
    };
    let provider = provider_item
        .as_table()
        .ok_or(CodexConfigError::ConflictingToolProvider)?;

    if provider.len() != TOOL_PROVIDER_KEYS.len()
        || provider.iter().any(|(key, _)| !is_tool_provider_key(key))
    {
        return Err(CodexConfigError::ConflictingToolProvider);
    }

    let projection = ToolProviderProjection {
        name: required_string(provider, "name", "model_providers.codex_switch.name")?.to_owned(),
        base_url: required_string(
            provider,
            "base_url",
            "model_providers.codex_switch.base_url",
        )?
        .to_owned(),
        wire_api: required_string(
            provider,
            "wire_api",
            "model_providers.codex_switch.wire_api",
        )?
        .to_owned(),
        requires_openai_auth: required_bool(
            provider,
            "requires_openai_auth",
            "model_providers.codex_switch.requires_openai_auth",
        )?,
    };
    if projection.wire_api != RESPONSES_WIRE_API || !projection.requires_openai_auth {
        return Err(CodexConfigError::ConflictingToolProvider);
    }
    Ok(Some(projection))
}

fn provider_registry_mut(root: &mut Table) -> Result<&mut Table, CodexConfigError> {
    if !root.contains_key("model_providers") {
        let mut table = Table::new();
        table.set_implicit(true);
        root.insert("model_providers", Item::Table(table));
    }
    root.get_mut("model_providers")
        .and_then(Item::as_table_mut)
        .ok_or_else(|| type_error("model_providers", "a table"))
}

fn is_tool_provider_key(key: &str) -> bool {
    TOOL_PROVIDER_KEYS.contains(&key)
}

fn set_string_preserving_decor(
    table: &mut Table,
    key: &str,
    new_value: &str,
) -> Result<(), CodexConfigError> {
    let Some(item) = table.get_mut(key) else {
        table.insert(key, Item::Value(Value::from(new_value)));
        return Ok(());
    };
    let value = item
        .as_value_mut()
        .ok_or_else(|| type_error(key, "a string"))?;
    if !value.is_str() {
        return Err(type_error(key, "a string"));
    }
    let decor = value.decor().clone();
    let mut replacement = Value::from(new_value);
    *replacement.decor_mut() = decor;
    *value = replacement;
    Ok(())
}

fn set_bool_preserving_decor(
    table: &mut Table,
    key: &str,
    new_value: bool,
) -> Result<(), CodexConfigError> {
    let Some(item) = table.get_mut(key) else {
        table.insert(key, Item::Value(Value::from(new_value)));
        return Ok(());
    };
    let value = item
        .as_value_mut()
        .ok_or_else(|| type_error(key, "a boolean"))?;
    if !value.is_bool() {
        return Err(type_error(key, "a boolean"));
    }
    let decor = value.decor().clone();
    let mut replacement = Value::from(new_value);
    *replacement.decor_mut() = decor;
    *value = replacement;
    Ok(())
}

fn ensure_optional_string(table: &Table, key: &str, path: &str) -> Result<(), CodexConfigError> {
    optional_string(table, key, path).map(|_| ())
}

fn optional_string<'a>(
    table: &'a Table,
    key: &str,
    path: &str,
) -> Result<Option<&'a str>, CodexConfigError> {
    match table.get(key) {
        None => Ok(None),
        Some(item) => item
            .as_str()
            .map(Some)
            .ok_or_else(|| type_error(path, "a string")),
    }
}

fn required_string<'a>(
    table: &'a Table,
    key: &str,
    path: &str,
) -> Result<&'a str, CodexConfigError> {
    optional_string(table, key, path)?
        .ok_or_else(|| CodexConfigError::MissingField(path.to_owned()))
}

fn optional_bool(table: &Table, key: &str, path: &str) -> Result<Option<bool>, CodexConfigError> {
    match table.get(key) {
        None => Ok(None),
        Some(item) => item
            .as_bool()
            .map(Some)
            .ok_or_else(|| type_error(path, "a boolean")),
    }
}

fn required_bool(table: &Table, key: &str, path: &str) -> Result<bool, CodexConfigError> {
    optional_bool(table, key, path)?.ok_or_else(|| CodexConfigError::MissingField(path.to_owned()))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn type_error(path: impl Into<String>, expected: &'static str) -> CodexConfigError {
    CodexConfigError::InvalidValueType {
        path: path.into(),
        expected,
    }
}

#[derive(Debug, Error)]
pub enum CodexConfigError {
    #[error("Codex config TOML is invalid: {0}")]
    InvalidToml(toml_edit::TomlError),
    #[error(transparent)]
    InvalidDomain(#[from] DomainError),
    #[error("{path} must be {expected}")]
    InvalidValueType {
        path: String,
        expected: &'static str,
    },
    #[error("required Codex config field is missing: {0}")]
    MissingField(String),
    #[error("model provider {0:?} has no custom provider table and cannot be imported")]
    ProviderNotDefined(String),
    #[error("provider {provider_id:?} uses unsupported fields: {fields:?}")]
    UnsupportedProviderFields {
        provider_id: String,
        fields: Vec<String>,
    },
    #[error("provider uses unsupported wire API {0:?}; only responses is supported")]
    UnsupportedWireApi(String),
    #[error("provider does not use Codex/OpenAI authentication")]
    ProviderDoesNotUseOpenAiAuth,
    #[error("model_providers.codex_switch already exists with a conflicting shape")]
    ConflictingToolProvider,
    #[error("auth.json is invalid: {0}")]
    InvalidAuthJson(#[from] serde_json::Error),
    #[error("auth.json root must be a JSON object")]
    AuthRootNotObject,
    #[error("auth.json does not contain OPENAI_API_KEY")]
    MissingApiKey,
    #[error("auth.json contains an invalid API key: {0}")]
    InvalidApiKey(ApiKeyError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ApiKey, ProfileId};

    fn activation(review_model: Option<&str>) -> Activation {
        Activation {
            profile_id: ProfileId::new(),
            provider_name: "Relay A".to_owned(),
            base_url: "https://relay.example/v1".to_owned(),
            api_key: ApiKey::new("sk-new-secret").unwrap(),
            model: "gpt-5.2-codex".to_owned(),
            review_model: review_model.map(str::to_owned),
        }
    }

    #[test]
    fn patches_only_owned_fields_and_preserves_comments() {
        let raw = r#"# user comment
model_provider = "old"
model = "old-model" # keep this explanation
review_model = "old-review"

[features]
experimental = true

[model_providers.other]
name = "Other"
base_url = "https://other.example/v1"
"#;

        let result = patch_codex_config(raw, &activation(None)).unwrap();

        assert!(result.contents.contains("# user comment"));
        assert!(result.contents.contains("# keep this explanation"));
        assert!(result.contents.contains("[features]\nexperimental = true"));
        assert!(result.contents.contains("[model_providers.other]"));
        assert!(!result.contents.contains("review_model"));
        assert_eq!(
            result.projection.model_provider.as_deref(),
            Some(TOOL_PROVIDER_ID)
        );
        assert_eq!(result.projection.model.as_deref(), Some("gpt-5.2-codex"));
        assert_eq!(
            result.projection.tool_provider,
            Some(ToolProviderProjection {
                name: "Relay A".to_owned(),
                base_url: "https://relay.example/v1".to_owned(),
                wire_api: RESPONSES_WIRE_API.to_owned(),
                requires_openai_auth: true,
            })
        );
    }

    #[test]
    fn accepts_tool_shaped_provider_but_rejects_collisions() {
        let compatible = r#"
[model_providers.codex_switch]
name = "Old"
base_url = "https://old.example/v1"
wire_api = "responses"
requires_openai_auth = true
"#;
        assert!(patch_codex_config(compatible, &activation(Some("review"))).is_ok());

        let conflicting = format!("{compatible}env_key = \"RELAY_KEY\"\n");
        assert!(matches!(
            patch_codex_config(&conflicting, &activation(None)),
            Err(CodexConfigError::ConflictingToolProvider)
        ));
    }

    #[test]
    fn auth_patch_preserves_unknown_fields_and_does_not_expose_key_in_debug() {
        let raw = br#"{"metadata":{"keep":true},"OPENAI_API_KEY":"old"}"#;
        let key = ApiKey::new("sk-new-secret").unwrap();
        let patched = patch_auth_json(Some(raw), &key).unwrap();
        let parsed: JsonValue = serde_json::from_slice(&patched).unwrap();

        assert_eq!(parsed["metadata"]["keep"], true);
        assert_eq!(parsed["OPENAI_API_KEY"], "sk-new-secret");
        assert!(!format!("{key:?}").contains("sk-new-secret"));
    }

    #[test]
    fn fingerprint_ignores_unrelated_config_and_comments() {
        let first = r#"
model_provider = "codex_switch"
model = "gpt-5"
[features]
one = true
[model_providers.codex_switch]
name = "Relay"
base_url = "https://relay.example/v1"
wire_api = "responses"
requires_openai_auth = true
"#;
        let second = first.replace("one = true", "one = false\n# unrelated change");
        let auth = br#"{"OPENAI_API_KEY":"sk-same"}"#;

        assert_eq!(
            relevant_fingerprint(first, Some(auth)).unwrap(),
            relevant_fingerprint(&second, Some(auth)).unwrap()
        );
    }

    #[test]
    fn imports_current_compatible_provider_and_api_key() {
        let config = r#"
model_provider = "relay"
model = "gpt-5"
review_model = "gpt-5-review"

[model_providers.relay]
name = "My Relay"
base_url = "https://relay.example/v1"
wire_api = "responses"
requires_openai_auth = true
"#;
        let auth = br#"{"OPENAI_API_KEY":"sk-imported"}"#;

        let profile = import_current_profile(config, Some(auth)).unwrap();
        assert_eq!(profile.name, "My Relay");
        assert_eq!(profile.model, "gpt-5");
        assert_eq!(profile.review_model.as_deref(), Some("gpt-5-review"));
        assert_eq!(
            profile.api_key.as_ref().unwrap().expose_secret(),
            "sk-imported"
        );
    }
}
