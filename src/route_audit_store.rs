use std::collections::HashSet;
use std::fmt::{self, Write as _};
use std::path::PathBuf;

use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use thiserror::Error;
use uuid::Uuid;

use crate::domain::{Profile, ProfileId};
use crate::durable_fs::{self, BoundedRead, DurableFsError};

const SCHEMA_VERSION: u32 = 1;
const MAX_RESULTS: usize = 2_000;
const MAX_FILE_BYTES: u64 = 256 * 1024;
const REVISION_KEY_BYTES: usize = 32;
const REVISION_KEY_ID_DOMAIN: &[u8] = b"codex-switch-route-audit-key-id-v1";
pub const STALE_AFTER_MS: u64 = 24 * 60 * 60 * 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteAuditResultKind {
    Success,
    Error,
    Incomplete,
    Stopped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteAuditErrorCategory {
    ModelRequestFailed,
    MissingBaseUrl,
    MissingApiKey,
    MissingModel,
    MissingMultipleFields,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteAuditRecord {
    pub profile_id: ProfileId,
    pub profile_revision: String,
    pub result: RouteAuditResultKind,
    pub model_count: Option<u32>,
    pub model_check_duration_ms: Option<u64>,
    pub checked_at_unix_ms: u64,
    pub error_category: Option<RouteAuditErrorCategory>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteAuditSnapshot {
    pub revision_key_id: String,
    pub results: Vec<RouteAuditRecord>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RouteAuditLoad {
    Missing,
    Ready(RouteAuditSnapshot),
    Corrupt,
    FutureSchema,
}

#[derive(Clone)]
pub struct RouteAuditRevisionKey([u8; REVISION_KEY_BYTES]);

impl fmt::Debug for RouteAuditRevisionKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RouteAuditRevisionKey([REDACTED])")
    }
}

#[derive(Debug)]
pub struct RouteAuditStore {
    path: PathBuf,
    revision_key_path: PathBuf,
}

impl RouteAuditStore {
    pub fn new(path: impl Into<PathBuf>, revision_key_path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            revision_key_path: revision_key_path.into(),
        }
    }

    pub fn load(&self) -> Result<RouteAuditLoad, RouteAuditStoreError> {
        let bytes = match durable_fs::read_optional_bounded(&self.path, MAX_FILE_BYTES) {
            Ok(BoundedRead::Missing) => return Ok(RouteAuditLoad::Missing),
            Ok(BoundedRead::TooLarge) | Err(DurableFsError::UnsafeTarget(_)) => {
                return Ok(RouteAuditLoad::Corrupt);
            }
            Ok(BoundedRead::Contents(bytes)) => bytes,
            Err(error) => return Err(error.into()),
        };
        let value: serde_json::Value = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(_) => return Ok(RouteAuditLoad::Corrupt),
        };
        let Some(schema_version) = value.get("schema_version").and_then(|value| value.as_u64())
        else {
            return Ok(RouteAuditLoad::Corrupt);
        };
        if schema_version > u64::from(SCHEMA_VERSION) {
            return Ok(RouteAuditLoad::FutureSchema);
        }
        if schema_version != u64::from(SCHEMA_VERSION) {
            return Ok(RouteAuditLoad::Corrupt);
        }
        let document: RouteAuditDocument = match serde_json::from_value(value) {
            Ok(document) => document,
            Err(_) => return Ok(RouteAuditLoad::Corrupt),
        };
        if document.validate().is_err() {
            return Ok(RouteAuditLoad::Corrupt);
        }
        Ok(RouteAuditLoad::Ready(RouteAuditSnapshot {
            revision_key_id: document.revision_key_id,
            results: document.results,
        }))
    }

    pub fn save(
        &self,
        results: Vec<RouteAuditRecord>,
        revision_key: &RouteAuditRevisionKey,
    ) -> Result<(), RouteAuditStoreError> {
        if matches!(self.load()?, RouteAuditLoad::FutureSchema) {
            return Err(RouteAuditStoreError::FutureSchema);
        }
        let document = RouteAuditDocument {
            schema_version: SCHEMA_VERSION,
            revision_key_id: revision_key_id(revision_key),
            results,
        };
        document.validate()?;
        let mut bytes = serde_json::to_vec_pretty(&document)?;
        bytes.push(b'\n');
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(RouteAuditStoreError::TooLarge);
        }
        durable_fs::atomic_write(&self.path, &bytes)?;
        Ok(())
    }

    pub fn clear(&self) -> Result<(), RouteAuditStoreError> {
        durable_fs::atomic_remove(&self.path)?;
        Ok(())
    }

    pub fn load_revision_key(&self) -> Result<Option<RouteAuditRevisionKey>, RouteAuditStoreError> {
        let bytes = match durable_fs::read_optional_bounded(
            &self.revision_key_path,
            REVISION_KEY_BYTES as u64,
        ) {
            Ok(BoundedRead::Missing | BoundedRead::TooLarge)
            | Err(DurableFsError::UnsafeTarget(_)) => return Ok(None),
            Ok(BoundedRead::Contents(bytes)) => bytes,
            Err(error) => return Err(error.into()),
        };
        let Ok(key) = <[u8; REVISION_KEY_BYTES]>::try_from(bytes.as_slice()) else {
            return Ok(None);
        };
        Ok(Some(RouteAuditRevisionKey(key)))
    }

    pub fn ensure_revision_key(&self) -> Result<RouteAuditRevisionKey, RouteAuditStoreError> {
        if let Some(key) = self.load_revision_key()? {
            return Ok(key);
        }
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let mut bytes = [0_u8; REVISION_KEY_BYTES];
        bytes[..16].copy_from_slice(first.as_bytes());
        bytes[16..].copy_from_slice(second.as_bytes());
        durable_fs::atomic_write(&self.revision_key_path, &bytes)?;
        Ok(RouteAuditRevisionKey(bytes))
    }
}

