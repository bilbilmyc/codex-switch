use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::codex_config::{
    self, TOOL_PROVIDER_ID, profile_id_from_provider_id, provider_id_for_profile,
};
use crate::codex_validator::CodexStagedValidator;
use crate::context::{self, InstructionScope};
use crate::domain::{Activation, ApiKey, AutoCompactScope, Profile, ProfileContext, ProfileId};
use crate::durable_fs;
use crate::legacy_usage::{
    ProfileLegacyUsageWindow, normalize_profile_windows, reconstruct_legacy_usage,
};
use crate::models;
use crate::paths::AppPaths;
use crate::process;
use crate::profiles::{ProfileStore, ProfilesDocument};
use crate::transaction::{ConflictPolicy, TransactionError, TransactionManager};
use crate::usage::{LegacyUsageWindow, UsagePeriod, UsageScope};
use crate::usage_store::UsageStore;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileDraft {
    pub name: String,
    pub base_url: String,
    /// New values replace the stored secret. An omitted value preserves an existing secret.
    pub api_key: Option<String>,
    #[serde(default)]
    pub clear_api_key: bool,
    pub model: String,
    pub review_model: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileSummary {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub model: String,
    pub review_model: Option<String>,
    pub has_api_key: bool,
    pub is_active: bool,
    pub apply_state: ProfileApplyState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileApplyState {
    Inactive,
    Applied,
    PendingChanges,
    ExternalDrift,
    Unknown,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Bootstrap {
    pub profiles: Vec<ProfileSummary>,
    pub can_restore: bool,
    pub startup_message: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelListView {
    pub models: Vec<String>,
    pub cache_label: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextDraft {
    pub use_defaults: bool,
    pub window_k: String,
    pub compact_percent: u8,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextView {
    pub use_defaults: bool,
    pub window_k: String,
    pub compact_percent: u8,
    pub summary: String,
    pub is_active: bool,
    pub sync_state: String,
    pub status: String,
    pub budget: ContextBudgetView,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextBudgetView {
    pub recent_session: String,
    pub instruction_tokens: String,
    pub available_budget: String,
    pub history_ratio: f32,
    pub instruction_ratio: f32,
    pub remaining_ratio: f32,
    pub suggested_window_k: String,
    pub instructions: Vec<InstructionView>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstructionView {
    pub name: String,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageValue {
    pub input: String,
    pub cached: String,
    pub output: String,
    pub calls: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageTrend {
    pub label: String,
    pub input: String,
    pub output: String,
    pub input_ratio: f32,
    pub output_ratio: f32,
    pub usage: UsageValue,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageModel {
    pub model: String,
    pub input: String,
    pub cached: String,
    pub output: String,
    pub calls: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageView {
    pub period: String,
    /// The latest day (or latest hour bucket for today), matching V1's dashboard cards.
    pub current: UsageValue,
    /// V1's relay overview uses the current calendar day's aggregate, independently of the
    /// detailed statistics period currently selected in the UI.
    pub today_summary: String,
    /// The selected period aggregate, shown separately for 7- and 30-day views.
    pub period_total: UsageValue,
    pub trend: Vec<UsageTrend>,
    pub models: Vec<UsageModel>,
    pub has_data: bool,
    pub status: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfirmationOption {
    pub id: String,
    pub label: String,
    pub intent: ConfirmationIntent,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmationIntent {
    Primary,
    Danger,
    Neutral,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Confirmation {
    pub token: String,
    pub title: String,
    pub detail: String,
    pub options: Vec<ConfirmationOption>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApplyResponse {
    Applied {
        active_profile_id: String,
        warning: Option<String>,
    },
    RequiresConfirmation {
        confirmation: Confirmation,
    },
    ImportedCurrent {
        profile: ProfileSummary,
        warning: Option<String>,
    },
    Restored {
        active_profile_id: Option<String>,
        warning: Option<String>,
    },
    ContextSaved {
        context: ContextView,
        warning: Option<String>,
    },
}

#[derive(Debug)]
enum PendingConfirmation {
    Process {
        activation: Activation,
        desktop_executable: Option<PathBuf>,
        desktop_only: bool,
    },
    Conflict {
        activation: Activation,
        desktop_executable: Option<PathBuf>,
        desktop_was_closed: bool,
    },
    RestoreProcess {
        desktop_executable: Option<PathBuf>,
        desktop_only: bool,
    },
    ContextProcess {
        profile_id: ProfileId,
        settings: crate::codex_config::ContextSettings,
        desktop_executable: Option<PathBuf>,
        desktop_only: bool,
    },
    ContextConflict {
        profile_id: ProfileId,
        settings: crate::codex_config::ContextSettings,
        desktop_executable: Option<PathBuf>,
        desktop_was_closed: bool,
    },
}

pub struct AppService {
    paths: AppPaths,
    legacy_paths: AppPaths,
    /// Serializes IPC operations that read-modify-write the shared V1/V2 state.
    operations: Mutex<()>,
    pending: Mutex<HashMap<Uuid, PendingConfirmation>>,
}

impl AppService {
    pub fn discover() -> Result<Self, ServiceError> {
        let paths = AppPaths::discover().map_err(ServiceError::paths)?;
        // The Tauri shell holds the shared V1/V2 lifecycle lock after its same-instance plugin
        // initializes. The service itself remains lock-free so it cannot acquire that lock twice.
        durable_fs::ensure_private_dir(&paths.tool_dir).map_err(ServiceError::filesystem)?;
        Ok(Self::with_paths(paths.clone(), paths))
    }

    pub fn with_paths(paths: AppPaths, legacy_paths: AppPaths) -> Self {
        Self {
            paths,
            legacy_paths,
            operations: Mutex::new(()),
            pending: Mutex::new(HashMap::new()),
        }
    }

    pub fn bootstrap(&self) -> Result<Bootstrap, ServiceError> {
        let _operation = self.operation_guard()?;
        self.bootstrap_inner()
    }

    fn bootstrap_inner(&self) -> Result<Bootstrap, ServiceError> {
        durable_fs::ensure_private_dir(&self.paths.tool_dir).map_err(ServiceError::filesystem)?;
        durable_fs::ensure_private_dir(&self.paths.model_cache_dir)
            .map_err(ServiceError::filesystem)?;
        durable_fs::ensure_private_dir(&self.paths.backups_dir)
            .map_err(ServiceError::filesystem)?;
        let recovery = TransactionManager::new(self.paths.clone())
            .recover_if_needed()
            .map_err(ServiceError::transaction)?;
        self.migrate_legacy_profiles_if_needed()?;
        let first_run = !self.paths.profiles.exists();
        let mut startup_message = match recovery {
            crate::transaction::RecoveryOutcome::None => None,
            crate::transaction::RecoveryOutcome::RolledBack { .. } => {
                Some("检测到未完成的切换，已恢复原配置".to_owned())
            }
        };
        if first_run {
            match self.import_current_profile() {
                Ok(ApplyResponse::ImportedCurrent { warning, .. }) => {
                    startup_message =
                        Some(warning.unwrap_or_else(|| "已导入当前 Codex 配置".to_owned()));
                }
                Ok(_) => unreachable!("importing the current profile has one response variant"),
                Err(_) => {
                    self.store()
                        .save(&ProfilesDocument::default())
                        .map_err(ServiceError::profiles)?;
                    startup_message = Some("未能自动导入当前配置，可手动新建中转站".to_owned());
                }
            }
        }
        let document = self.store().load().map_err(ServiceError::profiles)?;
        let active_profile_id = self.active_profile_id(&document);
        let (live_profile, baseline_matches) = self.live_apply_context();

        Ok(Bootstrap {
            profiles: document
                .profiles
                .iter()
                .map(|profile| {
                    ProfileSummary::from_profile(
                        profile,
                        active_profile_id,
                        live_profile.as_ref(),
                        baseline_matches,
                    )
                })
                .collect(),
            can_restore: TransactionManager::new(self.paths.clone())
                .has_backup()
                .unwrap_or(false),
            startup_message,
        })
    }

    fn operation_guard(&self) -> Result<MutexGuard<'_, ()>, ServiceError> {
        self.operations.lock().map_err(|_| ServiceError::internal())
    }

    pub fn new_profile(&self) -> Result<ProfileSummary, ServiceError> {
        let _operation = self.operation_guard()?;
        self.migrate_legacy_profiles_if_needed()?;
        let mut document = self.store().load().map_err(ServiceError::profiles)?;
        let profile = Profile::without_api_key(
            unique_name(&document, "新中转站"),
            "https://relay.example/v1",
            "gpt-5",
            None,
        )
        .map_err(ServiceError::domain)?;
        document
            .insert(profile.clone())
            .map_err(ServiceError::profiles)?;
        self.store()
            .save(&document)
            .map_err(ServiceError::profiles)?;
        Ok(self.profile_summary(&profile, &document))
    }

    pub fn create_profile(&self, draft: ProfileDraft) -> Result<ProfileSummary, ServiceError> {
        let _operation = self.operation_guard()?;
        self.migrate_legacy_profiles_if_needed()?;
        let mut document = self.store().load().map_err(ServiceError::profiles)?;
        let profile = profile_from_draft(ProfileId::new(), None, draft)?;
        document
            .insert(profile.clone())
            .map_err(ServiceError::profiles)?;
        self.store()
            .save(&document)
            .map_err(ServiceError::profiles)?;
        Ok(self.profile_summary(&profile, &document))
    }

    pub fn update_profile(
        &self,
        profile_id: String,
        draft: ProfileDraft,
    ) -> Result<ProfileSummary, ServiceError> {
        let _operation = self.operation_guard()?;
        self.migrate_legacy_profiles_if_needed()?;
        let profile_id = parse_profile_id(&profile_id)?;
        let mut document = self.store().load().map_err(ServiceError::profiles)?;
        let existing = document
            .get(profile_id)
            .cloned()
            .ok_or_else(|| ServiceError::not_found(profile_id))?;
        let profile = profile_from_draft(profile_id, Some(&existing), draft)?;
        let invalidate_model_cache = !same_model_catalog(&existing, &profile);
        *document
            .get_mut(profile_id)
            .expect("profile was just found in this document") = profile.clone();
        document.validate().map_err(ServiceError::profiles)?;
        self.store()
            .save(&document)
            .map_err(ServiceError::profiles)?;
        if invalidate_model_cache {
            let _ = models::remove_cache(&self.paths.model_cache_dir, profile_id);
        }
        Ok(self.profile_summary(&profile, &document))
    }

    pub fn duplicate_profile(&self, profile_id: String) -> Result<ProfileSummary, ServiceError> {
        let _operation = self.operation_guard()?;
        self.migrate_legacy_profiles_if_needed()?;
        let profile_id = parse_profile_id(&profile_id)?;
        let mut document = self.store().load().map_err(ServiceError::profiles)?;
        let source = document
            .get(profile_id)
            .cloned()
            .ok_or_else(|| ServiceError::not_found(profile_id))?;
        let mut duplicate = source;
        duplicate.id = ProfileId::new();
        duplicate.name = unique_name(&document, &format!("{} 副本", duplicate.name));
        document
            .insert(duplicate.clone())
            .map_err(ServiceError::profiles)?;
        self.store()
            .save(&document)
            .map_err(ServiceError::profiles)?;
        Ok(self.profile_summary(&duplicate, &document))
    }

    pub fn delete_profile(&self, profile_id: String) -> Result<(), ServiceError> {
        let _operation = self.operation_guard()?;
        self.migrate_legacy_profiles_if_needed()?;
        let profile_id = parse_profile_id(&profile_id)?;
        let mut document = self.store().load().map_err(ServiceError::profiles)?;
        document
            .remove(profile_id)
            .ok_or_else(|| ServiceError::not_found(profile_id))?;
        self.store()
            .save(&document)
            .map_err(ServiceError::profiles)?;
        let _ = models::remove_cache(&self.paths.model_cache_dir, profile_id);
        Ok(())
    }

    pub fn import_profiles_interactive(&self) -> Result<Bootstrap, ServiceError> {
        let _operation = self.operation_guard()?;
        self.migrate_legacy_profiles_if_needed()?;
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Codex Switch profiles", &["toml"])
            .pick_file()
        else {
            return self.bootstrap_inner();
        };
        let bytes = fs::read(path).map_err(ServiceError::filesystem)?;
        let imported = ProfileStore::deserialize(&bytes).map_err(ServiceError::profiles)?;
        let mut document = self.store().load().map_err(ServiceError::profiles)?;
        for mut profile in imported.profiles {
            if document.get(profile.id).is_some() {
                profile.id = ProfileId::new();
            }
            profile.name = unique_name(&document, &profile.name);
            document.insert(profile).map_err(ServiceError::profiles)?;
        }
        self.store()
            .save(&document)
            .map_err(ServiceError::profiles)?;
        self.bootstrap_inner()
    }

    pub fn import_current(&self) -> Result<ProfileSummary, ServiceError> {
        let _operation = self.operation_guard()?;
        self.migrate_legacy_profiles_if_needed()?;
        match self.import_current_profile()? {
            ApplyResponse::ImportedCurrent { profile, .. } => Ok(profile),
            _ => Err(ServiceError::internal()),
        }
    }

    pub fn export_profiles_interactive(&self, include_keys: bool) -> Result<(), ServiceError> {
        let _operation = self.operation_guard()?;
        self.migrate_legacy_profiles_if_needed()?;
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Codex Switch profiles", &["toml"])
            .set_file_name("codex-switch-profiles.toml")
            .save_file()
        else {
            return Ok(());
        };
        let mut document = self.store().load().map_err(ServiceError::profiles)?;
        if !include_keys {
            for profile in &mut document.profiles {
                profile.api_key = None;
            }
        }
        let bytes = ProfileStore::serialize(&document).map_err(ServiceError::profiles)?;
        durable_fs::atomic_write(&path, &bytes).map_err(ServiceError::filesystem)
    }

    pub fn load_model_cache(&self, profile_id: String) -> Result<ModelListView, ServiceError> {
        let _operation = self.operation_guard()?;
        self.migrate_legacy_profiles_if_needed()?;
        let profile_id = parse_profile_id(&profile_id)?;
        let document = self.store().load().map_err(ServiceError::profiles)?;
        let profile = document
            .get(profile_id)
            .ok_or_else(|| ServiceError::not_found(profile_id))?;
        match models::load_cache(&self.paths.model_cache_dir, profile_id) {
            Ok(Some(cache)) => Ok(ModelListView {
                models: models_with_current(&cache.models, &profile.model),
                cache_label: format!("已缓存 {} 个模型，点击刷新可重新获取", cache.models.len()),
            }),
            Ok(None) => Ok(ModelListView {
                models: vec![profile.model.clone()],
                cache_label: "尚未获取模型列表".to_owned(),
            }),
            Err(_) => Ok(ModelListView {
                models: vec![profile.model.clone()],
                cache_label: "模型缓存不可用，可点击刷新重建".to_owned(),
            }),
        }
    }

    pub fn refresh_models(
        &self,
        profile_id: String,
        draft: ProfileDraft,
    ) -> Result<ModelListView, ServiceError> {
        let _operation = self.operation_guard()?;
        self.migrate_legacy_profiles_if_needed()?;
        let profile_id = parse_profile_id(&profile_id)?;
        let document = self.store().load().map_err(ServiceError::profiles)?;
        let existing = document
            .get(profile_id)
            .ok_or_else(|| ServiceError::not_found(profile_id))?;
        let fetched_from = profile_from_draft(profile_id, Some(existing), draft)?;
        let cache = models::fetch_models(&fetched_from).map_err(ServiceError::models)?;
        let current = self.store().load().map_err(ServiceError::profiles)?;
        let cache_owned = current
            .get(profile_id)
            .is_some_and(|saved| same_model_catalog(saved, &fetched_from));
        if cache_owned {
            models::save_cache(&self.paths.model_cache_dir, &cache)
                .map_err(ServiceError::models)?;
        }
        Ok(ModelListView {
            models: models_with_current(&cache.models, &fetched_from.model),
            cache_label: if cache_owned {
                format!("刚刚获取了 {} 个模型", cache.models.len())
            } else {
                format!("已获取 {} 个模型；保存连接后可缓存", cache.models.len())
            },
        })
    }

    pub fn prepare_restore(&self) -> Result<ApplyResponse, ServiceError> {
        let _operation = self.operation_guard()?;
        self.prepare_restore_inner()
    }

    fn prepare_restore_inner(&self) -> Result<ApplyResponse, ServiceError> {
        let report = process::detect_codex_processes();
        if report.is_clear() {
            return self.restore_latest(false, None);
        }
        let desktop_only = report.has_desktop() && !report.has_command_line();
        let token = self.remember(PendingConfirmation::RestoreProcess {
            desktop_executable: report.desktop_executable(),
            desktop_only,
        });
        Ok(ApplyResponse::RequiresConfirmation {
            confirmation: Confirmation {
                token,
                title: "Codex 正在运行".to_owned(),
                detail: if desktop_only {
                    "推荐先退出 Codex Desktop，恢复完成后工具会重新打开它。".to_owned()
                } else {
                    "请先结束 Codex 命令行任务，再恢复配置。".to_owned()
                },
                options: if desktop_only {
                    vec![
                        option(
                            "quit_desktop_and_restore",
                            "退出并恢复",
                            ConfirmationIntent::Primary,
                        ),
                        option("restore_anyway", "仍然恢复", ConfirmationIntent::Danger),
                    ]
                } else {
                    vec![
                        option("recheck_restore", "重新检测", ConfirmationIntent::Primary),
                        option("restore_anyway", "仍然恢复", ConfirmationIntent::Danger),
                    ]
                },
            },
        })
    }

    pub fn load_context(&self, profile_id: String) -> Result<ContextView, ServiceError> {
        let _operation = self.operation_guard()?;
        self.migrate_legacy_profiles_if_needed()?;
        let profile_id = parse_profile_id(&profile_id)?;
        let document = self.store().load().map_err(ServiceError::profiles)?;
        let profile = document
            .get(profile_id)
            .ok_or_else(|| ServiceError::not_found(profile_id))?;
        Ok(self.context_view(
            profile,
            self.active_profile_id(&document) == Some(profile_id),
            &document,
        ))
    }

    pub fn save_context(
        &self,
        profile_id: String,
        draft: ContextDraft,
    ) -> Result<ApplyResponse, ServiceError> {
        let _operation = self.operation_guard()?;
        self.migrate_legacy_profiles_if_needed()?;
        let profile_id = parse_profile_id(&profile_id)?;
        let context = context_from_draft(draft)?;
        let mut document = self.store().load().map_err(ServiceError::profiles)?;
        let active = self.active_profile_id(&document) == Some(profile_id);
        let profile = document
            .get_mut(profile_id)
            .ok_or_else(|| ServiceError::not_found(profile_id))?;
        profile.context = Some(context);
        document.validate().map_err(ServiceError::profiles)?;
        self.store()
            .save(&document)
            .map_err(ServiceError::profiles)?;

        if active {
            return self.prepare_context_sync(profile_id, context.into());
        }

        let profile = document
            .get(profile_id)
            .expect("profile was just updated in this document");
        Ok(ApplyResponse::ContextSaved {
            context: self.context_view(profile, false, &document),
            warning: None,
        })
    }

    pub fn refresh_usage(
        &self,
        profile_id: String,
        period: String,
    ) -> Result<UsageView, ServiceError> {
        let _operation = self.operation_guard()?;
        self.migrate_legacy_profiles_if_needed()?;
        let profile_id = parse_profile_id(&profile_id)?;
        let document = self.store().load().map_err(ServiceError::profiles)?;
        document
            .get(profile_id)
            .ok_or_else(|| ServiceError::not_found(profile_id))?;
        let period = usage_period_from_name(&period)?;
        let scope = profile_usage_scope(&self.paths, &document, profile_id)?;
        let store = UsageStore::new(self.paths.usage_database.clone());
        let report = store
            .refresh_scoped(
                &self.paths.codex_sessions,
                &self.paths.codex_archived_sessions,
                period,
                &scope,
            )
            .map_err(ServiceError::usage)?;
        Ok(UsageView::from_report(&report))
    }

    pub fn export_usage(&self, profile_id: String, period: String) -> Result<(), ServiceError> {
        let _operation = self.operation_guard()?;
        let profile_id = parse_profile_id(&profile_id)?;
        let period = usage_period_from_name(&period)?;
        let Some(path) = rfd::FileDialog::new()
            .add_filter("CSV", &["csv"])
            .set_file_name(format!(
                "codex-switch-usage-{}.csv",
                usage_period_name(period)
            ))
            .save_file()
        else {
            return Ok(());
        };
        let document = self.store().load().map_err(ServiceError::profiles)?;
        document
            .get(profile_id)
            .ok_or_else(|| ServiceError::not_found(profile_id))?;
        let scope = profile_usage_scope(&self.paths, &document, profile_id)?;
        let store = UsageStore::new(self.paths.usage_database.clone());
        let report = store
            .refresh_scoped(
                &self.paths.codex_sessions,
                &self.paths.codex_archived_sessions,
                period,
                &scope,
            )
            .map_err(ServiceError::usage)?;
        durable_fs::atomic_write(&path, report.model_distribution_csv().as_bytes())
            .map_err(ServiceError::filesystem)
    }

    pub fn prepare_apply(&self, profile_id: String) -> Result<ApplyResponse, ServiceError> {
        let _operation = self.operation_guard()?;
        self.migrate_legacy_profiles_if_needed()?;
        let profile_id = parse_profile_id(&profile_id)?;
        let document = self.store().load().map_err(ServiceError::profiles)?;
        let profile = document
            .get(profile_id)
            .ok_or_else(|| ServiceError::not_found(profile_id))?;
        let activation = profile.activation().map_err(ServiceError::domain)?;
        let report = process::detect_codex_processes();

        if report.is_clear() {
            return self.apply_activation(activation, ConflictPolicy::Reject, false, None);
        }

        let desktop_only = report.has_desktop() && !report.has_command_line();
        let pending = PendingConfirmation::Process {
            activation,
            desktop_executable: report.desktop_executable(),
            desktop_only,
        };
        Ok(ApplyResponse::RequiresConfirmation {
            confirmation: self.remember_process_confirmation(pending, desktop_only),
        })
    }

    pub fn continue_apply(
        &self,
        token: String,
        choice: String,
    ) -> Result<ApplyResponse, ServiceError> {
        let _operation = self.operation_guard()?;
        let token = Uuid::parse_str(&token).map_err(|_| ServiceError::invalid_confirmation())?;
        let pending = self
            .pending
            .lock()
            .map_err(|_| ServiceError::internal())?
            .remove(&token)
            .ok_or_else(ServiceError::invalid_confirmation)?;

        match pending {
            PendingConfirmation::Process {
                activation,
                desktop_executable,
                desktop_only,
            } => match choice.as_str() {
                "quit_desktop_and_apply" if desktop_only => {
                    process::quit_desktop_safely(Duration::from_secs(8))
                        .map_err(ServiceError::process)?;
                    self.apply_activation(
                        activation,
                        ConflictPolicy::Reject,
                        true,
                        desktop_executable,
                    )
                }
                "recheck" if !desktop_only => {
                    let report = process::detect_codex_processes();
                    if report.is_clear() {
                        self.apply_activation(activation, ConflictPolicy::Reject, false, None)
                    } else {
                        let next_desktop_only = report.has_desktop() && !report.has_command_line();
                        let pending = PendingConfirmation::Process {
                            activation,
                            desktop_executable: report.desktop_executable(),
                            desktop_only: next_desktop_only,
                        };
                        Ok(ApplyResponse::RequiresConfirmation {
                            confirmation: self
                                .remember_process_confirmation(pending, next_desktop_only),
                        })
                    }
                }
                "apply_anyway" => {
                    self.apply_activation(activation, ConflictPolicy::Reject, false, None)
                }
                _ => Err(ServiceError::invalid_confirmation()),
            },
            PendingConfirmation::Conflict {
                activation,
                desktop_executable,
                desktop_was_closed,
            } => match choice.as_str() {
                "overwrite" => self.apply_activation(
                    activation,
                    ConflictPolicy::Overwrite,
                    desktop_was_closed,
                    desktop_executable,
                ),
                "import_current" => self
                    .import_current_profile_with_relaunch(desktop_was_closed, desktop_executable),
                _ => Err(ServiceError::invalid_confirmation()),
            },
            PendingConfirmation::RestoreProcess {
                desktop_executable,
                desktop_only,
            } => match choice.as_str() {
                "quit_desktop_and_restore" if desktop_only => {
                    process::quit_desktop_safely(Duration::from_secs(8))
                        .map_err(ServiceError::process)?;
                    self.restore_latest(true, desktop_executable)
                }
                "recheck_restore" if !desktop_only => self.prepare_restore_inner(),
                "restore_anyway" => self.restore_latest(false, None),
                _ => Err(ServiceError::invalid_confirmation()),
            },
            PendingConfirmation::ContextProcess {
                profile_id,
                settings,
                desktop_executable,
                desktop_only,
            } => match choice.as_str() {
                "quit_desktop_and_sync" if desktop_only => {
                    process::quit_desktop_safely(Duration::from_secs(8))
                        .map_err(ServiceError::process)?;
                    self.sync_context(
                        profile_id,
                        settings,
                        ConflictPolicy::Reject,
                        true,
                        desktop_executable,
                    )
                }
                "recheck_sync" if !desktop_only => self.prepare_context_sync(profile_id, settings),
                "sync_anyway" => {
                    self.sync_context(profile_id, settings, ConflictPolicy::Reject, false, None)
                }
                _ => Err(ServiceError::invalid_confirmation()),
            },
            PendingConfirmation::ContextConflict {
                profile_id,
                settings,
                desktop_executable,
                desktop_was_closed,
            } => match choice.as_str() {
                "preserve_external_and_sync" => self.sync_context(
                    profile_id,
                    settings,
                    ConflictPolicy::Overwrite,
                    desktop_was_closed,
                    desktop_executable,
                ),
                "import_current" => self
                    .import_current_profile_with_relaunch(desktop_was_closed, desktop_executable),
                _ => Err(ServiceError::invalid_confirmation()),
            },
        }
    }

    pub fn dismiss_confirmation(&self, token: String) -> Result<(), ServiceError> {
        let _operation = self.operation_guard()?;
        let token = Uuid::parse_str(&token).map_err(|_| ServiceError::invalid_confirmation())?;
        self.pending
            .lock()
            .map_err(|_| ServiceError::internal())?
            .remove(&token);
        Ok(())
    }

    fn store(&self) -> ProfileStore {
        ProfileStore::new(self.paths.profiles.clone())
    }

    fn profile_summary(&self, profile: &Profile, document: &ProfilesDocument) -> ProfileSummary {
        let active_profile_id = self.active_profile_id(document);
        let (live_profile, baseline_matches) = self.live_apply_context();
        ProfileSummary::from_profile(
            profile,
            active_profile_id,
            live_profile.as_ref(),
            baseline_matches,
        )
    }

    fn live_apply_context(&self) -> (Option<Profile>, RelevantFingerprintMatch) {
        let live_profile = read_live_profile(&self.paths).ok();
        let baseline_matches = match TransactionManager::new(self.paths.clone()).load_state() {
            Ok(Some(state)) => current_fingerprint_match(&self.paths, &state.relevant_fingerprint),
            Ok(None) | Err(_) => RelevantFingerprintMatch::Unknown,
        };
        (live_profile, baseline_matches)
    }

    fn migrate_legacy_profiles_if_needed(&self) -> Result<(), ServiceError> {
        if self.paths.profiles.exists() {
            return Ok(());
        }
        let legacy = ProfileStore::new(self.legacy_paths.profiles.clone());
        let document = legacy.load().map_err(ServiceError::profiles)?;
        if !document.profiles.is_empty() {
            self.store()
                .save(&document)
                .map_err(ServiceError::profiles)?;
        }
        Ok(())
    }

    fn active_profile_id(&self, document: &ProfilesDocument) -> Option<ProfileId> {
        // A current managed provider carries the stable profile UUID. For imported or legacy
        // configurations, validate the transaction fingerprint before trusting saved state.
        if let Some(profile_id) = managed_live_profile_id(&self.paths) {
            return document.get(profile_id).map(|_| profile_id);
        }

        let live_profile = read_live_profile(&self.paths).ok()?;
        let transaction = TransactionManager::new(self.paths.clone());
        if let Ok(Some(state)) = transaction.load_state()
            && current_fingerprint_matches(&self.paths, &state.relevant_fingerprint)
            && let Some(profile_id) = state.active_profile_id
            && document
                .get(profile_id)
                .is_some_and(|profile| same_connection(profile, &live_profile))
        {
            return Some(profile_id);
        }

        document
            .profiles
            .iter()
            .find(|profile| same_connection(profile, &live_profile))
            .map(|profile| profile.id)
    }

    fn remember_process_confirmation(
        &self,
        pending: PendingConfirmation,
        desktop_only: bool,
    ) -> Confirmation {
        let token = self.remember(pending);
        let (detail, options) = if desktop_only {
            (
                "Codex Desktop 正在运行。先退出再切换可避免正在运行的会话继续使用旧配置。"
                    .to_owned(),
                vec![
                    option(
                        "quit_desktop_and_apply",
                        "退出并切换",
                        ConfirmationIntent::Primary,
                    ),
                    option("apply_anyway", "仍然切换", ConfirmationIntent::Danger),
                ],
            )
        } else {
            (
                "检测到 Codex 命令行任务或多个 Codex 进程。请先结束相关任务，避免会话继续使用旧配置。".to_owned(),
                vec![
                    option("recheck", "重新检测", ConfirmationIntent::Primary),
                    option("apply_anyway", "仍然切换", ConfirmationIntent::Danger),
                ],
            )
        };

        Confirmation {
            token,
            title: "Codex 正在运行".to_owned(),
            detail,
            options,
        }
    }

    fn apply_activation(
        &self,
        activation: Activation,
        policy: ConflictPolicy,
        desktop_was_closed: bool,
        desktop_executable: Option<PathBuf>,
    ) -> Result<ApplyResponse, ServiceError> {
        let manager = TransactionManager::new(self.paths.clone());
        let validator = CodexStagedValidator::discover_for_desktop(desktop_executable.as_deref());
        let staged_validator = validator
            .as_ref()
            .map(|validator| validator as &dyn crate::transaction::StagedValidator);
        let validation_skipped = staged_validator.is_none();

        match manager.apply_validated(&activation, policy, staged_validator) {
            Ok(outcome) => {
                let relaunch_warning = desktop_was_closed
                    .then(|| process::relaunch_desktop(desktop_executable.as_deref()).err())
                    .flatten()
                    .map(|error| format!("切换完成，但 Codex Desktop 未能重新打开：{error}"));
                let warning = relaunch_warning.or_else(|| {
                    validation_skipped
                        .then_some("切换完成；未找到 Codex 校验器，仅完成结构校验".to_owned())
                });
                Ok(ApplyResponse::Applied {
                    active_profile_id: outcome
                        .state
                        .active_profile_id
                        .map_or_else(String::new, |id| id.to_string()),
                    warning,
                })
            }
            Err(TransactionError::ExternalConflict(conflict)) => {
                let token = self.remember(PendingConfirmation::Conflict {
                    activation,
                    desktop_executable,
                    desktop_was_closed,
                });
                Ok(ApplyResponse::RequiresConfirmation {
                    confirmation: Confirmation {
                        token,
                        title: "检测到外部修改".to_owned(),
                        detail: format!(
                            "Codex 的中转站或模型配置已在工具外发生变化。{}",
                            if conflict.to_string().is_empty() {
                                String::new()
                            } else {
                                "请选择如何继续。".to_owned()
                            }
                        ),
                        options: vec![
                            option("import_current", "导入当前", ConfirmationIntent::Primary),
                            option("overwrite", "覆盖", ConfirmationIntent::Danger),
                        ],
                    },
                })
            }
            Err(error) => {
                let _ = desktop_was_closed
                    .then(|| process::relaunch_desktop(desktop_executable.as_deref()));
                Err(ServiceError::transaction(error))
            }
        }
    }

    fn prepare_context_sync(
        &self,
        profile_id: ProfileId,
        settings: crate::codex_config::ContextSettings,
    ) -> Result<ApplyResponse, ServiceError> {
        let report = process::detect_codex_processes();
        if report.is_clear() {
            return self.sync_context(profile_id, settings, ConflictPolicy::Reject, false, None);
        }
        let desktop_only = report.has_desktop() && !report.has_command_line();
        let token = self.remember(PendingConfirmation::ContextProcess {
            profile_id,
            settings,
            desktop_executable: report.desktop_executable(),
            desktop_only,
        });
        Ok(ApplyResponse::RequiresConfirmation {
            confirmation: Confirmation {
                token,
                title: "Codex 正在运行".to_owned(),
                detail: if desktop_only {
                    "推荐先退出 Codex Desktop。上下文同步完成后工具会重新打开它。".to_owned()
                } else {
                    "检测到 Codex 命令行任务或多个 Codex 进程。请先结束相关任务，避免会话继续使用旧上下文配置。".to_owned()
                },
                options: if desktop_only {
                    vec![
                        option(
                            "quit_desktop_and_sync",
                            "退出并同步",
                            ConfirmationIntent::Primary,
                        ),
                        option("sync_anyway", "仍然同步", ConfirmationIntent::Danger),
                    ]
                } else {
                    vec![
                        option("recheck_sync", "重新检测", ConfirmationIntent::Primary),
                        option("sync_anyway", "仍然同步", ConfirmationIntent::Danger),
                    ]
                },
            },
        })
    }

    fn sync_context(
        &self,
        profile_id: ProfileId,
        settings: crate::codex_config::ContextSettings,
        policy: ConflictPolicy,
        desktop_was_closed: bool,
        desktop_executable: Option<PathBuf>,
    ) -> Result<ApplyResponse, ServiceError> {
        let validator = CodexStagedValidator::discover_for_desktop(desktop_executable.as_deref());
        let staged_validator = validator
            .as_ref()
            .map(|validator| validator as &dyn crate::transaction::StagedValidator);
        let validation_skipped = staged_validator.is_none();
        match TransactionManager::new(self.paths.clone()).update_context_with_policy(
            settings,
            policy,
            staged_validator,
        ) {
            Ok(_) => {
                let warning = desktop_was_closed
                    .then(|| process::relaunch_desktop(desktop_executable.as_deref()).err())
                    .flatten()
                    .map(|error| format!("上下文已同步，但 Codex Desktop 未能重新打开：{error}"))
                    .or_else(|| {
                        validation_skipped.then_some(
                            "上下文已同步；未找到 Codex 校验器，仅完成结构校验".to_owned(),
                        )
                    });
                let document = self.store().load().map_err(ServiceError::profiles)?;
                let profile = document
                    .get(profile_id)
                    .ok_or_else(|| ServiceError::not_found(profile_id))?;
                Ok(ApplyResponse::ContextSaved {
                    context: self.context_view(profile, true, &document),
                    warning,
                })
            }
            Err(TransactionError::ExternalConflict(conflict)) => {
                let token = self.remember(PendingConfirmation::ContextConflict {
                    profile_id,
                    settings,
                    desktop_executable,
                    desktop_was_closed,
                });
                Ok(ApplyResponse::RequiresConfirmation {
                    confirmation: Confirmation {
                        token,
                        title: "检测到外部修改".to_owned(),
                        detail: format!(
                            "Codex 的中转站或模型配置已在工具外发生变化。{}",
                            if conflict.to_string().is_empty() {
                                String::new()
                            } else {
                                "请选择如何继续。".to_owned()
                            }
                        ),
                        options: vec![
                            option("import_current", "导入当前", ConfirmationIntent::Primary),
                            option(
                                "preserve_external_and_sync",
                                "保留外部并同步",
                                ConfirmationIntent::Danger,
                            ),
                        ],
                    },
                })
            }
            Err(error) => {
                let _ = desktop_was_closed
                    .then(|| process::relaunch_desktop(desktop_executable.as_deref()));
                Err(ServiceError::transaction(error))
            }
        }
    }

    fn context_view(
        &self,
        profile: &Profile,
        is_active: bool,
        profiles: &ProfilesDocument,
    ) -> ContextView {
        let live = is_active
            .then(|| live_context_settings(&self.paths))
            .flatten();
        let budget = self.context_budget(profile.id, profile.context, is_active, profiles);
        ContextView::from_profile(profile, is_active, live, budget)
    }

    fn context_budget(
        &self,
        profile_id: ProfileId,
        context: Option<ProfileContext>,
        is_active: bool,
        profiles: &ProfilesDocument,
    ) -> ContextBudgetView {
        let live_config = durable_fs::read_optional(&self.paths.codex_config)
            .ok()
            .flatten()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .unwrap_or_default();
        let report = is_active.then(|| {
            profile_usage_scope(&self.paths, profiles, profile_id)
                .ok()
                .and_then(|scope| {
                    UsageStore::new(self.paths.usage_database.clone())
                        .refresh_scoped(
                            &self.paths.codex_sessions,
                            &self.paths.codex_archived_sessions,
                            UsagePeriod::Today,
                            &scope,
                        )
                        .ok()
                })
        });
        let latest = report.flatten().and_then(|report| report.latest_context);
        let instructions = context::discover_instruction_sources(
            &self.paths.codex_dir,
            latest.as_ref().and_then(|usage| usage.cwd.as_deref()),
            &live_config,
        );
        let effective_window = latest
            .as_ref()
            .map(|usage| usage.model_context_window)
            .filter(|window| *window > 0)
            .or_else(|| context.and_then(|context| context.model_context_window));
        let active_context = latest.as_ref().map(|usage| usage.total_tokens).unwrap_or(0);
        let estimated_instructions = instructions.estimated_tokens.min(active_context);
        let other_input = active_context.saturating_sub(estimated_instructions);
        let (history_ratio, instruction_ratio, remaining_ratio) = match effective_window {
            Some(window) if window > 0 => {
                let capacity = window as f64;
                let history = (other_input as f64 / capacity).clamp(0.0, 1.0);
                let instruction =
                    (estimated_instructions as f64 / capacity).clamp(0.0, 1.0 - history);
                (
                    history as f32,
                    instruction as f32,
                    (1.0 - history - instruction) as f32,
                )
            }
            _ => (0.0, 0.0, 1.0),
        };
        ContextBudgetView {
            recent_session: latest
                .as_ref()
                .map(|usage| format_compact_tokens(usage.total_tokens))
                .unwrap_or_else(|| "暂无记录".to_owned()),
            instruction_tokens: if instructions.sources.is_empty() {
                "暂无记录".to_owned()
            } else {
                format_compact_tokens(instructions.estimated_tokens)
            },
            available_budget: effective_window
                .map(|window| format_compact_tokens(window.saturating_sub(active_context)))
                .unwrap_or_else(|| "自动".to_owned()),
            history_ratio,
            instruction_ratio,
            remaining_ratio,
            suggested_window_k: latest
                .as_ref()
                .map(|usage| usage.model_context_window)
                .filter(|window| *window > 0)
                .or_else(|| context.and_then(|context| context.model_context_window))
                .map(format_context_window_k)
                .unwrap_or_else(|| "272".to_owned()),
            instructions: instructions
                .sources
                .iter()
                .map(|source| InstructionView {
                    name: source.name.clone(),
                    detail: format!(
                        "约 {} tokens · {}",
                        format_compact_tokens(source.estimated_tokens),
                        instruction_source_label(source.scope, &source.path, &self.paths)
                    ),
                })
                .collect(),
        }
    }

    fn restore_latest(
        &self,
        desktop_was_closed: bool,
        desktop_executable: Option<PathBuf>,
    ) -> Result<ApplyResponse, ServiceError> {
        let result = TransactionManager::new(self.paths.clone()).restore_latest();
        match result {
            Ok(outcome) => {
                let warning = desktop_was_closed
                    .then(|| process::relaunch_desktop(desktop_executable.as_deref()).err())
                    .flatten()
                    .map(|error| format!("恢复完成，但 Codex Desktop 未能重新打开：{error}"));
                Ok(ApplyResponse::Restored {
                    active_profile_id: outcome.state.active_profile_id.map(|id| id.to_string()),
                    warning,
                })
            }
            Err(error) => {
                let _ = desktop_was_closed
                    .then(|| process::relaunch_desktop(desktop_executable.as_deref()));
                Err(ServiceError::transaction(error))
            }
        }
    }

    fn import_current_profile(&self) -> Result<ApplyResponse, ServiceError> {
        let config_bytes = durable_fs::read_optional(&self.paths.codex_config)
            .map_err(ServiceError::filesystem)?
            .ok_or_else(ServiceError::missing_config)?;
        let config = std::str::from_utf8(&config_bytes)
            .map_err(|_| ServiceError::invalid_config_encoding())?;
        let auth =
            durable_fs::read_optional(&self.paths.codex_auth).map_err(ServiceError::filesystem)?;
        let mut profile = codex_config::import_current_profile(config, auth.as_deref())
            .map_err(ServiceError::config)?;

        let mut document = self.store().load().map_err(ServiceError::profiles)?;
        document.remove(profile.id);
        profile.name = unique_name(&document, &profile.name);
        document
            .insert(profile.clone())
            .map_err(ServiceError::profiles)?;
        self.store()
            .save(&document)
            .map_err(ServiceError::profiles)?;
        let warning = TransactionManager::new(self.paths.clone())
            .adopt_current(Some(profile.id))
            .err()
            .map(|error| format!("已导入当前配置，但无法建立冲突检测基线：{error}"));

        Ok(ApplyResponse::ImportedCurrent {
            profile: self.profile_summary(&profile, &document),
            warning,
        })
    }

    fn import_current_profile_with_relaunch(
        &self,
        desktop_was_closed: bool,
        desktop_executable: Option<PathBuf>,
    ) -> Result<ApplyResponse, ServiceError> {
        let response = self.import_current_profile()?;
        let relaunch_warning = desktop_was_closed
            .then(|| process::relaunch_desktop(desktop_executable.as_deref()).err())
            .flatten()
            .map(|error| format!("已导入当前 Codex 配置，但 Codex Desktop 未能重新打开：{error}"));
        match response {
            ApplyResponse::ImportedCurrent { profile, warning } => {
                Ok(ApplyResponse::ImportedCurrent {
                    profile,
                    warning: relaunch_warning.or(warning),
                })
            }
            _ => unreachable!("importing the current profile has one response variant"),
        }
    }

    fn remember(&self, pending: PendingConfirmation) -> String {
        let token = Uuid::new_v4();
        self.pending
            .lock()
            .expect("pending confirmation mutex poisoned")
            .insert(token, pending);
        token.to_string()
    }
}

impl ProfileSummary {
    fn from_profile(
        profile: &Profile,
        active_profile_id: Option<ProfileId>,
        live_profile: Option<&Profile>,
        baseline_matches: RelevantFingerprintMatch,
    ) -> Self {
        let is_active = active_profile_id == Some(profile.id);
        let apply_state = if !is_active {
            ProfileApplyState::Inactive
        } else {
            match live_profile {
                Some(live) if same_connection(profile, live) => ProfileApplyState::Applied,
                Some(_) => match baseline_matches {
                    RelevantFingerprintMatch::Exact => ProfileApplyState::PendingChanges,
                    RelevantFingerprintMatch::Mismatch => ProfileApplyState::ExternalDrift,
                    RelevantFingerprintMatch::LegacyPreContext
                    | RelevantFingerprintMatch::Unknown => ProfileApplyState::Unknown,
                },
                None => ProfileApplyState::Unknown,
            }
        };
        Self {
            id: profile.id.to_string(),
            name: profile.name.clone(),
            base_url: profile.base_url.clone(),
            model: profile.model.clone(),
            review_model: profile.review_model.clone(),
            has_api_key: profile.api_key.is_some(),
            is_active,
            apply_state,
        }
    }
}

impl ContextView {
    fn from_profile(
        profile: &Profile,
        is_active: bool,
        live_context: Option<crate::codex_config::ContextSettings>,
        budget: ContextBudgetView,
    ) -> Self {
        let saved_context = profile.context;
        let context =
            saved_context.unwrap_or_else(|| live_context.map(Into::into).unwrap_or_default());
        let use_defaults = context == ProfileContext::default();
        let window_k = context
            .model_context_window
            .map(format_context_window_k)
            .unwrap_or_default();
        let compact_percent = context
            .model_auto_compact_token_limit
            .zip(context.model_context_window)
            .and_then(|(limit, window)| rounded_percent(limit, window))
            .and_then(|percent| u8::try_from(percent).ok())
            .map(|percent| percent.clamp(50, 95))
            .unwrap_or(80);
        Self {
            use_defaults,
            window_k,
            compact_percent,
            summary: context_summary(context),
            is_active,
            sync_state: if !is_active {
                "saved_for_switch".to_owned()
            } else if saved_context.is_none() {
                "inherited_live".to_owned()
            } else if live_context.is_some_and(|live| ProfileContext::from(live) == context) {
                "synced".to_owned()
            } else {
                "unsynced".to_owned()
            },
            status: if !is_active {
                "上下文配置 · 已保存，切换后生效".to_owned()
            } else if saved_context.is_none() {
                "上下文配置 · 沿用当前 Codex 配置".to_owned()
            } else if live_context.is_some_and(|live| ProfileContext::from(live) == context) {
                "上下文配置 · 已同步到 Codex".to_owned()
            } else {
                "上下文配置 · 尚未同步到 Codex".to_owned()
            },
            budget,
        }
    }
}

impl UsageView {
    fn from_report(report: &crate::usage::UsageReport) -> Self {
        let input_peak = report
            .trend
            .iter()
            .map(|point| point.usage.input_tokens)
            .max()
            .unwrap_or(0);
        let output_peak = report
            .trend
            .iter()
            .map(|point| point.usage.output_tokens)
            .max()
            .unwrap_or(0);
        let selected_day = report.daily.last().map(|day| day.usage).unwrap_or_default();
        Self {
            period: usage_period_name(report.period).to_owned(),
            current: UsageValue::from_usage(selected_day),
            today_summary: if selected_day.calls == 0 {
                "今日暂无本地记录".to_owned()
            } else {
                format!(
                    "{} tokens · {} 次调用",
                    format_compact_tokens(selected_day.total_tokens()),
                    selected_day.calls
                )
            },
            period_total: UsageValue::from_usage(report.current),
            trend: report
                .trend
                .iter()
                .map(|point| UsageTrend {
                    label: point.label.clone(),
                    input: format_compact_tokens(point.usage.input_tokens),
                    output: format_compact_tokens(point.usage.output_tokens),
                    input_ratio: ratio(point.usage.input_tokens, input_peak),
                    output_ratio: ratio(point.usage.output_tokens, output_peak),
                    usage: UsageValue::from_usage(point.usage),
                })
                .collect(),
            models: report
                .model_distribution
                .iter()
                .map(|model| UsageModel {
                    model: model.model.clone(),
                    input: format_compact_tokens(model.usage.input_tokens),
                    cached: format_compact_tokens(model.usage.cached_input_tokens),
                    output: format_compact_tokens(model.usage.output_tokens),
                    calls: format!("{} 次", model.usage.calls),
                })
                .collect(),
            has_data: report.current.calls > 0,
            status: if report.current.calls == 0 {
                "暂无用量数据".to_owned()
            } else {
                "本地用量数据已读取".to_owned()
            },
        }
    }
}

fn ratio(value: u64, peak: u64) -> f32 {
    if peak == 0 {
        0.0
    } else {
        (value as f64 / peak as f64).clamp(0.0, 1.0) as f32
    }
}

impl UsageValue {
    fn from_usage(usage: crate::usage::TokenUsage) -> Self {
        Self {
            input: format_compact_tokens(usage.input_tokens),
            cached: format_compact_tokens(usage.cached_input_tokens),
            output: format_compact_tokens(usage.output_tokens),
            calls: format!("{} 次", usage.calls),
        }
    }
}

fn profile_from_draft(
    profile_id: ProfileId,
    existing: Option<&Profile>,
    draft: ProfileDraft,
) -> Result<Profile, ServiceError> {
    let api_key = if draft.clear_api_key {
        None
    } else {
        match draft.api_key.filter(|value| !value.is_empty()) {
            Some(value) => Some(ApiKey::new(value).map_err(ServiceError::domain)?),
            None => existing.and_then(|profile| profile.api_key.clone()),
        }
    };
    let review_model = draft
        .review_model
        .and_then(|value| (!value.trim().is_empty()).then_some(value.trim().to_owned()));
    let profile = Profile {
        id: profile_id,
        name: draft.name.trim().to_owned(),
        base_url: draft.base_url.trim().to_owned(),
        api_key,
        model: draft.model.trim().to_owned(),
        review_model,
        // V1 creates a profile with explicit default context settings. Applying a fresh profile
        // must remove any context overrides left by the previously active relay.
        context: existing
            .and_then(|profile| profile.context)
            .or_else(|| existing.is_none().then_some(ProfileContext::default())),
    };
    profile.validate().map_err(ServiceError::domain)?;
    Ok(profile)
}

fn parse_profile_id(value: &str) -> Result<ProfileId, ServiceError> {
    value
        .parse()
        .map_err(|_| ServiceError::invalid_profile_id())
}

fn managed_live_profile_id(paths: &AppPaths) -> Option<ProfileId> {
    let config = durable_fs::read_optional(&paths.codex_config)
        .ok()
        .flatten()?;
    let config = std::str::from_utf8(&config).ok()?;
    codex_config::inspect_codex_config(config)
        .ok()?
        .model_provider
        .as_deref()
        .and_then(profile_id_from_provider_id)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RelevantFingerprintMatch {
    Exact,
    LegacyPreContext,
    Mismatch,
    Unknown,
}

fn current_fingerprint_match(paths: &AppPaths, expected: &str) -> RelevantFingerprintMatch {
    let config_bytes = match durable_fs::read_optional(&paths.codex_config) {
        Ok(Some(config)) => config,
        Ok(None) | Err(_) => return RelevantFingerprintMatch::Unknown,
    };
    let config = match std::str::from_utf8(&config_bytes) {
        Ok(config) => config,
        Err(_) => return RelevantFingerprintMatch::Unknown,
    };
    let auth = match durable_fs::read_optional(&paths.codex_auth) {
        Ok(auth) => auth,
        Err(_) => return RelevantFingerprintMatch::Unknown,
    };
    let projection = match codex_config::relevant_projection(config, auth.as_deref()) {
        Ok(projection) => projection,
        Err(_) => return RelevantFingerprintMatch::Unknown,
    };
    let fingerprint = match codex_config::relevant_fingerprint(config, auth.as_deref()) {
        Ok(fingerprint) => fingerprint,
        Err(_) => return RelevantFingerprintMatch::Unknown,
    };
    if fingerprint == expected {
        return RelevantFingerprintMatch::Exact;
    }
    match codex_config::pre_context_relevant_fingerprint(&projection) {
        Ok(fingerprint) if fingerprint == expected => RelevantFingerprintMatch::LegacyPreContext,
        Ok(_) => RelevantFingerprintMatch::Mismatch,
        Err(_) => RelevantFingerprintMatch::Unknown,
    }
}

fn current_fingerprint_matches(paths: &AppPaths, expected: &str) -> bool {
    matches!(
        current_fingerprint_match(paths, expected),
        RelevantFingerprintMatch::Exact | RelevantFingerprintMatch::LegacyPreContext
    )
}

fn same_connection(left: &Profile, right: &Profile) -> bool {
    left.name == right.name
        && left.base_url == right.base_url
        && left.api_key == right.api_key
        && left.model == right.model
        && left.review_model == right.review_model
        && left
            .context
            .is_none_or(|context| right.context == Some(context))
}

fn unique_name(document: &ProfilesDocument, preferred: &str) -> String {
    if !document
        .profiles
        .iter()
        .any(|profile| profile.name.eq_ignore_ascii_case(preferred))
    {
        return preferred.to_owned();
    }

    for suffix in 2.. {
        let candidate = format!("{preferred} {suffix}");
        if !document
            .profiles
            .iter()
            .any(|profile| profile.name.eq_ignore_ascii_case(&candidate))
        {
            return candidate;
        }
    }
    unreachable!("an infinite sequence of profile names cannot all be occupied")
}

fn same_model_catalog(left: &Profile, right: &Profile) -> bool {
    left.base_url == right.base_url && left.api_key == right.api_key
}

fn models_with_current(models: &[String], current: &str) -> Vec<String> {
    let mut result = models.to_vec();
    if !current.is_empty() && !result.iter().any(|model| model == current) {
        result.push(current.to_owned());
        result.sort();
    }
    result
}

fn profile_usage_scope(
    paths: &AppPaths,
    profiles: &ProfilesDocument,
    selected: ProfileId,
) -> Result<UsageScope, ServiceError> {
    let transaction = TransactionManager::new(paths.clone());
    let timeline = transaction
        .legacy_usage_history()
        .map(|history| reconstruct_legacy_usage(&history))
        .unwrap_or_default();
    let store = UsageStore::new(paths.usage_database.clone());
    let durable_windows = store
        .remember_legacy_windows(&timeline.durable_windows)
        .unwrap_or_else(|_| timeline.durable_windows.clone());
    let mut windows = durable_windows;
    windows.extend(timeline.live_windows.iter().copied());
    windows = normalize_profile_windows(windows);
    if timeline.live_windows.is_empty()
        && let Some(window) = inferred_live_legacy_window(paths, &transaction, profiles, &windows)
    {
        windows.push(window);
    }
    let windows = normalize_profile_windows(windows);
    let known_windows = windows
        .iter()
        .map(|window| LegacyUsageWindow::new(window.start_unix_ms, window.end_exclusive_unix_ms))
        .collect();
    let selected_windows = windows
        .iter()
        .filter(|window| window.profile_id == selected)
        .map(|window| LegacyUsageWindow::new(window.start_unix_ms, window.end_exclusive_unix_ms))
        .collect();
    Ok(UsageScope::profile(
        provider_id_for_profile(selected),
        TOOL_PROVIDER_ID,
        selected_windows,
        known_windows,
    ))
}

fn inferred_live_legacy_window(
    paths: &AppPaths,
    transaction: &TransactionManager,
    profiles: &ProfilesDocument,
    known_windows: &[ProfileLegacyUsageWindow],
) -> Option<ProfileLegacyUsageWindow> {
    let config_bytes = durable_fs::read_optional(&paths.codex_config)
        .ok()
        .flatten()?;
    let config = std::str::from_utf8(&config_bytes).ok()?;
    if codex_config::inspect_codex_config(config)
        .ok()?
        .model_provider
        .as_deref()
        != Some(TOOL_PROVIDER_ID)
    {
        return None;
    }
    let active_profile_id = transaction.load_state().ok().flatten()?.active_profile_id?;
    let live_profile = read_live_profile(paths).ok()?;
    let config_modified_at_unix_ms = config_modified_unix_ms(&paths.codex_config)?;
    infer_unvalidated_live_legacy_window(
        active_profile_id,
        Some(&live_profile),
        profiles,
        config_modified_at_unix_ms,
        known_windows,
    )
}

fn infer_unvalidated_live_legacy_window(
    active_profile_id: ProfileId,
    live_profile: Option<&Profile>,
    profiles: &ProfilesDocument,
    config_modified_at_unix_ms: u64,
    known_windows: &[ProfileLegacyUsageWindow],
) -> Option<ProfileLegacyUsageWindow> {
    if known_windows
        .iter()
        .any(|window| window.end_exclusive_unix_ms == u64::MAX)
    {
        return None;
    }
    let live_profile = live_profile?;
    let matching_profiles: Vec<_> = profiles
        .profiles
        .iter()
        .filter(|profile| same_legacy_usage_relay(profile, live_profile))
        .collect();
    let [matching_profile] = matching_profiles.as_slice() else {
        return None;
    };
    if matching_profile.id != active_profile_id {
        return None;
    }
    let known_history_end = known_windows
        .iter()
        .map(|window| window.end_exclusive_unix_ms)
        .max()
        .unwrap_or_default();
    Some(ProfileLegacyUsageWindow {
        profile_id: active_profile_id,
        start_unix_ms: config_modified_at_unix_ms.max(known_history_end),
        end_exclusive_unix_ms: u64::MAX,
    })
}

fn read_live_profile(paths: &AppPaths) -> Result<Profile, ServiceError> {
    let config_bytes = durable_fs::read_optional(&paths.codex_config)
        .map_err(ServiceError::filesystem)?
        .ok_or_else(ServiceError::missing_config)?;
    let config =
        std::str::from_utf8(&config_bytes).map_err(|_| ServiceError::invalid_config_encoding())?;
    let auth = durable_fs::read_optional(&paths.codex_auth).map_err(ServiceError::filesystem)?;
    codex_config::import_current_profile(config, auth.as_deref()).map_err(ServiceError::config)
}

fn live_context_settings(paths: &AppPaths) -> Option<crate::codex_config::ContextSettings> {
    let bytes = durable_fs::read_optional(&paths.codex_config)
        .ok()
        .flatten()?;
    let config = std::str::from_utf8(&bytes).ok()?;
    codex_config::inspect_context_settings(config).ok()
}

fn config_modified_unix_ms(path: &Path) -> Option<u64> {
    fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis()
        .try_into()
        .ok()
}

fn same_legacy_usage_relay(left: &Profile, right: &Profile) -> bool {
    left.name == right.name && left.base_url == right.base_url && left.api_key == right.api_key
}

fn instruction_source_label(scope: InstructionScope, path: &Path, paths: &AppPaths) -> String {
    match scope {
        InstructionScope::Global => path
            .strip_prefix(&paths.home_dir)
            .map(|relative| format!("~/{}", relative.display()))
            .unwrap_or_else(|_| "全局生效".to_owned()),
        InstructionScope::Project => path
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| format!("项目 · {name}"))
            .unwrap_or_else(|| "项目指令".to_owned()),
    }
}

fn context_from_draft(draft: ContextDraft) -> Result<ProfileContext, ServiceError> {
    if draft.use_defaults {
        return Ok(ProfileContext::default());
    }
    let window = parse_context_window_k(&draft.window_k)?;
    let percent = u64::from(draft.compact_percent.clamp(50, 95));
    let context = ProfileContext {
        model_context_window: Some(window),
        model_auto_compact_token_limit: Some(compact_limit_for_percent(window, percent)),
        model_auto_compact_token_limit_scope: Some(AutoCompactScope::Total),
    };
    context.validate().map_err(ServiceError::domain)?;
    Ok(context)
}

fn parse_context_window_k(input: &str) -> Result<u64, ServiceError> {
    const MIN_CONTEXT_WINDOW_TOKENS: u64 = 20;
    let input = input.trim();
    let mut parts = input.split('.');
    let whole = parts.next().ok_or_else(ServiceError::invalid_context)?;
    let fraction = parts.next();
    if parts.next().is_some()
        || whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(ServiceError::invalid_context());
    }
    let whole_tokens = whole
        .parse::<u64>()
        .map_err(|_| ServiceError::invalid_context())?
        .checked_mul(1_000)
        .ok_or_else(ServiceError::invalid_context)?;
    let fraction_tokens = match fraction {
        None => 0,
        Some(fraction)
            if !fraction.is_empty()
                && fraction.len() <= 3
                && fraction.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            let value = fraction
                .parse::<u64>()
                .map_err(|_| ServiceError::invalid_context())?;
            value
                * match fraction.len() {
                    1 => 100,
                    2 => 10,
                    3 => 1,
                    _ => unreachable!(),
                }
        }
        Some(_) => return Err(ServiceError::invalid_context()),
    };
    let tokens = whole_tokens
        .checked_add(fraction_tokens)
        .ok_or_else(ServiceError::invalid_context)?;
    (tokens >= MIN_CONTEXT_WINDOW_TOKENS)
        .then_some(tokens)
        .ok_or_else(ServiceError::invalid_context)
}

fn format_context_window_k(tokens: u64) -> String {
    let whole = tokens / 1_000;
    let remainder = tokens % 1_000;
    if remainder == 0 {
        return whole.to_string();
    }
    let fraction = format!("{remainder:03}");
    format!("{whole}.{}", fraction.trim_end_matches('0'))
}

fn rounded_percent(value: u64, total: u64) -> Option<u64> {
    if total == 0 {
        return None;
    }
    let percent = (u128::from(value) * 100 + u128::from(total) / 2) / u128::from(total);
    u64::try_from(percent).ok()
}

fn compact_limit_for_percent(window: u64, percent: u64) -> u64 {
    (u128::from(window) * u128::from(percent) / 100) as u64
}

fn context_summary(context: ProfileContext) -> String {
    let window = context
        .model_context_window
        .map(|value| format!("{}K 窗口", format_context_window_k(value)))
        .unwrap_or_else(|| "自动窗口".to_owned());
    let compact = match (
        context.model_auto_compact_token_limit,
        context.model_context_window,
    ) {
        (Some(limit), Some(window)) if window > 0 => {
            format!("压缩 {}%", rounded_percent(limit, window).unwrap_or(0))
        }
        _ => "自动压缩".to_owned(),
    };
    format!("{window} · 输出不限 · {compact}")
}

fn usage_period_from_name(value: &str) -> Result<UsagePeriod, ServiceError> {
    match value {
        "today" => Ok(UsagePeriod::Today),
        "last_7_days" => Ok(UsagePeriod::Last7Days),
        "last_30_days" => Ok(UsagePeriod::Last30Days),
        _ => Err(ServiceError::invalid_usage_period()),
    }
}

fn usage_period_name(period: UsagePeriod) -> &'static str {
    match period {
        UsagePeriod::Today => "today",
        UsagePeriod::Last7Days => "last_7_days",
        UsagePeriod::Last30Days => "last_30_days",
    }
}

fn format_compact_tokens(value: u64) -> String {
    if value < 1_000 {
        return value.to_string();
    }
    if value < 1_000_000 {
        return format!("{:.1}K", value as f64 / 1_000.0)
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_owned();
    }
    format!("{:.1}M", value as f64 / 1_000_000.0)
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_owned()
}

fn option(id: &str, label: &str, intent: ConfirmationIntent) -> ConfirmationOption {
    ConfirmationOption {
        id: id.to_owned(),
        label: label.to_owned(),
        intent,
    }
}

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("无法确定当前用户目录")]
    Paths,
    #[error("中转站数据无法读取或保存")]
    Profiles,
    #[error("中转站信息不完整：{0}")]
    Domain(String),
    #[error("找不到所选中转站")]
    NotFound,
    #[error("中转站标识无效")]
    InvalidProfileId,
    #[error("确认请求已失效，请重新操作")]
    InvalidConfirmation,
    #[error("内部状态暂时不可用，请重新操作")]
    Internal,
    #[error("Codex 配置不存在")]
    MissingConfig,
    #[error("Codex 配置不是有效的 UTF-8")]
    InvalidConfigEncoding,
    #[error("无法读取 Codex 配置")]
    Filesystem,
    #[error("当前 Codex 配置无法导入")]
    Config,
    #[error("无法安全地结束或重新打开 Codex Desktop")]
    Process,
    #[error("切换失败，未提交新配置")]
    Transaction,
    #[error("上下文窗口请输入不小于 0.02K 的有效数值")]
    InvalidContext,
    #[error("统计范围无效")]
    InvalidUsagePeriod,
    #[error("无法读取本地用量数据：{0}")]
    Usage(String),
    #[error("模型列表无法读取或刷新")]
    Models,
}

impl ServiceError {
    fn paths(_error: impl std::fmt::Display) -> Self {
        Self::Paths
    }

    fn profiles(_error: impl std::fmt::Display) -> Self {
        Self::Profiles
    }

    fn domain(error: impl std::fmt::Display) -> Self {
        Self::Domain(error.to_string())
    }

    fn not_found(_profile_id: ProfileId) -> Self {
        Self::NotFound
    }

    fn invalid_profile_id() -> Self {
        Self::InvalidProfileId
    }

    fn invalid_confirmation() -> Self {
        Self::InvalidConfirmation
    }

    fn internal() -> Self {
        Self::Internal
    }

    fn missing_config() -> Self {
        Self::MissingConfig
    }

    fn invalid_config_encoding() -> Self {
        Self::InvalidConfigEncoding
    }

    fn filesystem(_error: impl std::fmt::Display) -> Self {
        Self::Filesystem
    }

    fn config(_error: impl std::fmt::Display) -> Self {
        Self::Config
    }

    fn process(_error: impl std::fmt::Display) -> Self {
        Self::Process
    }

    fn transaction(_error: impl std::fmt::Display) -> Self {
        Self::Transaction
    }

    fn invalid_context() -> Self {
        Self::InvalidContext
    }

    fn invalid_usage_period() -> Self {
        Self::InvalidUsagePeriod
    }

    fn usage(error: impl std::fmt::Display) -> Self {
        Self::Usage(error.to_string())
    }

    fn models(_error: impl std::fmt::Display) -> Self {
        Self::Models
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> (tempfile::TempDir, AppService) {
        let home = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_home(home.path());
        (home, AppService::with_paths(paths.clone(), paths))
    }

    fn draft(name: &str) -> ProfileDraft {
        ProfileDraft {
            name: name.to_owned(),
            base_url: "https://relay.example/v1".to_owned(),
            api_key: Some("sk-v2-test".to_owned()),
            clear_api_key: false,
            model: "gpt-5.2-codex".to_owned(),
            review_model: None,
        }
    }

    #[test]
    fn saved_profiles_never_expose_api_keys_in_bootstrap() {
        let (_home, service) = service();

        let created = service.create_profile(draft("Relay A")).unwrap();
        let bootstrap = service.bootstrap().unwrap();

        assert!(created.has_api_key);
        assert_eq!(bootstrap.profiles.len(), 1);
        assert!(bootstrap.profiles[0].has_api_key);
        assert!(!format!("{bootstrap:?}").contains("sk-v2-test"));
    }

    #[test]
    fn updating_without_a_key_preserves_the_saved_secret() {
        let (_home, service) = service();
        let created = service.create_profile(draft("Relay A")).unwrap();
        let mut update = draft("Relay B");
        update.api_key = None;

        let updated = service.update_profile(created.id, update).unwrap();

        assert_eq!(updated.name, "Relay B");
        assert!(updated.has_api_key);
    }

    #[test]
    fn explicit_clear_removes_a_saved_api_key() {
        let (_home, service) = service();
        let created = service.create_profile(draft("Relay A")).unwrap();
        let mut update = draft("Relay A");
        update.api_key = None;
        update.clear_api_key = true;

        let updated = service.update_profile(created.id.clone(), update).unwrap();

        assert!(!updated.has_api_key);
        let document = service.store().load().unwrap();
        let profile = document.get(created.id.parse().unwrap()).unwrap();
        assert!(profile.api_key.is_none());
    }

    #[test]
    fn bootstrap_reads_profiles_from_the_shared_v1_data_directory() {
        let (home, service) = service();
        let legacy = ProfileStore::new(home.path().join(".codex-switch/profiles.toml"));
        let mut document = ProfilesDocument::default();
        document
            .insert(profile_from_draft(ProfileId::new(), None, draft("Legacy")).unwrap())
            .unwrap();
        legacy.save(&document).unwrap();

        let bootstrap = service.bootstrap().unwrap();

        assert_eq!(bootstrap.profiles[0].name, "Legacy");
        assert!(home.path().join(".codex-switch/profiles.toml").exists());
    }

    #[test]
    fn live_connection_beats_a_stale_but_matching_transaction_state() {
        let (_home, service) = service();
        let mut first = profile_from_draft(ProfileId::new(), None, draft("Relay A")).unwrap();
        let mut second = profile_from_draft(ProfileId::new(), None, draft("Relay B")).unwrap();
        first.context = None;
        second.context = None;

        let mut document = ProfilesDocument::default();
        document.insert(first.clone()).unwrap();
        document.insert(second.clone()).unwrap();
        service.store().save(&document).unwrap();

        let config = r#"
model_provider = "relay"
model = "gpt-5.2-codex"

[model_providers.relay]
name = "Relay B"
base_url = "https://relay.example/v1"
wire_api = "responses"
requires_openai_auth = true
"#;
        durable_fs::atomic_write(&service.paths.codex_config, config.as_bytes()).unwrap();
        durable_fs::atomic_write(
            &service.paths.codex_auth,
            br#"{"OPENAI_API_KEY":"sk-v2-test"}"#,
        )
        .unwrap();
        TransactionManager::new(service.paths.clone())
            .adopt_current(Some(first.id))
            .unwrap();

        assert_eq!(service.active_profile_id(&document), Some(second.id));
    }

    #[test]
    fn active_profile_reports_saved_changes_as_pending_until_reapplied() {
        let (_home, service) = service();
        let created = service.create_profile(draft("Relay A")).unwrap();
        let profile_id: ProfileId = created.id.parse().unwrap();
        let document = service.store().load().unwrap();
        TransactionManager::new(service.paths.clone())
            .apply(
                &document.get(profile_id).unwrap().activation().unwrap(),
                ConflictPolicy::Reject,
            )
            .unwrap();

        let applied = service.bootstrap().unwrap().profiles.remove(0);
        assert!(applied.is_active);
        assert_eq!(applied.apply_state, ProfileApplyState::Applied);

        let mut update = draft("Relay A");
        update.model = "gpt-5.6-sol".to_owned();
        let pending = service.update_profile(created.id, update).unwrap();
        assert!(pending.is_active);
        assert_eq!(pending.apply_state, ProfileApplyState::PendingChanges);

        let document = service.store().load().unwrap();
        TransactionManager::new(service.paths.clone())
            .apply(
                &document.get(profile_id).unwrap().activation().unwrap(),
                ConflictPolicy::Reject,
            )
            .unwrap();
        let reapplied = service.bootstrap().unwrap().profiles.remove(0);
        assert_eq!(reapplied.apply_state, ProfileApplyState::Applied);
    }

    #[test]
    fn active_profile_distinguishes_external_drift_from_saved_changes() {
        let (_home, service) = service();
        let created = service.create_profile(draft("Relay A")).unwrap();
        let profile_id: ProfileId = created.id.parse().unwrap();
        let document = service.store().load().unwrap();
        TransactionManager::new(service.paths.clone())
            .apply(
                &document.get(profile_id).unwrap().activation().unwrap(),
                ConflictPolicy::Reject,
            )
            .unwrap();
        let external_config = fs::read_to_string(&service.paths.codex_config)
            .unwrap()
            .replace("model = \"gpt-5.2-codex\"", "model = \"external-model\"");
        durable_fs::atomic_write(&service.paths.codex_config, external_config.as_bytes()).unwrap();

        let drifted = service.bootstrap().unwrap().profiles.remove(0);

        assert!(drifted.is_active);
        assert_eq!(drifted.apply_state, ProfileApplyState::ExternalDrift);
    }

    #[test]
    fn legacy_pre_context_state_reports_context_differences_as_unknown() {
        let (_home, service) = service();
        let created = service.create_profile(draft("Relay A")).unwrap();
        let profile_id: ProfileId = created.id.parse().unwrap();
        let document = service.store().load().unwrap();
        let manager = TransactionManager::new(service.paths.clone());
        manager
            .apply(
                &document.get(profile_id).unwrap().activation().unwrap(),
                ConflictPolicy::Reject,
            )
            .unwrap();

        let config = fs::read_to_string(&service.paths.codex_config).unwrap();
        let auth = fs::read(&service.paths.codex_auth).unwrap();
        let projection = codex_config::relevant_projection(&config, Some(&auth)).unwrap();
        let mut state = manager.load_state().unwrap().unwrap();
        state.relevant_fingerprint =
            codex_config::pre_context_relevant_fingerprint(&projection).unwrap();
        durable_fs::atomic_write(
            &service.paths.state,
            &serde_json::to_vec_pretty(&state).unwrap(),
        )
        .unwrap();

        let external_config = codex_config::patch_context_settings(
            &config,
            codex_config::ContextSettings {
                model_context_window: Some(272_000),
                model_auto_compact_token_limit: Some(217_600),
                model_auto_compact_token_limit_scope: Some(AutoCompactScope::Total),
            },
        )
        .unwrap();
        durable_fs::atomic_write(&service.paths.codex_config, external_config.as_bytes()).unwrap();

        let summary = service.bootstrap().unwrap().profiles.remove(0);

        assert!(summary.is_active);
        assert_eq!(summary.apply_state, ProfileApplyState::Unknown);
    }

    #[test]
    fn unreadable_live_credentials_report_an_unknown_apply_state() {
        let (_home, service) = service();
        let created = service.create_profile(draft("Relay A")).unwrap();
        let profile_id: ProfileId = created.id.parse().unwrap();
        let document = service.store().load().unwrap();
        TransactionManager::new(service.paths.clone())
            .apply(
                &document.get(profile_id).unwrap().activation().unwrap(),
                ConflictPolicy::Reject,
            )
            .unwrap();
        durable_fs::atomic_write(&service.paths.codex_auth, b"not-json").unwrap();

        let summary = service.bootstrap().unwrap().profiles.remove(0);

        assert!(summary.is_active);
        assert_eq!(summary.apply_state, ProfileApplyState::Unknown);
    }

    #[test]
    fn profile_summary_serializes_the_apply_state_contract() {
        for (apply_state, expected) in [
            (ProfileApplyState::Inactive, "inactive"),
            (ProfileApplyState::Applied, "applied"),
            (ProfileApplyState::PendingChanges, "pending_changes"),
            (ProfileApplyState::ExternalDrift, "external_drift"),
            (ProfileApplyState::Unknown, "unknown"),
        ] {
            let value = serde_json::to_value(ProfileSummary {
                id: "profile-id".to_owned(),
                name: "Relay A".to_owned(),
                base_url: "https://relay.example/v1".to_owned(),
                model: "gpt-5.2-codex".to_owned(),
                review_model: None,
                has_api_key: true,
                is_active: apply_state != ProfileApplyState::Inactive,
                apply_state,
            })
            .unwrap();

            assert_eq!(value["applyState"], expected);
            assert!(value.get("isApplied").is_none());
        }
    }

    #[test]
    fn prepare_restore_accepts_legacy_backup_without_catalog_metadata() {
        let (_home, service) = service();
        let original_config = b"model = \"gpt-5.6-sol\"\n";
        let original_auth = br#"{"OPENAI_API_KEY":"sk-old"}"#;
        durable_fs::atomic_write(&service.paths.codex_config, original_config).unwrap();
        durable_fs::atomic_write(&service.paths.codex_auth, original_auth).unwrap();

        let created = service.create_profile(draft("Relay A")).unwrap();
        let profile_id: ProfileId = created.id.parse().unwrap();
        let document = service.store().load().unwrap();
        let activation = document.get(profile_id).unwrap().activation().unwrap();
        let applied = TransactionManager::new(service.paths.clone())
            .apply(&activation, ConflictPolicy::Reject)
            .unwrap();
        let manifest_path = service
            .paths
            .backups_dir
            .join(applied.backup.id.to_string())
            .join("manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest
            .as_object_mut()
            .expect("backup manifest must be an object")
            .remove("catalog");
        durable_fs::atomic_write(
            &manifest_path,
            &serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let response = service.prepare_restore().unwrap();
        let response = match response {
            ApplyResponse::RequiresConfirmation { confirmation } => service
                .continue_apply(confirmation.token, "restore_anyway".to_owned())
                .unwrap(),
            response => response,
        };

        assert!(matches!(response, ApplyResponse::Restored { .. }));
        assert_eq!(
            fs::read(&service.paths.codex_config).unwrap(),
            original_config
        );
        assert_eq!(fs::read(&service.paths.codex_auth).unwrap(), original_auth);
        assert!(!service.paths.managed_model_catalog.exists());
    }

    #[test]
    fn context_draft_uses_explicit_window_and_compact_limit() {
        let context = context_from_draft(ContextDraft {
            use_defaults: false,
            window_k: "272".to_owned(),
            compact_percent: 80,
        })
        .unwrap();

        assert_eq!(context.model_context_window, Some(272_000));
        assert_eq!(context.model_auto_compact_token_limit, Some(217_600));
        assert_eq!(
            context.model_auto_compact_token_limit_scope,
            Some(AutoCompactScope::Total)
        );
    }

    #[test]
    fn a_new_profile_explicitly_restores_codex_context_defaults_on_apply() {
        let profile = profile_from_draft(ProfileId::new(), None, draft("Relay A")).unwrap();

        assert_eq!(profile.context, Some(ProfileContext::default()));
    }

    #[test]
    fn changing_a_saved_connection_invalidates_its_model_cache() {
        let (_home, service) = service();
        let created = service.create_profile(draft("Relay A")).unwrap();
        let profile_id: ProfileId = created.id.parse().unwrap();
        models::save_cache(
            &service.paths.model_cache_dir,
            &models::ModelCache {
                schema_version: 1,
                profile_id,
                fetched_at_ms: 1,
                models: vec!["glm-5.3".to_owned()],
            },
        )
        .unwrap();

        let mut update = draft("Relay A");
        update.base_url = "https://changed.example/v1".to_owned();
        service.update_profile(created.id, update).unwrap();

        assert!(
            models::load_cache(&service.paths.model_cache_dir, profile_id)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn cached_models_keep_a_manually_entered_current_model_selectable() {
        assert_eq!(
            models_with_current(&["glm-5.3".to_owned()], "custom-relay-model"),
            vec!["custom-relay-model".to_owned(), "glm-5.3".to_owned()]
        );
    }

    #[test]
    fn glm_profile_uses_the_shared_transaction_catalog() {
        let (_home, service) = service();
        let mut glm = draft("GLM Relay");
        glm.model = "glm-5.3".to_owned();
        let created = service.create_profile(glm).unwrap();
        let profile_id: ProfileId = created.id.parse().unwrap();
        let document = service.store().load().unwrap();
        let activation = document.get(profile_id).unwrap().activation().unwrap();

        TransactionManager::new(service.paths.clone())
            .apply(&activation, ConflictPolicy::Reject)
            .unwrap();

        let config = fs::read_to_string(&service.paths.codex_config).unwrap();
        assert!(
            config.contains("model_catalog_json = \"model-catalogs/codex-switch-models.json\"")
        );
        let catalog: serde_json::Value =
            serde_json::from_slice(&fs::read(&service.paths.managed_model_catalog).unwrap())
                .unwrap();
        assert_eq!(catalog["models"][0]["slug"], "glm-5.3");
    }
}
