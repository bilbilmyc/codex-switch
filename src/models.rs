use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::ACCEPT;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

use crate::domain::{Profile, ProfileId};
use crate::durable_fs::{self, DurableFsError};
use crate::openai_http::{self, ResponseReadError};

const MAX_MODELS: usize = 2_000;
const MAX_MODEL_ID_BYTES: usize = 256;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelCache {
    pub schema_version: u32,
    pub profile_id: ProfileId,
    pub fetched_at_ms: u64,
    pub models: Vec<String>,
}

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("Base URL is not a valid HTTP or HTTPS address")]
    InvalidBaseUrl,
    #[error("model endpoint rejected the request with HTTP {0}")]
    HttpStatus(u16),
    #[error("model request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("model response exceeded 2 MiB")]
    ResponseTooLarge,
    #[error("model response was not an OpenAI-compatible model list")]
    InvalidResponse,
    #[error("model cache could not be read or written: {0}")]
    Cache(#[from] DurableFsError),
    #[error("model cache JSON was invalid: {0}")]
    CacheJson(#[from] serde_json::Error),
    #[error("this profile does not have an API key")]
    MissingApiKey,
}

#[derive(Debug, Deserialize)]
struct ModelsResponse {
    data: Vec<ModelRecord>,
}

#[derive(Debug, Deserialize)]
struct ModelRecord {
    id: String,
}

pub fn fetch_models(profile: &Profile) -> Result<ModelCache, ModelError> {
    let endpoint = models_endpoint(&profile.base_url)?;
    let client = openai_http::client(Duration::from_secs(15))?;
    let api_key = profile.api_key.as_ref().ok_or(ModelError::MissingApiKey)?;
    let response = client
        .get(endpoint)
        .bearer_auth(api_key.expose_secret())
        .header(ACCEPT, "application/json")
        .send()?;

    if !response.status().is_success() {
        return Err(ModelError::HttpStatus(response.status().as_u16()));
    }

    let bytes =
        openai_http::read_limited(response, openai_http::MAX_RESPONSE_BYTES).map_err(|error| {
            match error {
                ResponseReadError::TooLarge => ModelError::ResponseTooLarge,
                ResponseReadError::Io => ModelError::InvalidResponse,
            }
        })?;

    let models = parse_model_response(&bytes)?;
    Ok(ModelCache {
        schema_version: 1,
        profile_id: profile.id,
        fetched_at_ms: unix_time_ms(),
        models,
    })
}

pub fn load_cache(
    cache_dir: &Path,
    profile_id: ProfileId,
) -> Result<Option<ModelCache>, ModelError> {
    let path = cache_path(cache_dir, profile_id);
    let Some(bytes) = durable_fs::read_optional(&path)? else {
        return Ok(None);
    };
    let cache: ModelCache = serde_json::from_slice(&bytes)?;
    if cache.schema_version != 1 || cache.profile_id != profile_id {
        return Err(ModelError::InvalidResponse);
    }
    Ok(Some(cache))
}

pub fn save_cache(cache_dir: &Path, cache: &ModelCache) -> Result<(), ModelError> {
    durable_fs::ensure_private_dir(cache_dir)?;
    let bytes = serde_json::to_vec_pretty(cache)?;
    durable_fs::atomic_write(&cache_path(cache_dir, cache.profile_id), &bytes)?;
    Ok(())
}

pub fn remove_cache(cache_dir: &Path, profile_id: ProfileId) -> Result<(), ModelError> {
    durable_fs::atomic_remove(&cache_path(cache_dir, profile_id))?;
    Ok(())
}

pub fn cache_path(cache_dir: &Path, profile_id: ProfileId) -> PathBuf {
    cache_dir.join(format!("{profile_id}.json"))
}

pub fn models_endpoint(base_url: &str) -> Result<Url, ModelError> {
    openai_http::endpoint(base_url, "models").map_err(|_| ModelError::InvalidBaseUrl)
}

fn parse_model_response(bytes: &[u8]) -> Result<Vec<String>, ModelError> {
    let response: ModelsResponse =
        serde_json::from_slice(bytes).map_err(|_| ModelError::InvalidResponse)?;
    if response.data.len() > MAX_MODELS {
        return Err(ModelError::InvalidResponse);
    }

    let models: BTreeSet<String> = response
        .data
        .into_iter()
        .map(|record| record.id.trim().to_owned())
        .filter(|id| !id.is_empty() && id.len() <= MAX_MODEL_ID_BYTES)
        .collect();
    if models.is_empty() {
        return Err(ModelError::InvalidResponse);
    }
    Ok(models.into_iter().collect())
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ApiKey;

    #[test]
    fn appends_models_to_versioned_base_path() {
        assert_eq!(
            models_endpoint("https://relay.example/v1")
                .unwrap()
                .as_str(),
            "https://relay.example/v1/models"
        );
    }

    #[test]
    fn rejects_credentials_and_query_parameters() {
        assert!(models_endpoint("https://user:secret@example.test/v1").is_err());
        assert!(models_endpoint("https://example.test/v1?token=bad").is_err());
    }

    #[test]
    fn parses_deduplicated_sorted_model_ids() {
        let models =
            parse_model_response(br#"{"data":[{"id":"zeta"},{"id":" alpha "},{"id":"zeta"}]}"#)
                .unwrap();
        assert_eq!(models, vec!["alpha", "zeta"]);
    }

    #[test]
    fn fetches_standard_models_endpoint_with_bearer_auth() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr().to_ip().unwrap();
        let handle = std::thread::spawn(move || {
            let request = server.recv().unwrap();
            assert_eq!(request.url(), "/v1/models");
            let authorization = request
                .headers()
                .iter()
                .find(|header| header.field.equiv("Authorization"))
                .map(|header| header.value.as_str());
            assert_eq!(authorization, Some("Bearer sk-model-test"));
            request
                .respond(
                    tiny_http::Response::from_string(
                        r#"{"data":[{"id":"model-b"},{"id":"model-a"}]}"#,
                    )
                    .with_header(
                        tiny_http::Header::from_bytes(b"Content-Type", b"application/json")
                            .unwrap(),
                    ),
                )
                .unwrap();
        });
        let profile = Profile::new(
            "Local",
            format!("http://{address}/v1"),
            ApiKey::new("sk-model-test").unwrap(),
            "model-a",
            None,
        )
        .unwrap();

        let cache = fetch_models(&profile).unwrap();

        handle.join().unwrap();
        assert_eq!(cache.profile_id, profile.id);
        assert_eq!(cache.models, vec!["model-a", "model-b"]);
    }
}
