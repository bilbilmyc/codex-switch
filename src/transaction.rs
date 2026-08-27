#[cfg(test)]
use std::cell::Cell;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::str;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::codex_config::{
    CodexConfigError, ContextSettings, RelevantProjection, TOOL_PROVIDER_ID, inspect_codex_config,
    patch_auth_json, patch_codex_config, patch_context_settings, pre_context_relevant_fingerprint,
    relevant_fingerprint, relevant_projection,
};
use crate::domain::{Activation, ProfileId};
use crate::durable_fs::{self, DurableFsError};
use crate::legacy_usage::{LegacyUsageHistory, LegacyUsageObservation};
use crate::paths::AppPaths;

const TRANSACTION_SCHEMA_VERSION: u32 = 1;
const STATE_SCHEMA_VERSION: u32 = 1;
const BACKUP_SCHEMA_VERSION: u32 = 1;
const MAX_BACKUPS: usize = 10;
const BACKUP_MANIFEST_FILE: &str = "manifest.json";
const BACKUP_CONFIG_FILE: &str = "config.toml";
const BACKUP_AUTH_FILE: &str = "auth.json";
const BACKUP_STATE_FILE: &str = "state.json";
const BACKUP_STAGING_PREFIX: &str = ".staging-";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BackupId(Uuid);

