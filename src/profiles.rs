use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::str;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::{DomainError, Profile, ProfileId};
use crate::durable_fs::{self, DurableFsError};

pub const PROFILES_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfilesDocument {
    pub schema_version: u32,
    #[serde(default)]
    pub profiles: Vec<Profile>,
}

impl Default for ProfilesDocument {
    fn default() -> Self {
        Self {
            schema_version: PROFILES_SCHEMA_VERSION,
            profiles: Vec::new(),
        }
    }
}

impl ProfilesDocument {
    pub fn validate(&self) -> Result<(), ProfileStoreError> {
        if self.schema_version != PROFILES_SCHEMA_VERSION {
            return Err(ProfileStoreError::UnsupportedSchema {
                found: self.schema_version,
                supported: PROFILES_SCHEMA_VERSION,
            });
        }

        let mut ids = HashSet::with_capacity(self.profiles.len());
        let mut names = HashSet::with_capacity(self.profiles.len());
        for profile in &self.profiles {
            profile
                .validate()
                .map_err(|source| ProfileStoreError::InvalidProfile {
                    id: profile.id,
                    source,
                })?;
            if !ids.insert(profile.id) {
                return Err(ProfileStoreError::DuplicateId(profile.id));
            }
            let normalized_name = profile.name.trim().to_lowercase();
            if !names.insert(normalized_name) {
                return Err(ProfileStoreError::DuplicateName(profile.name.clone()));
            }
        }
        Ok(())
    }

    pub fn get(&self, id: ProfileId) -> Option<&Profile> {
        self.profiles.iter().find(|profile| profile.id == id)
    }

    pub fn get_mut(&mut self, id: ProfileId) -> Option<&mut Profile> {
        self.profiles.iter_mut().find(|profile| profile.id == id)
    }

    pub fn insert(&mut self, profile: Profile) -> Result<(), ProfileStoreError> {
        profile
            .validate()
            .map_err(|source| ProfileStoreError::InvalidProfile {
                id: profile.id,
                source,
            })?;
        if self.get(profile.id).is_some() {
            return Err(ProfileStoreError::DuplicateId(profile.id));
        }
        let normalized_name = profile.name.trim().to_lowercase();
        if self
            .profiles
            .iter()
            .any(|existing| existing.name.trim().to_lowercase() == normalized_name)
        {
            return Err(ProfileStoreError::DuplicateName(profile.name));
        }
        self.profiles.push(profile);
        Ok(())
    }

    pub fn remove(&mut self, id: ProfileId) -> Option<Profile> {
        let index = self.profiles.iter().position(|profile| profile.id == id)?;
        Some(self.profiles.remove(index))
    }
}

#[derive(Clone, Debug)]
pub struct ProfileStore {
    path: PathBuf,
}

impl ProfileStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<ProfilesDocument, ProfileStoreError> {
        match durable_fs::read_optional(&self.path)? {
            Some(bytes) => Self::deserialize(&bytes),
            None => Ok(ProfilesDocument::default()),
        }
    }

    pub fn deserialize(bytes: &[u8]) -> Result<ProfilesDocument, ProfileStoreError> {
        let text = str::from_utf8(bytes).map_err(ProfileStoreError::InvalidUtf8)?;
        let document: ProfilesDocument = toml::from_str(text).map_err(|mut source| {
            // TOML errors retain and render their full input unless it is explicitly removed.
            source.set_input(None);
            ProfileStoreError::Parse(source)
        })?;
        document.validate()?;
        Ok(document)
    }

    pub fn serialize(document: &ProfilesDocument) -> Result<Vec<u8>, ProfileStoreError> {
        document.validate()?;
        let mut text = toml::to_string_pretty(document)?;
        if !text.ends_with('\n') {
            text.push('\n');
        }
        Ok(text.into_bytes())
    }

    pub fn save(&self, document: &ProfilesDocument) -> Result<(), ProfileStoreError> {
        let bytes = Self::serialize(document)?;
        durable_fs::atomic_write(&self.path, &bytes)?;
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum ProfileStoreError {
    #[error(transparent)]
    FileSystem(#[from] DurableFsError),
    #[error("profiles file is not valid UTF-8")]
    InvalidUtf8(#[source] str::Utf8Error),
    #[error("profiles TOML is invalid: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("profiles could not be serialized: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("profiles schema version {found} is not supported; expected {supported}")]
    UnsupportedSchema { found: u32, supported: u32 },
    #[error("profile {id} is invalid: {source}")]
    InvalidProfile {
        id: ProfileId,
        #[source]
        source: DomainError,
    },
    #[error("profile ID {0} appears more than once")]
    DuplicateId(ProfileId),
    #[error("profile name {0:?} appears more than once")]
    DuplicateName(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ApiKey;

    fn profile(name: &str) -> Profile {
        Profile::new(
            name,
            "https://relay.example/v1",
            ApiKey::new("sk-plain-text").unwrap(),
            "gpt-5.2-codex",
            Some("gpt-5.2".to_owned()),
        )
        .unwrap()
    }

    #[test]
    fn serializes_plaintext_key_and_round_trips() {
        let document = ProfilesDocument {
            schema_version: PROFILES_SCHEMA_VERSION,
            profiles: vec![profile("Relay A")],
        };

        let bytes = ProfileStore::serialize(&document).unwrap();
        let text = str::from_utf8(&bytes).unwrap();
        assert!(text.contains("api_key = \"sk-plain-text\""));
        assert_eq!(ProfileStore::deserialize(&bytes).unwrap(), document);
        assert!(!format!("{document:?}").contains("sk-plain-text"));
    }

    #[test]
    fn rejects_unknown_schema_and_duplicate_names() {
        let mut document = ProfilesDocument {
            schema_version: 99,
            profiles: Vec::new(),
        };
        assert!(matches!(
            document.validate(),
            Err(ProfileStoreError::UnsupportedSchema { found: 99, .. })
        ));

        document.schema_version = PROFILES_SCHEMA_VERSION;
        document.profiles = vec![profile("Relay A"), profile("relay a")];
        assert!(matches!(
            document.validate(),
            Err(ProfileStoreError::DuplicateName(_))
        ));
    }

    #[test]
    fn load_missing_file_returns_empty_document_and_save_is_atomic() {
        let temp = tempfile::tempdir().unwrap();
        let store = ProfileStore::new(temp.path().join("nested/profiles.toml"));
        assert_eq!(store.load().unwrap(), ProfilesDocument::default());

        let document = ProfilesDocument {
            schema_version: PROFILES_SCHEMA_VERSION,
            profiles: vec![profile("Relay A")],
        };
        store.save(&document).unwrap();
        assert_eq!(store.load().unwrap(), document);
    }

    #[test]
    fn parse_errors_do_not_retain_or_render_profile_secrets() {
        const SECRET: &str = "sk-must-never-appear-in-an-error";
        let malformed =
            format!("schema_version = 1\n\n[[profiles]]\napi_key = \"{SECRET}\" unexpected\n");

        let error = ProfileStore::deserialize(malformed.as_bytes()).unwrap_err();

        assert!(matches!(error, ProfileStoreError::Parse(_)));
        assert!(!format!("{error}").contains(SECRET));
        assert!(!format!("{error:?}").contains(SECRET));
    }
}