pub fn profile_revision(
    key: &RouteAuditRevisionKey,
    profile: &Profile,
) -> Result<String, RouteAuditStoreError> {
    let projection = ProfileRevisionProjection {
        base_url: &profile.base_url,
        api_key: profile.api_key.as_ref().map(|key| key.expose_secret()),
        model: &profile.model,
        review_model: profile.review_model.as_deref(),
    };
    let bytes = serde_json::to_vec(&projection)?;
    Ok(hmac_hex(key, &bytes))
}

pub fn revision_key_id(key: &RouteAuditRevisionKey) -> String {
    hmac_hex(key, REVISION_KEY_ID_DOMAIN)
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RouteAuditDocument {
    schema_version: u32,
    revision_key_id: String,
    #[serde(default)]
    results: Vec<RouteAuditRecord>,
}

impl RouteAuditDocument {
    fn validate(&self) -> Result<(), RouteAuditStoreError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(RouteAuditStoreError::UnsupportedSchema);
        }
        if !is_sha256_hex(&self.revision_key_id) {
            return Err(RouteAuditStoreError::InvalidRecord);
        }
        if self.results.len() > MAX_RESULTS {
            return Err(RouteAuditStoreError::TooManyResults);
        }
        let mut profile_ids = HashSet::with_capacity(self.results.len());
        for record in &self.results {
            if !profile_ids.insert(record.profile_id) {
                return Err(RouteAuditStoreError::DuplicateProfile);
            }
            if !is_sha256_hex(&record.profile_revision) {
                return Err(RouteAuditStoreError::InvalidRecord);
            }
            let valid_shape = match record.result {
                RouteAuditResultKind::Success => {
                    record.model_count.is_some()
                        && record.model_check_duration_ms.is_some()
                        && record.error_category.is_none()
                }
                RouteAuditResultKind::Error => {
                    record.model_count.is_none() && record.error_category.is_some()
                }
                RouteAuditResultKind::Incomplete => {
                    record.model_count.is_none()
                        && record.model_check_duration_ms.is_none()
                        && record.error_category.is_some()
                }
                RouteAuditResultKind::Stopped => {
                    record.model_count.is_none()
                        && record.model_check_duration_ms.is_none()
                        && record.error_category.is_none()
                }
            };
            if !valid_shape {
                return Err(RouteAuditStoreError::InvalidRecord);
            }
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct ProfileRevisionProjection<'a> {
    base_url: &'a str,
    api_key: Option<&'a str>,
    model: &'a str,
    review_model: Option<&'a str>,
}

fn hmac_hex(key: &RouteAuditRevisionKey, contents: &[u8]) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(&key.0).expect("HMAC-SHA256 accepts a key of any length");
    mac.update(contents);
    let digest = mac.finalize().into_bytes();
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(output, "{byte:02x}").expect("writing to a string cannot fail");
    }
    output
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Debug, Error)]
pub enum RouteAuditStoreError {
    #[error(transparent)]
    FileSystem(#[from] DurableFsError),
    #[error("route audit results could not be serialized")]
    Serialize(#[from] serde_json::Error),
    #[error("route audit results use a newer schema")]
    FutureSchema,
    #[error("route audit results use an unsupported schema")]
    UnsupportedSchema,
    #[error("route audit results contain too many entries")]
    TooManyResults,
    #[error("route audit results contain a duplicate profile")]
    DuplicateProfile,
    #[error("route audit result is invalid")]
    InvalidRecord,
    #[error("route audit results exceed 256 KiB")]
    TooLarge,
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::domain::{ApiKey, AutoCompactScope, ProfileContext};

    fn record(profile: &Profile, key: &RouteAuditRevisionKey) -> RouteAuditRecord {
        RouteAuditRecord {
            profile_id: profile.id,
            profile_revision: profile_revision(key, profile).unwrap(),
            result: RouteAuditResultKind::Success,
            model_count: Some(3),
            model_check_duration_ms: Some(25),
            checked_at_unix_ms: 1_725_600_000_000,
            error_category: None,
        }
    }

    fn profile() -> Profile {
        Profile::new(
            "Secret display name",
            "https://secret-route.example/v1",
            ApiKey::new("sk-route-audit-secret").unwrap(),
            "secret-model-name",
            Some("secret-review-model".to_owned()),
        )
        .unwrap()
    }

    fn store(temp: &tempfile::TempDir) -> RouteAuditStore {
        RouteAuditStore::new(
            temp.path().join("route-audit.json"),
            temp.path().join(".route-audit-key"),
        )
    }

    #[test]
    fn saves_and_loads_only_keyed_redacted_route_audit_records() {
        let temp = tempfile::tempdir().unwrap();
        let store = store(&temp);
        let key = store.ensure_revision_key().unwrap();
        let profile = profile();
        let expected = vec![record(&profile, &key)];

        store.save(expected.clone(), &key).unwrap();

        let RouteAuditLoad::Ready(snapshot) = store.load().unwrap() else {
            panic!("expected saved route audit results");
        };
        assert_eq!(snapshot.results, expected);
        assert_eq!(snapshot.revision_key_id, revision_key_id(&key));
        let raw = fs::read_to_string(temp.path().join("route-audit.json")).unwrap();
        for secret in [
            "secret-route.example",
            "sk-route-audit-secret",
            "secret-model-name",
            "secret-review-model",
            "Secret display name",
        ] {
            assert!(!raw.contains(secret));
        }
        assert_eq!(
            fs::read(temp.path().join(".route-audit-key"))
                .unwrap()
                .len(),
            REVISION_KEY_BYTES
        );
        assert_eq!(format!("{key:?}"), "RouteAuditRevisionKey([REDACTED])");
    }

    #[test]
    fn profile_revision_tracks_only_route_and_model_catalog_fields() {
        let temp = tempfile::tempdir().unwrap();
        let key = store(&temp).ensure_revision_key().unwrap();
        let original = profile();
        let revision = profile_revision(&key, &original).unwrap();
        let mut cosmetic = original.clone();
        cosmetic.name = "Renamed".to_owned();
        cosmetic.context = Some(ProfileContext {
            model_context_window: Some(272_000),
            model_auto_compact_token_limit: Some(217_600),
            model_auto_compact_token_limit_scope: Some(AutoCompactScope::Total),
        });
        assert_eq!(profile_revision(&key, &cosmetic).unwrap(), revision);

        for changed in [
            {
                let mut profile = original.clone();
                profile.base_url = "https://changed.example/v1".to_owned();
                profile
            },
            {
                let mut profile = original.clone();
                profile.api_key = Some(ApiKey::new("sk-changed").unwrap());
                profile
            },
            {
                let mut profile = original.clone();
                profile.model = "changed-model".to_owned();
                profile
            },
            {
                let mut profile = original.clone();
                profile.review_model = Some("changed-review".to_owned());
                profile
            },
        ] {
            assert_ne!(profile_revision(&key, &changed).unwrap(), revision);
        }
    }

    #[test]
    fn missing_or_rotated_revision_keys_are_detectable() {
        let temp = tempfile::tempdir().unwrap();
        let store = store(&temp);
        let original = store.ensure_revision_key().unwrap();
        let original_id = revision_key_id(&original);
        fs::remove_file(temp.path().join(".route-audit-key")).unwrap();

        assert!(store.load_revision_key().unwrap().is_none());
        let replacement = store.ensure_revision_key().unwrap();
        assert_ne!(revision_key_id(&replacement), original_id);
    }

    #[test]
    fn corrupt_files_rebuild_but_future_schema_is_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let store = store(&temp);
        let path = temp.path().join("route-audit.json");
        let key = store.ensure_revision_key().unwrap();

        fs::write(&path, b"not json").unwrap();
        assert_eq!(store.load().unwrap(), RouteAuditLoad::Corrupt);
        store.save(vec![record(&profile(), &key)], &key).unwrap();
        assert!(matches!(store.load().unwrap(), RouteAuditLoad::Ready(_)));

        fs::write(
            &path,
            format!(
                r#"{{"schema_version":99,"revision_key_id":"{}","results":[]}}"#,
                revision_key_id(&key)
            ),
        )
        .unwrap();
        assert_eq!(store.load().unwrap(), RouteAuditLoad::FutureSchema);
        assert!(matches!(
            store.save(Vec::new(), &key),
            Err(RouteAuditStoreError::FutureSchema)
        ));

        fs::write(&path, vec![b'x'; MAX_FILE_BYTES as usize + 1]).unwrap();
        assert_eq!(store.load().unwrap(), RouteAuditLoad::Corrupt);
    }

    #[test]
    fn clear_removes_results_without_rotating_the_revision_key() {
        let temp = tempfile::tempdir().unwrap();
        let store = store(&temp);
        let key = store.ensure_revision_key().unwrap();
        let key_id = revision_key_id(&key);
        store.save(vec![record(&profile(), &key)], &key).unwrap();

        store.clear().unwrap();

        assert_eq!(store.load().unwrap(), RouteAuditLoad::Missing);
        assert_eq!(
            revision_key_id(&store.load_revision_key().unwrap().unwrap()),
            key_id
        );
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_history_and_key_symlinks_are_never_followed() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside");
        fs::write(&outside, b"secret outside contents").unwrap();
        let history = temp.path().join("route-audit.json");
        let key = temp.path().join(".route-audit-key");
        symlink(&outside, &history).unwrap();
        symlink(&outside, &key).unwrap();
        let store = RouteAuditStore::new(&history, &key);

        assert_eq!(store.load().unwrap(), RouteAuditLoad::Corrupt);
        assert!(store.load_revision_key().unwrap().is_none());
        assert!(store.ensure_revision_key().is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"secret outside contents");
    }
}
