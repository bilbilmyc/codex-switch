use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use url::Url;
use uuid::Uuid;

#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn new(value: impl Into<String>) -> Result<Self, ApiKeyError> {
        let value = value.into();
        validate_api_key(&value)?;
        Ok(Self(value))
    }

    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ApiKey([REDACTED])")
    }
}

impl fmt::Display for ApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

impl Serialize for ApiKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ApiKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ApiKeyError {
    #[error("API key cannot be empty")]
    Empty,
    #[error("API key cannot start or end with whitespace")]
    SurroundingWhitespace,
    #[error("API key cannot contain control characters")]
    ControlCharacter,
}

fn validate_api_key(value: &str) -> Result<(), ApiKeyError> {
    if value.trim().is_empty() {
        return Err(ApiKeyError::Empty);
    }
    if value.trim() != value {
        return Err(ApiKeyError::SurroundingWhitespace);
    }
    if value.chars().any(char::is_control) {
        return Err(ApiKeyError::ControlCharacter);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProfileId(Uuid);

impl ProfileId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    pub fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Default for ProfileId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ProfileId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for ProfileId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(value).map(Self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub id: ProfileId,
    pub name: String,
    pub base_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<ApiKey>,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<ProfileContext>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_auto_compact_token_limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_auto_compact_token_limit_scope: Option<AutoCompactScope>,
}

impl ProfileContext {
    pub fn validate(self) -> Result<(), DomainError> {
        for (field, value) in [
            ("model context window", self.model_context_window),
            (
                "automatic compaction token limit",
                self.model_auto_compact_token_limit,
            ),
        ] {
            if value == Some(0) {
                return Err(DomainError::InvalidTokenSetting { field });
            }
        }
        if let (Some(window), Some(limit)) = (
            self.model_context_window,
            self.model_auto_compact_token_limit,
        ) && limit > window
        {
            return Err(DomainError::CompactLimitExceedsContextWindow);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoCompactScope {
    Total,
    BodyAfterPrefix,
}

impl Profile {
    pub fn new(
        name: impl Into<String>,
        base_url: impl Into<String>,
        api_key: ApiKey,
        model: impl Into<String>,
        review_model: Option<String>,
    ) -> Result<Self, DomainError> {
        let profile = Self {
            id: ProfileId::new(),
            name: name.into(),
            base_url: base_url.into(),
            api_key: Some(api_key),
            model: model.into(),
            review_model,
            context: Some(ProfileContext::default()),
        };
        profile.validate()?;
        Ok(profile)
    }

    pub fn validate(&self) -> Result<(), DomainError> {
        validate_text("profile name", &self.name)?;
        validate_text("model", &self.model)?;
        if let Some(review_model) = &self.review_model {
            validate_text("review model", review_model)?;
        }
        if let Some(context) = self.context {
            context.validate()?;
        }
        validate_base_url(&self.base_url)
    }

    pub fn without_api_key(
        name: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        review_model: Option<String>,
    ) -> Result<Self, DomainError> {
        let profile = Self {
            id: ProfileId::new(),
            name: name.into(),
            base_url: base_url.into(),
            api_key: None,
            model: model.into(),
            review_model,
            context: Some(ProfileContext::default()),
        };
        profile.validate()?;
        Ok(profile)
    }

    pub fn activation(&self) -> Result<Activation, DomainError> {
        self.validate()?;
        Ok(Activation {
            profile_id: self.id,
            provider_name: self.name.clone(),
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone().ok_or(DomainError::MissingApiKey)?,
            model: self.model.clone(),
            review_model: self.review_model.clone(),
            context: self.context,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Activation {
    pub profile_id: ProfileId,
    pub provider_name: String,
    pub base_url: String,
    pub api_key: ApiKey,
    pub model: String,
    pub review_model: Option<String>,
    pub context: Option<ProfileContext>,
}

impl Activation {
    pub fn validate(&self) -> Result<(), DomainError> {
        validate_text("provider name", &self.provider_name)?;
        validate_text("model", &self.model)?;
        if let Some(review_model) = &self.review_model {
            validate_text("review model", review_model)?;
        }
        if let Some(context) = self.context {
            context.validate()?;
        }
        validate_base_url(&self.base_url)
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum DomainError {
    #[error("{field} cannot be empty")]
    EmptyField { field: &'static str },
    #[error("{field} cannot contain control characters")]
    ControlCharacter { field: &'static str },
    #[error("invalid base URL: {0}")]
    InvalidBaseUrl(String),
    #[error("base URL scheme must be http or https")]
    UnsupportedBaseUrlScheme,
    #[error("base URL must include a host")]
    MissingBaseUrlHost,
    #[error("base URL cannot contain credentials, a query, or a fragment")]
    UnsafeBaseUrl,
    #[error("API key is required before this profile can be applied")]
    MissingApiKey,
    #[error("{field} must be a positive token count")]
    InvalidTokenSetting { field: &'static str },
    #[error("automatic compaction token limit cannot exceed model context window")]
    CompactLimitExceedsContextWindow,
}

pub(crate) fn validate_text(field: &'static str, value: &str) -> Result<(), DomainError> {
    if value.trim().is_empty() {
        return Err(DomainError::EmptyField { field });
    }
    if value.chars().any(char::is_control) {
        return Err(DomainError::ControlCharacter { field });
    }
    Ok(())
}

pub(crate) fn validate_base_url(value: &str) -> Result<(), DomainError> {
    if value.trim() != value {
        return Err(DomainError::InvalidBaseUrl(
            "leading or trailing whitespace is not allowed".to_owned(),
        ));
    }
    let url = Url::parse(value).map_err(|error| DomainError::InvalidBaseUrl(error.to_string()))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(DomainError::UnsupportedBaseUrlScheme);
    }
    if url.host_str().is_none() {
        return Err(DomainError::MissingBaseUrlHost);
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(DomainError::UnsafeBaseUrl);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_key_is_redacted_but_serializes_as_plaintext() {
        let key = ApiKey::new("sk-test-secret").unwrap();

        assert_eq!(format!("{key}"), "[REDACTED]");
        assert_eq!(format!("{key:?}"), "ApiKey([REDACTED])");
        assert_eq!(serde_json::to_string(&key).unwrap(), r#""sk-test-secret""#);
        assert_eq!(key.expose_secret(), "sk-test-secret");
    }

    #[test]
    fn api_key_rejects_whitespace_and_control_characters() {
        assert_eq!(ApiKey::new(" ").unwrap_err(), ApiKeyError::Empty);
        assert_eq!(
            ApiKey::new(" sk-test").unwrap_err(),
            ApiKeyError::SurroundingWhitespace
        );
        assert_eq!(
            ApiKey::new("sk-test\n").unwrap_err(),
            ApiKeyError::SurroundingWhitespace
        );
    }

    #[test]
    fn profile_id_round_trips_through_display_and_from_str() {
        let id = ProfileId::new();
        assert_eq!(id.to_string().parse::<ProfileId>().unwrap(), id);
    }

    #[test]
    fn profile_validates_url_and_models() {
        let profile = Profile::new(
            "Relay A",
            "https://relay.example/v1",
            ApiKey::new("sk-test").unwrap(),
            "gpt-5.2-codex",
            None,
        )
        .unwrap();
        assert_eq!(profile.activation().unwrap().provider_name, "Relay A");

        let error = Profile::new(
            "Relay A",
            "https://user:pass@relay.example/v1",
            ApiKey::new("sk-test").unwrap(),
            "gpt-5.2-codex",
            None,
        )
        .unwrap_err();
        assert_eq!(error, DomainError::UnsafeBaseUrl);
    }

    #[test]
    fn profile_without_key_can_be_saved_but_not_activated() {
        let profile = Profile::without_api_key(
            "Shared Relay",
            "https://relay.example/v1",
            "gpt-5.2-codex",
            None,
        )
        .unwrap();

        assert_eq!(profile.api_key, None);
        assert_eq!(
            profile.activation().unwrap_err(),
            DomainError::MissingApiKey
        );
    }

    #[test]
    fn profile_context_rejects_invalid_token_relationships() {
        let mut profile = Profile::new(
            "Relay A",
            "https://relay.example/v1",
            ApiKey::new("sk-test").unwrap(),
            "gpt-5.2-codex",
            None,
        )
        .unwrap();
        profile.context = Some(ProfileContext {
            model_context_window: Some(100_000),
            model_auto_compact_token_limit: Some(120_000),
            model_auto_compact_token_limit_scope: Some(AutoCompactScope::Total),
        });

        assert_eq!(
            profile.validate().unwrap_err(),
            DomainError::CompactLimitExceedsContextWindow
        );
    }
}
