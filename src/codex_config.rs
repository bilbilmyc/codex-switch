use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue};
use sha2::{Digest, Sha256};
use thiserror::Error;
use toml_edit::{DocumentMut, Item, Table, Value};

use crate::domain::{
    Activation, ApiKey, ApiKeyError, AutoCompactScope, DomainError, Profile, ProfileContext,
    ProfileId, validate_base_url, validate_text,
};

pub const TOOL_PROVIDER_ID: &str = "codex_switch";
const TOOL_PROVIDER_ID_PREFIX: &str = "codex_switch_";
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
    pub context: ContextSettings,
    pub tool_provider: Option<ToolProviderProjection>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelevantProjection {
    pub config: ConfigProjection,
    pub auth_api_key_sha256: Option<String>,
}

// Exact serialized projection used by schema-v1 states created before context was managed.
#[derive(Serialize)]
struct PreContextConfigProjection<'a> {
    model_provider: &'a Option<String>,
    model: &'a Option<String>,
    review_model: &'a Option<String>,
    tool_provider: &'a Option<ToolProviderProjection>,
}

#[derive(Serialize)]
struct PreContextRelevantProjection<'a> {
    config: PreContextConfigProjection<'a>,
    auth_api_key_sha256: &'a Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PatchResult {
    pub contents: String,
    pub projection: ConfigProjection,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextSettings {
    pub model_context_window: Option<u64>,
    pub model_auto_compact_token_limit: Option<u64>,
    pub model_auto_compact_token_limit_scope: Option<AutoCompactScope>,
}