impl BackupId {
    pub fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl std::fmt::Display for BackupId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedState {
    pub schema_version: u32,
    pub active_profile_id: Option<ProfileId>,
    pub relevant_fingerprint: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupSummary {
    pub id: BackupId,
    pub created_at_unix_ms: u64,
    pub config_present: bool,
    pub auth_present: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictPolicy {
    Reject,
    Overwrite,
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("Codex relay/model fields changed outside Codex Switch")]
pub struct ExternalConflict {
    pub expected_fingerprint: String,
    pub actual_fingerprint: String,
    pub actual_projection: RelevantProjection,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryOutcome {
    None,
    RolledBack {
        transaction_id: Uuid,
        backup_id: BackupId,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyOutcome {
    pub backup: BackupSummary,
    pub state: ManagedState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreOutcome {
    pub restored: BackupSummary,
    pub rollback_backup: BackupSummary,
    pub state: ManagedState,
}

pub trait StagedValidator {
    fn validate(&self, config_toml: &str, auth_json: &[u8]) -> Result<(), String>;
}

#[derive(Debug)]
pub struct TransactionManager {
    paths: AppPaths,
    #[cfg(test)]
    failure_point: Cell<Option<TestFailurePoint>>,
    #[cfg(test)]
    mutation_point: Cell<Option<TestMutationPoint>>,
}

impl TransactionManager {
    pub fn new(paths: AppPaths) -> Self {
        Self {
            paths,
            #[cfg(test)]
            failure_point: Cell::new(None),
            #[cfg(test)]
            mutation_point: Cell::new(None),
        }
    }

    pub fn recover_if_needed(&self) -> Result<RecoveryOutcome, TransactionError> {
        let _lock = durable_fs::acquire_lock(&self.paths.lock)?;
        self.recover_locked()
    }

    pub fn load_state(&self) -> Result<Option<ManagedState>, TransactionError> {
        let _lock = durable_fs::acquire_lock(&self.paths.lock)?;
        self.recover_locked()?;
        self.load_state_locked()
    }

    pub fn adopt_current(
        &self,
        active_profile_id: Option<ProfileId>,
    ) -> Result<ManagedState, TransactionError> {
        let _lock = durable_fs::acquire_lock(&self.paths.lock)?;
        self.recover_locked()?;

        let current = self.read_live_snapshot()?;
        let expected_live_revisions = LiveRevisions::from_snapshot(&current);
        let current_config = config_text(&self.paths.codex_config, current.config.as_deref())?;
        let state = ManagedState {
            schema_version: STATE_SCHEMA_VERSION,
            active_profile_id,
            relevant_fingerprint: relevant_fingerprint(current_config, current.auth.as_deref())?,
        };
        self.maybe_mutate_live(TestMutationPoint::BeforeAdoptRevisionCheck)?;
        self.ensure_live_revisions(&expected_live_revisions)?;
        durable_fs::atomic_write(&self.paths.state, &serialize_json(&state)?)?;
        Ok(state)
    }

    pub fn apply(
        &self,
        activation: &Activation,
        policy: ConflictPolicy,
    ) -> Result<ApplyOutcome, TransactionError> {
        self.apply_validated(activation, policy, None)
    }

    pub fn apply_validated(
        &self,
        activation: &Activation,
        policy: ConflictPolicy,
        validator: Option<&dyn StagedValidator>,
    ) -> Result<ApplyOutcome, TransactionError> {
        let _lock = durable_fs::acquire_lock(&self.paths.lock)?;
        self.recover_locked()?;

        let current = self.read_live_snapshot()?;
        let current_config = config_text(&self.paths.codex_config, current.config.as_deref())?;
        let actual_projection = relevant_projection(current_config, current.auth.as_deref())?;
        let actual_fingerprint = relevant_fingerprint(current_config, current.auth.as_deref())?;

        if policy == ConflictPolicy::Reject
            && let Some(state) = self.load_state_locked()?
            && !state_fingerprint_matches(&state, &actual_fingerprint, &actual_projection)?
        {
            return Err(TransactionError::ExternalConflict(Box::new(
                ExternalConflict {
                    expected_fingerprint: state.relevant_fingerprint,
                    actual_fingerprint,
                    actual_projection,
                },
            )));
        }

        let patched_config = patch_codex_config(current_config, activation)?;
        let patched_auth = patch_auth_json(current.auth.as_deref(), &activation.api_key)?;
        let target_fingerprint =
            relevant_fingerprint(&patched_config.contents, Some(patched_auth.as_slice()))?;

        if let Some(validator) = validator {
            validator
                .validate(&patched_config.contents, &patched_auth)
                .map_err(TransactionError::StagedValidation)?;
        }

        let state = ManagedState {
            schema_version: STATE_SCHEMA_VERSION,
            active_profile_id: Some(activation.profile_id),
            relevant_fingerprint: target_fingerprint,
        };
        let target = Snapshot {
            config: Some(patched_config.contents.into_bytes()),
            auth: Some(patched_auth),
            state: Some(serialize_json(&state)?),
        };
        let backup = self.commit_snapshot(
            TransactionOperation::Apply {
                profile_id: activation.profile_id,
            },
            &current,
            &target,
        )?;

        Ok(ApplyOutcome { backup, state })
    }

    pub fn update_context(
        &self,
        settings: ContextSettings,
    ) -> Result<BackupSummary, TransactionError> {
        self.update_context_validated(settings, None)
    }

    pub fn update_context_validated(
        &self,
        settings: ContextSettings,
        validator: Option<&dyn StagedValidator>,
    ) -> Result<BackupSummary, TransactionError> {
        let _lock = durable_fs::acquire_lock(&self.paths.lock)?;
        self.recover_locked()?;

        let current = self.read_live_snapshot()?;
        let current_config = config_text(&self.paths.codex_config, current.config.as_deref())?;
        let actual_projection = relevant_projection(current_config, current.auth.as_deref())?;
        let actual_fingerprint = relevant_fingerprint(current_config, current.auth.as_deref())?;
        let current_state = deserialize_state(current.state.as_deref())?;
        if let Some(state) = &current_state
            && !state_fingerprint_matches(state, &actual_fingerprint, &actual_projection)?
        {
            return Err(TransactionError::ExternalConflict(Box::new(
                ExternalConflict {
                    expected_fingerprint: state.relevant_fingerprint.clone(),
                    actual_fingerprint,
                    actual_projection,
                },
            )));
        }

        let patched_config = patch_context_settings(current_config, settings)?;
        if let (Some(validator), Some(auth)) = (validator, current.auth.as_deref()) {
            validator
                .validate(&patched_config, auth)
                .map_err(TransactionError::StagedValidation)?;
        }
        let target_state = current_state
            .map(|mut state| {
                state.relevant_fingerprint =
                    relevant_fingerprint(&patched_config, current.auth.as_deref())?;
                serialize_json(&state)
            })
            .transpose()?;
        let target = Snapshot {
            config: Some(patched_config.into_bytes()),
            auth: current.auth.clone(),
            state: target_state,
        };
        self.commit_snapshot(TransactionOperation::UpdateContext, &current, &target)
    }

    pub fn has_backup(&self) -> Result<bool, TransactionError> {
        let _lock = durable_fs::acquire_lock(&self.paths.lock)?;
        self.recover_locked()?;
        Ok(!self.list_backups_locked()?.is_empty())
    }

    pub fn list_backups(&self) -> Result<Vec<BackupSummary>, TransactionError> {
        let _lock = durable_fs::acquire_lock(&self.paths.lock)?;
        self.recover_locked()?;
        self.list_backups_locked()
    }

    /// Returns only the validated, non-sensitive information needed to recover ownership of
    /// usage written before profiles received independent provider IDs.
    pub fn legacy_usage_history(&self) -> Result<LegacyUsageHistory, TransactionError> {
        let _lock = durable_fs::acquire_lock(&self.paths.lock)?;
        self.recover_locked()?;

        let mut summaries = self.list_backups_locked()?;
        summaries.sort_by(|left, right| {
            left.created_at_unix_ms
                .cmp(&right.created_at_unix_ms)
                .then_with(|| left.id.to_string().cmp(&right.id.to_string()))
        });
        let mut backups = Vec::with_capacity(summaries.len());
        for summary in summaries {
            let snapshot = self.read_backup(summary.id)?;
            let legacy_profile_id = self.legacy_profile_id_for_snapshot(
                &self.backup_dir(summary.id).join(BACKUP_CONFIG_FILE),
                &snapshot,
            )?;
            backups.push(LegacyUsageObservation {
                captured_at_unix_ms: summary.created_at_unix_ms,
                legacy_profile_id,
            });
        }

        let live = self.read_live_snapshot()?;
        let legacy_profile_id =
            self.legacy_profile_id_for_snapshot(&self.paths.codex_config, &live)?;
        Ok(LegacyUsageHistory {
            backups,
            live: LegacyUsageObservation {
                captured_at_unix_ms: unix_time_ms()?,
                legacy_profile_id,
            },
        })
    }

    pub fn restore_latest(&self) -> Result<RestoreOutcome, TransactionError> {
        let _lock = durable_fs::acquire_lock(&self.paths.lock)?;
        self.recover_locked()?;

        let restored = self
            .list_backups_locked()?
            .into_iter()
            .next()
            .ok_or(TransactionError::NoBackup)?;
        let source = self.read_backup(restored.id)?;
        let source_config = config_text(&self.paths.codex_config, source.config.as_deref())?;
        let source_projection = relevant_projection(source_config, source.auth.as_deref())?;
        let fingerprint = relevant_fingerprint(source_config, source.auth.as_deref())?;
        let source_state = deserialize_state(source.state.as_deref())?;
        let active_profile_id = if let Some(state) = source_state {
            if state_fingerprint_matches(&state, &fingerprint, &source_projection)? {
                state.active_profile_id
            } else {
                None
            }
        } else {
            None
        };
        let state = ManagedState {
            schema_version: STATE_SCHEMA_VERSION,
            active_profile_id,
            relevant_fingerprint: fingerprint,
        };
        let target = Snapshot {
            config: source.config,
            auth: source.auth,
            state: Some(serialize_json(&state)?),
        };
        let current = self.read_live_snapshot()?;
        let rollback_backup = self.commit_snapshot(
            TransactionOperation::Restore {
                source_backup_id: restored.id,
            },
            &current,
            &target,
        )?;

        Ok(RestoreOutcome {
            restored,
            rollback_backup,
            state,
        })
    }

    fn recover_locked(&self) -> Result<RecoveryOutcome, TransactionError> {
        self.cleanup_staging_backups()?;
        ensure_safe_file_target(&self.paths.journal)?;
        let Some(raw) = durable_fs::read_optional(&self.paths.journal)? else {
            return Ok(RecoveryOutcome::None);
        };
        let journal: TransactionJournal = serde_json::from_slice(&raw)
            .map_err(|source| metadata_error(&self.paths.journal, source))?;
        journal.validate()?;

        let snapshot = self.read_backup(journal.rollback_backup_id)?;
        self.restore_snapshot(&snapshot)?;
        durable_fs::atomic_remove(&self.paths.journal)?;

        Ok(RecoveryOutcome::RolledBack {
            transaction_id: journal.transaction_id,
            backup_id: journal.rollback_backup_id,
        })
    }

    fn load_state_locked(&self) -> Result<Option<ManagedState>, TransactionError> {
        ensure_safe_file_target(&self.paths.state)?;
        let raw = durable_fs::read_optional(&self.paths.state)?;
        deserialize_state(raw.as_deref())
    }

    fn commit_snapshot(
        &self,
        operation: TransactionOperation,
        current: &Snapshot,
        target: &Snapshot,
    ) -> Result<BackupSummary, TransactionError> {
        let expected_live_revisions = LiveRevisions::from_snapshot(current);
        self.ensure_live_revisions(&expected_live_revisions)?;
        self.prune_backups_to_limit(MAX_BACKUPS.saturating_sub(1), None)?;
        self.ensure_live_revisions(&expected_live_revisions)?;

        let staged_backup = self.stage_backup(current)?;
        if let Err(source) = self.maybe_mutate_live(TestMutationPoint::AfterBackupStaged) {
            return Err(self.abort_staged_backup(&staged_backup, source));
        }
        if let Err(source) = self.ensure_live_revisions(&expected_live_revisions) {
            return Err(self.abort_staged_backup(&staged_backup, source));
        }
        let rollback_backup = match self.finalize_backup(&staged_backup) {
            Ok(backup) => backup,
            Err(source) => return Err(self.abort_staged_backup(&staged_backup, source)),
        };
        if let Err(source) = self.ensure_live_revisions(&expected_live_revisions) {
            self.remove_backup(rollback_backup.id)?;
            return Err(source);
        }

        let journal = TransactionJournal {
            schema_version: TRANSACTION_SCHEMA_VERSION,
            transaction_id: Uuid::new_v4(),
            operation,
            rollback_backup_id: rollback_backup.id,
        };
        durable_fs::atomic_write(&self.paths.journal, &serialize_json(&journal)?)?;

        let commit_result = (|| {
            write_optional(&self.paths.codex_config, target.config.as_deref())?;
            self.maybe_fail(TestFailurePoint::Config)?;
            write_optional(&self.paths.codex_auth, target.auth.as_deref())?;
            self.maybe_fail(TestFailurePoint::Auth)?;
            write_optional(&self.paths.state, target.state.as_deref())?;
            self.maybe_fail(TestFailurePoint::State)?;
            durable_fs::atomic_remove(&self.paths.journal)?;
            Ok(())
        })();

        if let Err(source) = commit_result {
            let rollback_result = self
                .read_backup(rollback_backup.id)
                .and_then(|snapshot| self.restore_snapshot(&snapshot))
                .and_then(|()| {
                    durable_fs::atomic_remove(&self.paths.journal).map_err(TransactionError::from)
                });
            return match rollback_result {
                Ok(()) => Err(source),
                Err(rollback) => Err(TransactionError::RollbackFailed {
                    source: Box::new(source),
                    rollback: Box::new(rollback),
                }),
            };
        }

        Ok(rollback_backup)
    }

    fn read_live_snapshot(&self) -> Result<Snapshot, TransactionError> {
        ensure_safe_file_target(&self.paths.codex_config)?;
        ensure_safe_file_target(&self.paths.codex_auth)?;
        ensure_safe_file_target(&self.paths.state)?;
        Ok(Snapshot {
            config: durable_fs::read_optional(&self.paths.codex_config)?,
            auth: durable_fs::read_optional(&self.paths.codex_auth)?,
            state: durable_fs::read_optional(&self.paths.state)?,
        })
    }

    fn legacy_profile_id_for_snapshot(
        &self,
        config_path: &Path,
        snapshot: &Snapshot,
    ) -> Result<Option<ProfileId>, TransactionError> {
        let raw_config = config_text(config_path, snapshot.config.as_deref())?;
        let config = inspect_codex_config(raw_config)?;
        if config.model_provider.as_deref() != Some(TOOL_PROVIDER_ID) {
            return Ok(None);
        }
        let Some(state) = deserialize_state(snapshot.state.as_deref())? else {
            return Ok(None);
        };
        let projection = relevant_projection(raw_config, snapshot.auth.as_deref())?;
        let fingerprint = relevant_fingerprint(raw_config, snapshot.auth.as_deref())?;
        state_fingerprint_matches(&state, &fingerprint, &projection)
            .map(|matches| matches.then_some(state.active_profile_id).flatten())
    }

    fn ensure_live_revisions(&self, expected: &LiveRevisions) -> Result<(), TransactionError> {
        ensure_safe_file_target(&self.paths.codex_config)?;
        ensure_safe_file_target(&self.paths.codex_auth)?;
        let actual = LiveRevisions {
            config: durable_fs::revision(
                durable_fs::read_optional(&self.paths.codex_config)?.as_deref(),
            ),
            auth: durable_fs::revision(
                durable_fs::read_optional(&self.paths.codex_auth)?.as_deref(),
            ),
        };
        if &actual != expected {
            return Err(TransactionError::ConcurrentModification {
                expected_config_revision: expected.config.clone(),
                actual_config_revision: actual.config,
                expected_auth_revision: expected.auth.clone(),
                actual_auth_revision: actual.auth,
            });
        }
        Ok(())
    }

    fn restore_snapshot(&self, snapshot: &Snapshot) -> Result<(), TransactionError> {
        write_optional(&self.paths.codex_config, snapshot.config.as_deref())?;
        write_optional(&self.paths.codex_auth, snapshot.auth.as_deref())?;
        write_optional(&self.paths.state, snapshot.state.as_deref())?;
        Ok(())
    }

    #[cfg(test)]
    fn create_backup(&self, snapshot: &Snapshot) -> Result<BackupSummary, TransactionError> {
        self.ensure_backups_directory()?;
        self.prune_backups_to_limit(MAX_BACKUPS.saturating_sub(1), None)?;
        let staged = self.stage_backup(snapshot)?;
        match self.finalize_backup(&staged) {
            Ok(backup) => Ok(backup),
            Err(source) => Err(self.abort_staged_backup(&staged, source)),
        }
    }

    fn stage_backup(&self, snapshot: &Snapshot) -> Result<StagedBackup, TransactionError> {
        self.ensure_backups_directory()?;
        let (id, directory) = loop {
            let id = BackupId(Uuid::new_v4());
            let directory = self.staging_backup_dir(id);
            if path_is_missing(&directory)? && path_is_missing(&self.backup_dir(id))? {
                match fs::create_dir(&directory) {
                    Ok(()) => break (id, directory),
                    Err(source) if source.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(source) => return Err(io_error(&directory, source)),
                }
            }
        };
        durable_fs::ensure_private_dir(&directory)?;
        ensure_directory(&directory)?;

        let result = (|| {
            write_backup_file(
                &directory.join(BACKUP_CONFIG_FILE),
                snapshot.config.as_deref(),
            )?;
            write_backup_file(&directory.join(BACKUP_AUTH_FILE), snapshot.auth.as_deref())?;
            write_backup_file(
                &directory.join(BACKUP_STATE_FILE),
                snapshot.state.as_deref(),
            )?;
            let manifest = BackupManifest {
                schema_version: BACKUP_SCHEMA_VERSION,
                id,
                created_at_unix_ms: unix_time_ms()?,
                config: StoredFile::from_contents(snapshot.config.as_deref()),
                auth: StoredFile::from_contents(snapshot.auth.as_deref()),
                state: StoredFile::from_contents(snapshot.state.as_deref()),
            };
            durable_fs::atomic_write(
                &directory.join(BACKUP_MANIFEST_FILE),
                &serialize_json(&manifest)?,
            )?;
            durable_fs::sync_directory(&directory)?;
            Ok(StagedBackup {
                id,
                directory: directory.clone(),
                summary: BackupSummary::from(&manifest),
            })
        })();

        if result.is_err() {
            let _ = remove_directory_tree_safely(&directory);
            let _ = durable_fs::sync_directory(&self.paths.backups_dir);
        }
        result
    }

    fn finalize_backup(&self, staged: &StagedBackup) -> Result<BackupSummary, TransactionError> {
        ensure_directory(&staged.directory)?;
        let directory = self.backup_dir(staged.id);
        if !path_is_missing(&directory)? {
            return Err(TransactionError::BackupAlreadyExists(staged.id));
        }
        fs::rename(&staged.directory, &directory)
            .map_err(|source| io_error(&staged.directory, source))?;
        durable_fs::sync_directory(&self.paths.backups_dir)?;
        Ok(staged.summary.clone())
    }

    fn discard_staged_backup(&self, staged: &StagedBackup) -> Result<(), TransactionError> {
        remove_directory_tree_safely(&staged.directory)?;
        durable_fs::sync_directory(&self.paths.backups_dir)?;
        Ok(())
    }

    fn abort_staged_backup(
        &self,
        staged: &StagedBackup,
        source: TransactionError,
    ) -> TransactionError {
        match self.discard_staged_backup(staged) {
            Ok(()) => source,
            Err(cleanup) => TransactionError::RollbackFailed {
                source: Box::new(source),
                rollback: Box::new(cleanup),
            },
        }
    }

    fn cleanup_staging_backups(&self) -> Result<(), TransactionError> {
        match fs::symlink_metadata(&self.paths.backups_dir) {
            Ok(_) => ensure_directory(&self.paths.backups_dir)?,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(source) => return Err(io_error(&self.paths.backups_dir, source)),
        }
        let entries = fs::read_dir(&self.paths.backups_dir)
            .map_err(|source| io_error(&self.paths.backups_dir, source))?;

        let mut removed = false;
        for entry in entries {
            let entry = entry.map_err(|source| io_error(&self.paths.backups_dir, source))?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !name.starts_with(BACKUP_STAGING_PREFIX) {
                continue;
            }
            let directory = self.paths.backups_dir.join(name);
            remove_directory_tree_safely(&directory)?;
            removed = true;
        }
        if removed {
            durable_fs::sync_directory(&self.paths.backups_dir)?;
        }
        Ok(())
    }

    fn ensure_backups_directory(&self) -> Result<(), TransactionError> {
        match fs::symlink_metadata(&self.paths.backups_dir) {
            Ok(_) => ensure_directory(&self.paths.backups_dir)?,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(io_error(&self.paths.backups_dir, source)),
        }
        durable_fs::ensure_private_dir(&self.paths.backups_dir)?;
        ensure_directory(&self.paths.backups_dir)
    }

    fn read_backup(&self, id: BackupId) -> Result<Snapshot, TransactionError> {
        ensure_directory(&self.paths.backups_dir)?;
        let directory = self.backup_dir(id);
        ensure_directory(&directory)?;
        let manifest_path = directory.join(BACKUP_MANIFEST_FILE);
        ensure_safe_file_target(&manifest_path)?;
        let raw = durable_fs::read_optional(&manifest_path)?
            .ok_or(TransactionError::BackupMissing(id))?;
        let manifest: BackupManifest = serde_json::from_slice(&raw)
            .map_err(|source| metadata_error(&manifest_path, source))?;
        manifest.validate(id)?;

        Ok(Snapshot {
            config: read_backup_file(id, &directory.join(BACKUP_CONFIG_FILE), &manifest.config)?,
            auth: read_backup_file(id, &directory.join(BACKUP_AUTH_FILE), &manifest.auth)?,
            state: read_backup_file(id, &directory.join(BACKUP_STATE_FILE), &manifest.state)?,
        })
    }

    fn list_backups_locked(&self) -> Result<Vec<BackupSummary>, TransactionError> {
        match fs::symlink_metadata(&self.paths.backups_dir) {
            Ok(_) => ensure_directory(&self.paths.backups_dir)?,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(io_error(&self.paths.backups_dir, source)),
        }
        let entries = fs::read_dir(&self.paths.backups_dir)
            .map_err(|source| io_error(&self.paths.backups_dir, source))?;
        let mut backups = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| io_error(&self.paths.backups_dir, source))?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(uuid) = Uuid::parse_str(&name) else {
                continue;
            };
            let id = BackupId(uuid);
            let directory = self.backup_dir(id);
            ensure_directory(&directory)?;
            let manifest_path = directory.join(BACKUP_MANIFEST_FILE);
            ensure_safe_file_target(&manifest_path)?;
            let Some(raw) = durable_fs::read_optional(&manifest_path)? else {
                continue;
            };
            let manifest: BackupManifest = serde_json::from_slice(&raw)
                .map_err(|source| metadata_error(&manifest_path, source))?;
            manifest.validate(id)?;
            backups.push(BackupSummary::from(&manifest));
        }
        backups.sort_by(|left, right| {
            right
                .created_at_unix_ms
                .cmp(&left.created_at_unix_ms)
                .then_with(|| right.id.to_string().cmp(&left.id.to_string()))
        });
        Ok(backups)
    }

    fn prune_backups_to_limit(
        &self,
        limit: usize,
        protected: Option<BackupId>,
    ) -> Result<(), TransactionError> {
        self.ensure_backups_directory()?;
        let backups = self.list_backups_locked()?;
        let mut ordinary_slots = limit.saturating_sub(usize::from(protected.is_some()));
        for backup in backups {
            if Some(backup.id) == protected {
                continue;
            }
            if ordinary_slots > 0 {
                ordinary_slots -= 1;
                continue;
            }
            let directory = self.backup_dir(backup.id);
            remove_directory_tree_safely(&directory)?;
        }
        durable_fs::sync_directory(&self.paths.backups_dir)?;
        Ok(())
    }

    fn remove_backup(&self, id: BackupId) -> Result<(), TransactionError> {
        let directory = self.backup_dir(id);
        remove_directory_tree_safely(&directory)?;
        durable_fs::sync_directory(&self.paths.backups_dir)?;
        Ok(())
    }

    fn backup_dir(&self, id: BackupId) -> PathBuf {
        self.paths.backups_dir.join(id.to_string())
    }

    fn staging_backup_dir(&self, id: BackupId) -> PathBuf {
        self.paths
            .backups_dir
            .join(format!("{BACKUP_STAGING_PREFIX}{id}"))
    }

    #[cfg(test)]
    fn fail_once_at(&self, point: TestFailurePoint) {
        self.failure_point.set(Some(point));
    }

    #[cfg(test)]
    fn mutate_live_once_at(&self, point: TestMutationPoint) {
        self.mutation_point.set(Some(point));
    }

    #[cfg(test)]
    fn maybe_mutate_live(&self, point: TestMutationPoint) -> Result<(), TransactionError> {
        if self.mutation_point.get() == Some(point) {
            self.mutation_point.set(None);
            durable_fs::atomic_write(
                &self.paths.codex_config,
                b"model = \"external-during-transaction\"\n",
            )?;
        }
        Ok(())
    }

    #[cfg(not(test))]
    fn maybe_mutate_live(&self, _point: TestMutationPoint) -> Result<(), TransactionError> {
        Ok(())
    }

    #[cfg(test)]
    fn maybe_fail(&self, point: TestFailurePoint) -> Result<(), TransactionError> {
        if self.failure_point.get() == Some(point) {
            self.failure_point.set(None);
            return Err(TransactionError::InjectedFailure);
        }
        Ok(())
    }

    #[cfg(not(test))]
    fn maybe_fail(&self, _point: TestFailurePoint) -> Result<(), TransactionError> {
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct Snapshot {
    config: Option<Vec<u8>>,
    auth: Option<Vec<u8>>,
    state: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LiveRevisions {
    config: String,
    auth: String,
}

impl LiveRevisions {
    fn from_snapshot(snapshot: &Snapshot) -> Self {
        Self {
            config: durable_fs::revision(snapshot.config.as_deref()),
            auth: durable_fs::revision(snapshot.auth.as_deref()),
        }
    }
}

#[derive(Clone, Debug)]
struct StagedBackup {
    id: BackupId,
    directory: PathBuf,
    summary: BackupSummary,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TestFailurePoint {
    Config,
    Auth,
    State,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TestMutationPoint {
    BeforeAdoptRevisionCheck,
    AfterBackupStaged,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BackupManifest {
    schema_version: u32,
    id: BackupId,
    created_at_unix_ms: u64,
    config: StoredFile,
    auth: StoredFile,
    state: StoredFile,
}

impl BackupManifest {
    fn validate(&self, expected_id: BackupId) -> Result<(), TransactionError> {
        if self.schema_version != BACKUP_SCHEMA_VERSION {
            return Err(TransactionError::UnsupportedMetadataSchema {
                kind: "backup",
                found: self.schema_version,
                supported: BACKUP_SCHEMA_VERSION,
            });
        }
        if self.id != expected_id {
            return Err(TransactionError::BackupIdMismatch {
                expected: expected_id,
                found: self.id,
            });
        }
        Ok(())
    }
}

impl From<&BackupManifest> for BackupSummary {
    fn from(manifest: &BackupManifest) -> Self {
        Self {
            id: manifest.id,
            created_at_unix_ms: manifest.created_at_unix_ms,
            config_present: manifest.config.present,
            auth_present: manifest.auth.present,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredFile {
    present: bool,
    revision: String,
}

impl StoredFile {
    fn from_contents(contents: Option<&[u8]>) -> Self {
        Self {
            present: contents.is_some(),
            revision: durable_fs::revision(contents),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TransactionJournal {
    schema_version: u32,
    transaction_id: Uuid,
    operation: TransactionOperation,
    rollback_backup_id: BackupId,
}

impl TransactionJournal {
    fn validate(&self) -> Result<(), TransactionError> {
        if self.schema_version != TRANSACTION_SCHEMA_VERSION {
            return Err(TransactionError::UnsupportedMetadataSchema {
                kind: "transaction journal",
                found: self.schema_version,
                supported: TRANSACTION_SCHEMA_VERSION,
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum TransactionOperation {
    Apply { profile_id: ProfileId },
    UpdateContext,
    Restore { source_backup_id: BackupId },
}

fn deserialize_state(raw: Option<&[u8]>) -> Result<Option<ManagedState>, TransactionError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let state: ManagedState = serde_json::from_slice(raw)
        .map_err(|source| TransactionError::InvalidState(source.to_string()))?;
    if state.schema_version != STATE_SCHEMA_VERSION {
        return Err(TransactionError::UnsupportedMetadataSchema {
            kind: "state",
            found: state.schema_version,
            supported: STATE_SCHEMA_VERSION,
        });
    }
    if !is_sha256_hex(&state.relevant_fingerprint) {
        return Err(TransactionError::InvalidState(
            "relevant_fingerprint must be a lowercase SHA-256 digest".to_owned(),
        ));
    }
    Ok(Some(state))
}

fn serialize_json<T: Serialize>(value: &T) -> Result<Vec<u8>, TransactionError> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn state_fingerprint_matches(
    state: &ManagedState,
    actual_fingerprint: &str,
    actual_projection: &RelevantProjection,
) -> Result<bool, TransactionError> {
    if state.relevant_fingerprint == actual_fingerprint {
        return Ok(true);
    }
    Ok(state.relevant_fingerprint == pre_context_relevant_fingerprint(actual_projection)?)
}

fn config_text<'a>(path: &Path, raw: Option<&'a [u8]>) -> Result<&'a str, TransactionError> {
    match raw {
        Some(raw) => str::from_utf8(raw).map_err(|source| TransactionError::InvalidConfigUtf8 {
            path: path.to_path_buf(),
            source,
        }),
        None => Ok(""),
    }
}

fn write_optional(path: &Path, contents: Option<&[u8]>) -> Result<(), TransactionError> {
    match contents {
        Some(contents) => durable_fs::atomic_write(path, contents)?,
        None => durable_fs::atomic_remove(path)?,
    }
    Ok(())
}

fn write_backup_file(path: &Path, contents: Option<&[u8]>) -> Result<(), TransactionError> {
    if let Some(contents) = contents {
        durable_fs::atomic_write(path, contents)?;
    }
    Ok(())
}

fn read_backup_file(
    id: BackupId,
    path: &Path,
    metadata: &StoredFile,
) -> Result<Option<Vec<u8>>, TransactionError> {
    ensure_safe_file_target(path)?;
    let contents = durable_fs::read_optional(path)?;
    if contents.is_some() != metadata.present
        || durable_fs::revision(contents.as_deref()) != metadata.revision
    {
        return Err(TransactionError::CorruptBackup {
            id,
            file: path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("unknown")
                .to_owned(),
        });
    }
    Ok(contents)
}

fn path_is_missing(path: &Path) -> Result<bool, TransactionError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
                return Err(TransactionError::UnsafePath(path.to_path_buf()));
            }
            Ok(false)
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(source) => Err(io_error(path, source)),
    }
}

fn remove_directory_tree_safely(path: &Path) -> Result<(), TransactionError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(io_error(path, source)),
    };
    if metadata.file_type().is_symlink() || is_reparse_point(&metadata) || !metadata.is_dir() {
        return Err(TransactionError::UnsafePath(path.to_path_buf()));
    }

    for entry in fs::read_dir(path).map_err(|source| io_error(path, source))? {
        let entry = entry.map_err(|source| io_error(path, source))?;
        let entry_path = entry.path();
        let metadata =
            fs::symlink_metadata(&entry_path).map_err(|source| io_error(&entry_path, source))?;
        if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
            return Err(TransactionError::UnsafePath(entry_path));
        }
        if metadata.is_dir() {
            remove_directory_tree_safely(&entry_path)?;
        } else if metadata.is_file() {
            fs::remove_file(&entry_path).map_err(|source| io_error(&entry_path, source))?;
        } else {
            return Err(TransactionError::UnsafePath(entry_path));
        }
    }
    fs::remove_dir(path).map_err(|source| io_error(path, source))?;
    Ok(())
}

fn ensure_directory(path: &Path) -> Result<(), TransactionError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| io_error(path, source))?;
    if metadata.file_type().is_symlink() || is_reparse_point(&metadata) || !metadata.is_dir() {
        return Err(TransactionError::UnsafePath(path.to_path_buf()));
    }
    Ok(())
}

fn ensure_safe_file_target(path: &Path) -> Result<(), TransactionError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(io_error(path, source)),
    };
    if metadata.file_type().is_symlink() || is_reparse_point(&metadata) || !metadata.is_file() {
        return Err(TransactionError::UnsafePath(path.to_path_buf()));
    }
    Ok(())
}

#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

fn unix_time_ms() -> Result<u64, TransactionError> {
    let milliseconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| TransactionError::ClockBeforeUnixEpoch)?
        .as_millis();
    u64::try_from(milliseconds).map_err(|_| TransactionError::ClockOverflow)
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn metadata_error(path: &Path, source: serde_json::Error) -> TransactionError {
    TransactionError::InvalidMetadata {
        path: path.to_path_buf(),
        source,
    }
}

fn io_error(path: &Path, source: io::Error) -> TransactionError {
    TransactionError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[derive(Debug, Error)]
pub enum TransactionError {
    #[error(transparent)]
    FileSystem(#[from] DurableFsError),
    #[error(transparent)]
    CodexConfig(#[from] CodexConfigError),
    #[error(transparent)]
    ExternalConflict(Box<ExternalConflict>),
    #[error("Codex config is not valid UTF-8: {path}")]
    InvalidConfigUtf8 {
        path: PathBuf,
        #[source]
        source: str::Utf8Error,
    },
    #[error("transaction metadata is invalid at {path}: {source}")]
    InvalidMetadata {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("state file is invalid: {0}")]
    InvalidState(String),
    #[error("unsupported {kind} schema {found}; expected {supported}")]
    UnsupportedMetadataSchema {
        kind: &'static str,
        found: u32,
        supported: u32,
    },
    #[error("no backup is available")]
    NoBackup,
    #[error("backup {0} is missing")]
    BackupMissing(BackupId),
    #[error("backup {0} already exists")]
    BackupAlreadyExists(BackupId),
    #[error("backup directory does not match its manifest: expected {expected}, found {found}")]
    BackupIdMismatch { expected: BackupId, found: BackupId },
    #[error("backup {id} is corrupt: {file} does not match its manifest")]
    CorruptBackup { id: BackupId, file: String },
    #[error("refusing to read or replace a symbolic link, reparse point, or non-file path: {0}")]
    UnsafePath(PathBuf),
    #[error("staged Codex validation failed: {0}")]
    StagedValidation(String),
    #[error("Codex config or auth changed while the transaction was being prepared")]
    ConcurrentModification {
        expected_config_revision: String,
        actual_config_revision: String,
        expected_auth_revision: String,
        actual_auth_revision: String,
    },
    #[error("transaction failed ({source}); rollback also failed ({rollback})")]
    RollbackFailed {
        source: Box<TransactionError>,
        rollback: Box<TransactionError>,
    },
    #[error("file operation failed for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("system clock is before the Unix epoch")]
    ClockBeforeUnixEpoch,
    #[error("system time does not fit in a 64-bit millisecond timestamp")]
    ClockOverflow,
    #[error("transaction metadata could not be serialized: {0}")]
    Serialize(#[from] serde_json::Error),
    #[cfg(test)]
    #[error("injected transaction failure")]
    InjectedFailure,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ApiKey;

    const ORIGINAL_CONFIG: &str = r#"# keep this comment
model_provider = "old"
model = "old-model"

[features]
experimental = true

[model_providers.old]
name = "Old relay"
base_url = "https://old.example/v1"
wire_api = "responses"
requires_openai_auth = true
"#;
    const ORIGINAL_AUTH: &[u8] = br#"{
  "OPENAI_API_KEY": "sk-old",
  "tokens": { "preserve": true }
}
"#;

    fn fixture() -> (tempfile::TempDir, AppPaths, TransactionManager) {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_home(temp.path());
        durable_fs::atomic_write(&paths.codex_config, ORIGINAL_CONFIG.as_bytes()).unwrap();
        durable_fs::atomic_write(&paths.codex_auth, ORIGINAL_AUTH).unwrap();
        let manager = TransactionManager::new(paths.clone());
        (temp, paths, manager)
    }

    fn activation(model: &str) -> Activation {
        Activation {
            profile_id: ProfileId::new(),
            provider_name: "Relay A".to_owned(),
            base_url: "https://relay.example/v1".to_owned(),
            api_key: ApiKey::new("sk-new-secret").unwrap(),
            model: model.to_owned(),
            review_model: Some("review-model".to_owned()),
            context: None,
        }
    }

    fn write_pre_context_state(paths: &AppPaths, state: &ManagedState) -> ManagedState {
        let config = fs::read_to_string(&paths.codex_config).unwrap();
        let auth = fs::read(&paths.codex_auth).unwrap();
        let projection = relevant_projection(&config, Some(auth.as_slice())).unwrap();
        let mut pre_context_state = state.clone();
        pre_context_state.relevant_fingerprint =
            pre_context_relevant_fingerprint(&projection).unwrap();
        assert_ne!(
            pre_context_state.relevant_fingerprint,
            state.relevant_fingerprint
        );
        durable_fs::atomic_write(&paths.state, &serialize_json(&pre_context_state).unwrap())
            .unwrap();
        pre_context_state
    }

    #[test]
    fn apply_patches_only_owned_fields_and_records_state() {
        let (_temp, paths, manager) = fixture();
        let activation = activation("new-model");

        let outcome = manager.apply(&activation, ConflictPolicy::Reject).unwrap();

        let config = fs::read_to_string(&paths.codex_config).unwrap();
        let auth = fs::read_to_string(&paths.codex_auth).unwrap();
        assert!(config.contains("# keep this comment"));
        assert!(config.contains("experimental = true"));
        assert!(config.contains("model = \"new-model\""));
        assert!(auth.contains("sk-new-secret"));
        assert!(auth.contains("\"preserve\": true"));
        assert_eq!(outcome.state.active_profile_id, Some(activation.profile_id));
        assert_eq!(manager.load_state().unwrap(), Some(outcome.state));
        assert!(!paths.journal.exists());
        assert_eq!(manager.list_backups().unwrap().len(), 1);
    }

    #[test]
    fn legacy_usage_history_exposes_only_a_validated_legacy_profile_id() {
        let (_temp, paths, manager) = fixture();
        let profile_id = ProfileId::from_uuid(
            uuid::Uuid::parse_str("e519bc8f-120c-43c3-96b5-a7799f6eec18").unwrap(),
        );
        let config = r#"model_provider = "codex_switch"
model = "gpt-5.6-sol"

[model_providers.codex_switch]
name = "legacy relay"
base_url = "https://relay.example/v1"
wire_api = "responses"
requires_openai_auth = true
"#;
        durable_fs::atomic_write(&paths.codex_config, config.as_bytes()).unwrap();
        let state = ManagedState {
            schema_version: STATE_SCHEMA_VERSION,
            active_profile_id: Some(profile_id),
            relevant_fingerprint: relevant_fingerprint(config, Some(ORIGINAL_AUTH)).unwrap(),
        };
        durable_fs::atomic_write(&paths.state, &serialize_json(&state).unwrap()).unwrap();

        let history = manager.legacy_usage_history().unwrap();

        assert!(history.backups.is_empty());
        assert_eq!(history.live.legacy_profile_id, Some(profile_id));
    }

    #[test]
    fn context_update_is_transactional_and_preserves_auth_and_managed_state() {
        let (_temp, paths, manager) = fixture();
        let applied = manager
            .apply(&activation("new-model"), ConflictPolicy::Reject)
            .unwrap();
        let auth_before = fs::read(&paths.codex_auth).unwrap();

        manager
            .update_context(ContextSettings {
                model_context_window: Some(272_000),
                model_auto_compact_token_limit: Some(217_600),
                model_auto_compact_token_limit_scope: Some(crate::domain::AutoCompactScope::Total),
            })
            .unwrap();

        let config = fs::read_to_string(&paths.codex_config).unwrap();
        assert!(config.contains("model_context_window = 272000"));
        assert!(config.contains("model_auto_compact_token_limit = 217600"));
        assert!(!config.contains("max_output"));
        assert_eq!(fs::read(&paths.codex_auth).unwrap(), auth_before);
        let updated_state = manager.load_state().unwrap().unwrap();
        assert_eq!(
            updated_state.active_profile_id,
            applied.state.active_profile_id
        );
        assert_ne!(
            updated_state.relevant_fingerprint,
            applied.state.relevant_fingerprint
        );
        assert_eq!(manager.list_backups().unwrap().len(), 2);
        assert!(!paths.journal.exists());
    }

    #[test]
    fn pre_context_state_fingerprint_does_not_trigger_a_false_apply_conflict() {
        let (_temp, paths, manager) = fixture();
        let applied = manager
            .apply(&activation("first-model"), ConflictPolicy::Reject)
            .unwrap();
        write_pre_context_state(&paths, &applied.state);

        let next = activation("second-model");
        let outcome = manager.apply(&next, ConflictPolicy::Reject).unwrap();

        assert_eq!(outcome.state.active_profile_id, Some(next.profile_id));
        assert_eq!(manager.load_state().unwrap(), Some(outcome.state));
    }

    #[test]
    fn context_update_migrates_pre_context_state_without_losing_active_profile() {
        let (_temp, paths, manager) = fixture();
        let applied = manager
            .apply(&activation("managed-model"), ConflictPolicy::Reject)
            .unwrap();
        let pre_context_state = write_pre_context_state(&paths, &applied.state);

        manager
            .update_context(ContextSettings {
                model_context_window: Some(272_000),
                model_auto_compact_token_limit: Some(217_600),
                model_auto_compact_token_limit_scope: Some(crate::domain::AutoCompactScope::Total),
            })
            .unwrap();

        let updated_state = manager.load_state().unwrap().unwrap();
        assert_eq!(
            updated_state.active_profile_id,
            pre_context_state.active_profile_id
        );
        assert_ne!(
            updated_state.relevant_fingerprint,
            pre_context_state.relevant_fingerprint
        );
        let config = fs::read_to_string(&paths.codex_config).unwrap();
        let auth = fs::read(&paths.codex_auth).unwrap();
        assert_eq!(
            updated_state.relevant_fingerprint,
            relevant_fingerprint(&config, Some(auth.as_slice())).unwrap()
        );
    }

    #[test]
    fn context_update_rejects_external_managed_changes_without_adopting_them() {
        let (_temp, paths, manager) = fixture();
        let applied = manager
            .apply(&activation("managed-model"), ConflictPolicy::Reject)
            .unwrap();
        write_pre_context_state(&paths, &applied.state);
        let state_before = fs::read(&paths.state).unwrap();
        let backups_before = manager.list_backups().unwrap().len();
        let externally_edited = fs::read_to_string(&paths.codex_config)
            .unwrap()
            .replace("model = \"managed-model\"", "model = \"external-model\"");
        durable_fs::atomic_write(&paths.codex_config, externally_edited.as_bytes()).unwrap();

        let error = manager
            .update_context(ContextSettings {
                model_context_window: Some(272_000),
                model_auto_compact_token_limit: Some(217_600),
                model_auto_compact_token_limit_scope: Some(crate::domain::AutoCompactScope::Total),
            })
            .unwrap_err();

        assert!(matches!(error, TransactionError::ExternalConflict(_)));
        assert_eq!(fs::read(&paths.state).unwrap(), state_before);
        assert_eq!(manager.list_backups().unwrap().len(), backups_before);
        let config = fs::read_to_string(&paths.codex_config).unwrap();
        assert!(config.contains("model = \"external-model\""));
        assert!(!config.contains("model_context_window"));
    }

    #[test]
    fn conflict_detection_ignores_unrelated_edits_but_rejects_model_edits() {
        let (_temp, paths, manager) = fixture();
        manager
            .apply(&activation("first-model"), ConflictPolicy::Reject)
            .unwrap();

        let mut config = fs::read_to_string(&paths.codex_config).unwrap();
        config.push_str("\n[unrelated]\nvalue = 42\n");
        durable_fs::atomic_write(&paths.codex_config, config.as_bytes()).unwrap();
        manager
            .apply(&activation("second-model"), ConflictPolicy::Reject)
            .unwrap();

        let config = fs::read_to_string(&paths.codex_config)
            .unwrap()
            .replace("model = \"second-model\"", "model = \"external-model\"");
        durable_fs::atomic_write(&paths.codex_config, config.as_bytes()).unwrap();
        let error = manager
            .apply(&activation("third-model"), ConflictPolicy::Reject)
            .unwrap_err();
        assert!(matches!(error, TransactionError::ExternalConflict(_)));

        manager
            .apply(&activation("third-model"), ConflictPolicy::Overwrite)
            .unwrap();
        assert!(
            fs::read_to_string(&paths.codex_config)
                .unwrap()
                .contains("model = \"third-model\"")
        );
    }

    #[test]
    fn adopting_current_state_clears_the_conflict_without_touching_codex_files() {
        let (_temp, paths, manager) = fixture();
        manager
            .apply(&activation("managed-model"), ConflictPolicy::Reject)
            .unwrap();
        let externally_edited = fs::read_to_string(&paths.codex_config)
            .unwrap()
            .replace("model = \"managed-model\"", "model = \"external-model\"");
        durable_fs::atomic_write(&paths.codex_config, externally_edited.as_bytes()).unwrap();
        let auth_before = fs::read(&paths.codex_auth).unwrap();
        let backups_before = manager.list_backups().unwrap().len();
        assert!(matches!(
            manager.apply(&activation("next-model"), ConflictPolicy::Reject),
            Err(TransactionError::ExternalConflict(_))
        ));

        let adopted_profile_id = ProfileId::new();
        let state = manager.adopt_current(Some(adopted_profile_id)).unwrap();

        assert_eq!(state.active_profile_id, Some(adopted_profile_id));
        assert_eq!(
            fs::read_to_string(&paths.codex_config).unwrap(),
            externally_edited
        );
        assert_eq!(fs::read(&paths.codex_auth).unwrap(), auth_before);
        assert_eq!(manager.list_backups().unwrap().len(), backups_before);
        manager
            .apply(&activation("next-model"), ConflictPolicy::Reject)
            .unwrap();
    }

    #[test]
    fn failed_second_step_rolls_back_both_codex_files_and_state() {
        let (_temp, paths, manager) = fixture();
        manager.fail_once_at(TestFailurePoint::Config);

        let error = manager
            .apply(&activation("new-model"), ConflictPolicy::Reject)
            .unwrap_err();

        assert!(matches!(error, TransactionError::InjectedFailure));
        assert_eq!(
            fs::read(&paths.codex_config).unwrap(),
            ORIGINAL_CONFIG.as_bytes()
        );
        assert_eq!(fs::read(&paths.codex_auth).unwrap(), ORIGINAL_AUTH);
        assert!(!paths.state.exists());
        assert!(!paths.journal.exists());
        assert!(manager.has_backup().unwrap());
    }

    #[test]
    fn startup_recovery_uses_undo_journal_after_interrupted_write() {
        let (_temp, paths, manager) = fixture();
        let original = manager.read_live_snapshot().unwrap();
        let backup = manager.create_backup(&original).unwrap();
        let transaction_id = Uuid::new_v4();
        let journal = TransactionJournal {
            schema_version: TRANSACTION_SCHEMA_VERSION,
            transaction_id,
            operation: TransactionOperation::Apply {
                profile_id: ProfileId::new(),
            },
            rollback_backup_id: backup.id,
        };
        durable_fs::atomic_write(&paths.journal, &serialize_json(&journal).unwrap()).unwrap();
        durable_fs::atomic_write(&paths.codex_config, b"model = \"half-written\"\n").unwrap();

        assert_eq!(
            manager.recover_if_needed().unwrap(),
            RecoveryOutcome::RolledBack {
                transaction_id,
                backup_id: backup.id,
            }
        );
        assert_eq!(
            fs::read(&paths.codex_config).unwrap(),
            ORIGINAL_CONFIG.as_bytes()
        );
        assert_eq!(fs::read(&paths.codex_auth).unwrap(), ORIGINAL_AUTH);
        assert!(!paths.journal.exists());
    }

    #[test]
    fn restore_latest_returns_to_the_previous_configuration() {
        let (_temp, paths, manager) = fixture();
        manager
            .apply(&activation("new-model"), ConflictPolicy::Reject)
            .unwrap();

        let outcome = manager.restore_latest().unwrap();

        assert_eq!(
            fs::read(&paths.codex_config).unwrap(),
            ORIGINAL_CONFIG.as_bytes()
        );
        assert_eq!(fs::read(&paths.codex_auth).unwrap(), ORIGINAL_AUTH);
        assert_eq!(outcome.state.active_profile_id, None);
        assert!(manager.has_backup().unwrap());
    }

    #[test]
    fn restore_preserves_active_profile_from_a_pre_context_state() {
        let (_temp, paths, manager) = fixture();
        let first = activation("first-model");
        let first_outcome = manager.apply(&first, ConflictPolicy::Reject).unwrap();
        manager.remove_backup(first_outcome.backup.id).unwrap();
        write_pre_context_state(&paths, &first_outcome.state);
        manager
            .apply(&activation("second-model"), ConflictPolicy::Reject)
            .unwrap();

        let outcome = manager.restore_latest().unwrap();

        assert_eq!(outcome.state.active_profile_id, Some(first.profile_id));
        let config = fs::read_to_string(&paths.codex_config).unwrap();
        assert!(config.contains("model = \"first-model\""));
        let auth = fs::read(&paths.codex_auth).unwrap();
        assert_eq!(
            outcome.state.relevant_fingerprint,
            relevant_fingerprint(&config, Some(auth.as_slice())).unwrap()
        );
    }

    #[test]
    fn retains_only_ten_backups() {
        let (_temp, _paths, manager) = fixture();
        for index in 0..12 {
            manager
                .apply(
                    &activation(&format!("model-{index}")),
                    ConflictPolicy::Reject,
                )
                .unwrap();
        }

        assert_eq!(manager.list_backups().unwrap().len(), MAX_BACKUPS);
    }

    struct RejectingValidator;

    impl StagedValidator for RejectingValidator {
        fn validate(&self, _config_toml: &str, _auth_json: &[u8]) -> Result<(), String> {
            Err("strict parser rejected the staged files".to_owned())
        }
    }

    #[test]
    fn staged_validation_runs_before_backup_or_mutation() {
        let (_temp, paths, manager) = fixture();
        let error = manager
            .apply_validated(
                &activation("new-model"),
                ConflictPolicy::Reject,
                Some(&RejectingValidator),
            )
            .unwrap_err();

        assert!(matches!(error, TransactionError::StagedValidation(_)));
        assert_eq!(
            fs::read(&paths.codex_config).unwrap(),
            ORIGINAL_CONFIG.as_bytes()
        );
        assert!(!manager.has_backup().unwrap());
    }

    struct MutatingValidator {
        config_path: PathBuf,
        auth_path: PathBuf,
        config_contents: Vec<u8>,
        auth_contents: Vec<u8>,
    }

    impl StagedValidator for MutatingValidator {
        fn validate(&self, _config_toml: &str, _auth_json: &[u8]) -> Result<(), String> {
            durable_fs::atomic_write(&self.config_path, &self.config_contents).unwrap();
            durable_fs::atomic_write(&self.auth_path, &self.auth_contents).unwrap();
            Ok(())
        }
    }

    #[test]
    fn external_change_during_validation_aborts_without_overwriting_live_files() {
        let (_temp, paths, manager) = fixture();
        let external_config = b"model = \"external-from-validator\"\n".to_vec();
        let external_auth = br#"{"OPENAI_API_KEY":"sk-external-validator"}"#.to_vec();
        let validator = MutatingValidator {
            config_path: paths.codex_config.clone(),
            auth_path: paths.codex_auth.clone(),
            config_contents: external_config.clone(),
            auth_contents: external_auth.clone(),
        };

        let error = manager
            .apply_validated(
                &activation("new-model"),
                ConflictPolicy::Reject,
                Some(&validator),
            )
            .unwrap_err();

        assert!(matches!(
            error,
            TransactionError::ConcurrentModification { .. }
        ));
        assert_eq!(fs::read(&paths.codex_config).unwrap(), external_config);
        assert_eq!(fs::read(&paths.codex_auth).unwrap(), external_auth);
        assert!(!paths.journal.exists());
        assert!(!paths.state.exists());
        assert!(!manager.has_backup().unwrap());
    }

    #[test]
    fn external_change_during_backup_staging_aborts_without_committing() {
        let (_temp, paths, manager) = fixture();
        manager.mutate_live_once_at(TestMutationPoint::AfterBackupStaged);

        let error = manager
            .apply(&activation("new-model"), ConflictPolicy::Reject)
            .unwrap_err();

        assert!(matches!(
            error,
            TransactionError::ConcurrentModification { .. }
        ));
        assert_eq!(
            fs::read_to_string(&paths.codex_config).unwrap(),
            "model = \"external-during-transaction\"\n"
        );
        assert_eq!(fs::read(&paths.codex_auth).unwrap(), ORIGINAL_AUTH);
        assert!(!paths.journal.exists());
        assert!(!paths.state.exists());
        assert!(!manager.has_backup().unwrap());
    }

    #[test]
    fn external_change_during_restore_backup_staging_is_preserved() {
        let (_temp, paths, manager) = fixture();
        manager
            .apply(&activation("managed-model"), ConflictPolicy::Reject)
            .unwrap();
        manager.mutate_live_once_at(TestMutationPoint::AfterBackupStaged);

        let error = manager.restore_latest().unwrap_err();

        assert!(matches!(
            error,
            TransactionError::ConcurrentModification { .. }
        ));
        assert_eq!(
            fs::read_to_string(&paths.codex_config).unwrap(),
            "model = \"external-during-transaction\"\n"
        );
        assert!(
            fs::read_to_string(&paths.codex_auth)
                .unwrap()
                .contains("sk-new-secret")
        );
        assert!(!paths.journal.exists());
        assert_eq!(manager.list_backups().unwrap().len(), 1);
    }

    #[test]
    fn adopt_current_rechecks_live_files_before_writing_state() {
        let (_temp, paths, manager) = fixture();
        manager.mutate_live_once_at(TestMutationPoint::BeforeAdoptRevisionCheck);

        let error = manager.adopt_current(Some(ProfileId::new())).unwrap_err();

        assert!(matches!(
            error,
            TransactionError::ConcurrentModification { .. }
        ));
        assert_eq!(
            fs::read_to_string(&paths.codex_config).unwrap(),
            "model = \"external-during-transaction\"\n"
        );
        assert!(!paths.state.exists());
        assert!(!paths.journal.exists());
    }

    #[test]
    fn recovery_cleans_abandoned_backup_staging_directories() {
        let (_temp, paths, manager) = fixture();
        durable_fs::ensure_private_dir(&paths.backups_dir).unwrap();
        let staging = paths
            .backups_dir
            .join(format!("{BACKUP_STAGING_PREFIX}{}", Uuid::new_v4()));
        durable_fs::ensure_private_dir(&staging).unwrap();
        durable_fs::atomic_write(&staging.join(BACKUP_AUTH_FILE), b"plaintext-secret").unwrap();

        assert_eq!(manager.recover_if_needed().unwrap(), RecoveryOutcome::None);

        assert!(!staging.exists());
        assert!(manager.list_backups().unwrap().is_empty());
    }
}