impl AutoCompactScope {
    fn as_str(self) -> &'static str {
        match self {
            Self::Total => "total",
            Self::BodyAfterPrefix => "body_after_prefix",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedProvider {
    pub provider_id: String,
    pub name: String,
    pub base_url: String,
    pub model: String,
    pub review_model: Option<String>,
    pub context: ProfileContext,
}

pub fn provider_id_for_profile(profile_id: ProfileId) -> String {
    format!("{TOOL_PROVIDER_ID_PREFIX}{}", profile_id.as_uuid().simple())
}

/// Recovers the stable profile identity encoded in providers written by this application.
///
/// The original shared provider name intentionally has no embedded identity and therefore does
/// not match this function.
pub fn profile_id_from_provider_id(provider_id: &str) -> Option<ProfileId> {
    provider_id
        .strip_prefix(TOOL_PROVIDER_ID_PREFIX)
        .and_then(|suffix| uuid::Uuid::parse_str(suffix).ok())
        .map(ProfileId::from_uuid)
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
    let provider_id = provider_id_for_profile(activation.profile_id);
    inspect_tool_provider(&document, &provider_id)?;

    let root = document.as_table_mut();
    set_string_preserving_decor(root, "model_provider", &provider_id)?;
    set_string_preserving_decor(root, "model", &activation.model)?;
    if let Some(review_model) = &activation.review_model {
        set_string_preserving_decor(root, "review_model", review_model)?;
    } else {
        ensure_optional_string(root, "review_model", "review_model")?;
        root.remove("review_model");
    }
    if let Some(context) = activation.context {
        patch_context_fields(root, context.into())?;
    }

    let providers = provider_registry_mut(root)?;
    if !providers.contains_key(&provider_id) {
        providers.insert(&provider_id, Item::Table(Table::new()));
    }
    let provider = providers
        .get_mut(&provider_id)
        .and_then(Item::as_table_mut)
        .ok_or_else(|| type_error(format!("model_providers.{provider_id}"), "a table"))?;

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

pub fn patch_model_catalog_path(
    raw: &str,
    relative_path: &str,
) -> Result<String, CodexConfigError> {
    validate_text("model catalog path", relative_path)?;
    let mut document = parse_document(raw)?;
    set_string_preserving_decor(document.as_table_mut(), "model_catalog_json", relative_path)?;
    Ok(document.to_string())
}

pub fn inspect_context_settings(raw: &str) -> Result<ContextSettings, CodexConfigError> {
    let document = parse_document(raw)?;
    inspect_context_document(document.as_table())
}

fn inspect_context_document(root: &Table) -> Result<ContextSettings, CodexConfigError> {
    Ok(ContextSettings {
        model_context_window: optional_positive_integer(
            root,
            "model_context_window",
            "model_context_window",
        )?,
        model_auto_compact_token_limit: optional_positive_integer(
            root,
            "model_auto_compact_token_limit",
            "model_auto_compact_token_limit",
        )?,
        model_auto_compact_token_limit_scope: optional_auto_compact_scope(root)?,
    })
}

impl From<ProfileContext> for ContextSettings {
    fn from(value: ProfileContext) -> Self {
        Self {
            model_context_window: value.model_context_window,
            model_auto_compact_token_limit: value.model_auto_compact_token_limit,
            model_auto_compact_token_limit_scope: value.model_auto_compact_token_limit_scope,
        }
    }
}

impl From<ContextSettings> for ProfileContext {
    fn from(value: ContextSettings) -> Self {
        Self {
            model_context_window: value.model_context_window,
            model_auto_compact_token_limit: value.model_auto_compact_token_limit,
            model_auto_compact_token_limit_scope: value.model_auto_compact_token_limit_scope,
        }
    }
}

pub fn patch_context_settings(
    raw: &str,
    settings: ContextSettings,
) -> Result<String, CodexConfigError> {
    let mut document = parse_document(raw)?;
    patch_context_fields(document.as_table_mut(), settings)?;
    Ok(document.to_string())
}

fn patch_context_fields(
    root: &mut Table,
    settings: ContextSettings,
) -> Result<(), CodexConfigError> {
    if let (Some(window), Some(compact_limit)) = (
        settings.model_context_window,
        settings.model_auto_compact_token_limit,
    ) && compact_limit > window
    {
        return Err(CodexConfigError::CompactLimitExceedsContextWindow);
    }
    patch_optional_positive_integer(root, "model_context_window", settings.model_context_window)?;
    patch_optional_positive_integer(
        root,
        "model_auto_compact_token_limit",
        settings.model_auto_compact_token_limit,
    )?;
    patch_optional_auto_compact_scope(root, settings.model_auto_compact_token_limit_scope)?;
    Ok(())
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

    let context: ProfileContext = inspect_context_document(root)?.into();
    context.validate()?;

    Ok(ImportedProvider {
        provider_id,
        name,
        base_url,
        model,
        review_model,
        context,
    })
}

pub fn import_current_profile(
    config_raw: &str,
    auth_raw: Option<&[u8]>,
) -> Result<Profile, CodexConfigError> {
    let imported = import_current_provider(config_raw)?;
    let api_key = inspect_auth_api_key(auth_raw)?.ok_or(CodexConfigError::MissingApiKey)?;
    let mut profile = Profile::new(
        imported.name,
        imported.base_url,
        api_key,
        imported.model,
        imported.review_model,
    )?;
    if let Some(profile_id) = profile_id_from_provider_id(&imported.provider_id) {
        profile.id = profile_id;
    }
    profile.context = Some(imported.context);
    profile.validate()?;
    Ok(profile)
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
    serialized_fingerprint(&projection)
}

pub(crate) fn pre_context_relevant_fingerprint(
    projection: &RelevantProjection,
) -> Result<String, CodexConfigError> {
    let config = &projection.config;
    serialized_fingerprint(&PreContextRelevantProjection {
        config: PreContextConfigProjection {
            model_provider: &config.model_provider,
            model: &config.model,
            review_model: &config.review_model,
            tool_provider: &config.tool_provider,
        },
        auth_api_key_sha256: &projection.auth_api_key_sha256,
    })
}

fn serialized_fingerprint(value: &impl Serialize) -> Result<String, CodexConfigError> {
    let canonical = serde_json::to_vec(value)?;
    Ok(sha256_hex(&canonical))
}

fn parse_document(raw: &str) -> Result<DocumentMut, CodexConfigError> {
    raw.parse::<DocumentMut>()
        .map_err(CodexConfigError::InvalidToml)
}

fn inspect_document(document: &DocumentMut) -> Result<ConfigProjection, CodexConfigError> {
    let root = document.as_table();
    let model_provider =
        optional_string(root, "model_provider", "model_provider")?.map(str::to_owned);
    Ok(ConfigProjection {
        tool_provider: model_provider
            .as_deref()
            .filter(|provider_id| is_tool_provider_id(provider_id))
            .map(|provider_id| inspect_tool_provider(document, provider_id))
            .transpose()?
            .flatten(),
        model_provider,
        model: optional_string(root, "model", "model")?.map(str::to_owned),
        review_model: optional_string(root, "review_model", "review_model")?.map(str::to_owned),
        context: inspect_context_document(root)?,
    })
}

fn inspect_tool_provider(
    document: &DocumentMut,
    provider_id: &str,
) -> Result<Option<ToolProviderProjection>, CodexConfigError> {
    let Some(providers_item) = document.as_table().get("model_providers") else {
        return Ok(None);
    };
    let providers = providers_item
        .as_table()
        .ok_or_else(|| type_error("model_providers", "a table"))?;
    let Some(provider_item) = providers.get(provider_id) else {
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
        name: required_string(
            provider,
            "name",
            &format!("model_providers.{provider_id}.name"),
        )?
        .to_owned(),
        base_url: required_string(
            provider,
            "base_url",
            &format!("model_providers.{provider_id}.base_url"),
        )?
        .to_owned(),
        wire_api: required_string(
            provider,
            "wire_api",
            &format!("model_providers.{provider_id}.wire_api"),
        )?
        .to_owned(),
        requires_openai_auth: required_bool(
            provider,
            "requires_openai_auth",
            &format!("model_providers.{provider_id}.requires_openai_auth"),
        )?,
    };
    if projection.wire_api != RESPONSES_WIRE_API || !projection.requires_openai_auth {
        return Err(CodexConfigError::ConflictingToolProvider);
    }
    Ok(Some(projection))
}

fn is_tool_provider_id(provider_id: &str) -> bool {
    provider_id == TOOL_PROVIDER_ID || profile_id_from_provider_id(provider_id).is_some()
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

fn patch_optional_positive_integer(
    table: &mut Table,
    key: &str,
    value: Option<u64>,
) -> Result<(), CodexConfigError> {
    let Some(value) = value else {
        optional_positive_integer(table, key, key)?;
        table.remove(key);
        return Ok(());
    };
    let value = i64::try_from(value).map_err(|_| CodexConfigError::InvalidPositiveInteger {
        path: key.to_owned(),
    })?;
    if value == 0 {
        return Err(CodexConfigError::InvalidPositiveInteger {
            path: key.to_owned(),
        });
    }

    let Some(item) = table.get_mut(key) else {
        table.insert(key, Item::Value(Value::from(value)));
        return Ok(());
    };
    let current = item
        .as_value_mut()
        .ok_or_else(|| type_error(key, "a positive integer"))?;
    if !current.is_integer() {
        return Err(type_error(key, "a positive integer"));
    }
    let decor = current.decor().clone();
    let mut replacement = Value::from(value);
    *replacement.decor_mut() = decor;
    *current = replacement;
    Ok(())
}

fn patch_optional_auto_compact_scope(
    table: &mut Table,
    scope: Option<AutoCompactScope>,
) -> Result<(), CodexConfigError> {
    const KEY: &str = "model_auto_compact_token_limit_scope";
    match scope {
        Some(scope) => set_string_preserving_decor(table, KEY, scope.as_str()),
        None => {
            if let Some(item) = table.get(KEY)
                && item.as_str().is_none()
            {
                return Err(type_error(KEY, "total or body_after_prefix"));
            }
            table.remove(KEY);
            Ok(())
        }
    }
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

fn optional_positive_integer(
    table: &Table,
    key: &str,
    path: &str,
) -> Result<Option<u64>, CodexConfigError> {
    let Some(item) = table.get(key) else {
        return Ok(None);
    };
    let value = item
        .as_integer()
        .ok_or_else(|| type_error(path, "a positive integer"))?;
    u64::try_from(value)
        .ok()
        .filter(|value| *value > 0)
        .map(Some)
        .ok_or_else(|| CodexConfigError::InvalidPositiveInteger {
            path: path.to_owned(),
        })
}

fn optional_auto_compact_scope(
    table: &Table,
) -> Result<Option<AutoCompactScope>, CodexConfigError> {
    const KEY: &str = "model_auto_compact_token_limit_scope";
    let Some(item) = table.get(KEY) else {
        return Ok(None);
    };
    match item.as_str() {
        Some("total") => Ok(Some(AutoCompactScope::Total)),
        Some("body_after_prefix") => Ok(Some(AutoCompactScope::BodyAfterPrefix)),
        Some(value) => Err(CodexConfigError::InvalidAutoCompactScope(value.to_owned())),
        None => Err(type_error(KEY, "total or body_after_prefix")),
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
    #[error("{path} must be a positive integer")]
    InvalidPositiveInteger { path: String },
    #[error("model_auto_compact_token_limit cannot exceed model_context_window")]
    CompactLimitExceedsContextWindow,
    #[error("unsupported automatic compaction scope {0:?}")]
    InvalidAutoCompactScope(String),
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
            context: None,
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

        let activation = activation(None);
        let provider_id = provider_id_for_profile(activation.profile_id);
        let result = patch_codex_config(raw, &activation).unwrap();

        assert!(result.contents.contains("# user comment"));
        assert!(result.contents.contains("# keep this explanation"));
        assert!(result.contents.contains("[features]\nexperimental = true"));
        assert!(result.contents.contains("[model_providers.other]"));
        assert!(!result.contents.contains("review_model"));
        assert_eq!(
            result.projection.model_provider.as_deref(),
            Some(provider_id.as_str())
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
    fn context_patch_preserves_unrelated_config_and_never_adds_an_output_limit() {
        let raw = r#"# keep
model = "gpt-5"
model_context_window = 200000 # existing

[features]
one = true
"#;

        let patched = patch_context_settings(
            raw,
            ContextSettings {
                model_context_window: Some(272_000),
                model_auto_compact_token_limit: Some(217_600),
                model_auto_compact_token_limit_scope: Some(AutoCompactScope::Total),
            },
        )
        .unwrap();

        assert!(patched.contains("# keep"));
        assert!(patched.contains("model_context_window = 272000 # existing"));
        assert!(patched.contains("model_auto_compact_token_limit = 217600"));
        assert!(patched.contains("model_auto_compact_token_limit_scope = \"total\""));
        assert!(patched.contains("[features]\none = true"));
        assert!(!patched.contains("max_output"));
        assert_eq!(
            inspect_context_settings(&patched).unwrap(),
            ContextSettings {
                model_context_window: Some(272_000),
                model_auto_compact_token_limit: Some(217_600),
                model_auto_compact_token_limit_scope: Some(AutoCompactScope::Total),
            }
        );
    }

    #[test]
    fn restoring_context_defaults_removes_only_managed_context_fields() {
        let raw = r#"model = "gpt-5"
model_context_window = 272000
model_auto_compact_token_limit = 217600
model_auto_compact_token_limit_scope = "body_after_prefix"
"#;

        let patched = patch_context_settings(raw, ContextSettings::default()).unwrap();

        assert!(!patched.contains("model_context_window"));
        assert!(!patched.contains("model_auto_compact_token_limit ="));
        assert!(!patched.contains("model_auto_compact_token_limit_scope"));
        assert_eq!(
            inspect_context_settings(&patched).unwrap(),
            ContextSettings::default()
        );
    }

    #[test]
    fn context_settings_reject_invalid_values_without_rewriting_the_file() {
        assert!(matches!(
            inspect_context_settings("model_context_window = 0\n"),
            Err(CodexConfigError::InvalidPositiveInteger { .. })
        ));
        assert!(matches!(
            patch_context_settings(
                "model_context_window = \"large\"\n",
                ContextSettings::default()
            ),
            Err(CodexConfigError::InvalidValueType { .. })
        ));
        assert!(matches!(
            patch_context_settings(
                "",
                ContextSettings {
                    model_context_window: Some(100_000),
                    model_auto_compact_token_limit: Some(120_000),
                    model_auto_compact_token_limit_scope: Some(AutoCompactScope::Total),
                }
            ),
            Err(CodexConfigError::CompactLimitExceedsContextWindow)
        ));
    }

    #[test]
    fn accepts_tool_shaped_provider_but_rejects_collisions() {
        let activation = activation(Some("review"));
        let provider_id = provider_id_for_profile(activation.profile_id);
        let compatible = format!(
            r#"
[model_providers.{provider_id}]
name = "Old"
base_url = "https://old.example/v1"
wire_api = "responses"
requires_openai_auth = true
"#
        );
        assert!(patch_codex_config(&compatible, &activation).is_ok());

        let conflicting = format!("{compatible}env_key = \"RELAY_KEY\"\n");
        assert!(matches!(
            patch_codex_config(&conflicting, &activation),
            Err(CodexConfigError::ConflictingToolProvider)
        ));
    }

    #[test]
    fn switching_profiles_keeps_distinct_provider_ids_for_usage_attribution() {
        let first = activation(None);
        let second = activation(Some("review"));
        let first_id = provider_id_for_profile(first.profile_id);
        let second_id = provider_id_for_profile(second.profile_id);

        let after_first = patch_codex_config("", &first).unwrap();
        let after_second = patch_codex_config(&after_first.contents, &second).unwrap();

        assert_ne!(first_id, second_id);
        assert!(
            after_second
                .contents
                .contains(&format!("[model_providers.{first_id}]"))
        );
        assert!(
            after_second
                .contents
                .contains(&format!("[model_providers.{second_id}]"))
        );
        assert_eq!(
            after_second.projection.model_provider.as_deref(),
            Some(second_id.as_str())
        );
    }

    #[test]
    fn profile_context_is_applied_while_legacy_profiles_preserve_live_values() {
        let raw = r#"model_context_window = 180000
model_auto_compact_token_limit = 140000
model_auto_compact_token_limit_scope = "body_after_prefix"
"#;
        let legacy = patch_codex_config(raw, &activation(None)).unwrap();
        assert!(legacy.contents.contains("model_context_window = 180000"));
        assert!(
            legacy
                .contents
                .contains("model_auto_compact_token_limit_scope = \"body_after_prefix\"")
        );

        let mut managed = activation(None);
        managed.context = Some(ProfileContext {
            model_context_window: Some(272_000),
            model_auto_compact_token_limit: Some(217_600),
            model_auto_compact_token_limit_scope: Some(AutoCompactScope::Total),
        });
        let patched = patch_codex_config(raw, &managed).unwrap();
        assert!(patched.contents.contains("model_context_window = 272000"));
        assert!(
            patched
                .contents
                .contains("model_auto_compact_token_limit = 217600")
        );
        assert!(
            patched
                .contents
                .contains("model_auto_compact_token_limit_scope = \"total\"")
        );
        assert!(!patched.contents.contains("max_output"));
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
        let changed_context = format!("model_context_window = 272000\n{first}");
        assert_ne!(
            relevant_fingerprint(first, Some(auth)).unwrap(),
            relevant_fingerprint(&changed_context, Some(auth)).unwrap()
        );
        assert_eq!(
            pre_context_relevant_fingerprint(&relevant_projection(first, Some(auth)).unwrap())
                .unwrap(),
            pre_context_relevant_fingerprint(
                &relevant_projection(&changed_context, Some(auth)).unwrap()
            )
            .unwrap()
        );
    }

    #[test]
    fn imports_current_compatible_provider_and_api_key() {
        let config = r#"
model_provider = "relay"
model = "gpt-5"
review_model = "gpt-5-review"
model_context_window = 272000
model_auto_compact_token_limit = 217600
model_auto_compact_token_limit_scope = "total"

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
            profile.context,
            Some(ProfileContext {
                model_context_window: Some(272_000),
                model_auto_compact_token_limit: Some(217_600),
                model_auto_compact_token_limit_scope: Some(AutoCompactScope::Total),
            })
        );
        assert_eq!(
            profile.api_key.as_ref().unwrap().expose_secret(),
            "sk-imported"
        );
    }

    #[test]
    fn importing_a_managed_provider_preserves_its_profile_id() {
        let profile_id = ProfileId::from_uuid(
            uuid::Uuid::parse_str("e519bc8f-120c-43c3-96b5-a7799f6eec18").unwrap(),
        );
        let provider_id = provider_id_for_profile(profile_id);
        let config = format!(
            r#"
model_provider = "{provider_id}"
model = "gpt-5"

[model_providers.{provider_id}]
name = "Managed Relay"
base_url = "https://relay.example/v1"
wire_api = "responses"
requires_openai_auth = true
"#
        );

        let profile =
            import_current_profile(&config, Some(br#"{"OPENAI_API_KEY":"sk-imported"}"#)).unwrap();

        assert_eq!(profile.id, profile_id);
        assert_eq!(provider_id_for_profile(profile.id), provider_id);
    }

    #[test]
    fn patches_model_catalog_path_without_discarding_other_configuration() {
        let patched = patch_model_catalog_path(
            "# keep\nmodel = \"gpt-5\"\nmodel_catalog_json = \"old.json\"\n",
            "model-catalogs/codex-switch-models.json",
        )
        .unwrap();

        assert!(patched.contains("# keep"));
        assert!(patched.contains("model = \"gpt-5\""));
        assert!(
            patched.contains("model_catalog_json = \"model-catalogs/codex-switch-models.json\"")
        );
    }

    #[test]
    fn import_rejects_context_that_violates_profile_invariants() {
        let config = r#"
model_provider = "relay"
model = "gpt-5"
model_context_window = 100000
model_auto_compact_token_limit = 120000

[model_providers.relay]
name = "My Relay"
base_url = "https://relay.example/v1"
wire_api = "responses"
requires_openai_auth = true
"#;
        let auth = br#"{"OPENAI_API_KEY":"sk-imported"}"#;

        let error = import_current_profile(config, Some(auth)).unwrap_err();

        assert!(matches!(
            error,
            CodexConfigError::InvalidDomain(DomainError::CompactLimitExceedsContextWindow)
        ));
    }
}
