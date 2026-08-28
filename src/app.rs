use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, UNIX_EPOCH};

use chrono::Local;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::codex_config::{
    self, CodexConfigError, ContextSettings, TOOL_PROVIDER_ID, profile_id_from_provider_id,
    provider_id_for_profile,
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
use crate::transaction::{ConflictPolicy, ManagedState, TransactionError, TransactionManager};
use crate::usage::{
    DailyUsage, LegacyUsageWindow, TokenUsage, UsagePeriod, UsageReport,
    UsageScope as UsageQueryScope,
};
use crate::usage_store::UsageStore;

slint::include_modules!();

type SharedController = Arc<Mutex<Controller>>;

const MIN_CONTEXT_WINDOW_TOKENS: u64 = 20;
const MAX_CONTEXT_WINDOW_TOKENS: u64 = i64::MAX as u64;

fn usage_scope(
    selected: Option<ProfileId>,
    legacy_windows: &[ProfileLegacyUsageWindow],
) -> UsageQueryScope {
    let Some(selected) = selected else {
        return UsageQueryScope::all();
    };
    let known_legacy_windows = legacy_windows
        .iter()
        .map(|window| LegacyUsageWindow::new(window.start_unix_ms, window.end_exclusive_unix_ms))
        .collect();
    let selected_legacy_windows = legacy_windows
        .iter()
        .filter(|window| window.profile_id == selected)
        .map(|window| LegacyUsageWindow::new(window.start_unix_ms, window.end_exclusive_unix_ms))
        .collect();
    UsageQueryScope::profile(
        provider_id_for_profile(selected),
        TOOL_PROVIDER_ID,
        selected_legacy_windows,
        known_legacy_windows,
    )
}

fn includes_inferred_legacy_usage(
    selected: Option<ProfileId>,
    legacy_windows: &[ProfileLegacyUsageWindow],
) -> bool {
    selected.is_some_and(|selected| {
        legacy_windows
            .iter()
            .any(|window| window.profile_id == selected)
    })
}

/// Recovers the current shared-provider segment when only model, review, or context settings
/// changed after the last managed apply. This is intentionally transient: a full state
/// fingerprint remains required for persisted history and write-conflict protection.
fn infer_unvalidated_live_legacy_window(
    active_profile_id: Option<ProfileId>,
    live_profile: Option<&Profile>,
    profiles: &ProfilesDocument,
    config_modified_at_unix_ms: Option<u64>,
    known_windows: &[ProfileLegacyUsageWindow],
) -> Option<ProfileLegacyUsageWindow> {
    let active_profile_id = active_profile_id?;
    let live_profile = live_profile?;
    let config_modified_at_unix_ms = config_modified_at_unix_ms?;

    // A validated live window is authoritative. Never layer a guessed window over it.
    if known_windows
        .iter()
        .any(|window| window.end_exclusive_unix_ms == u64::MAX)
    {
        return None;
    }

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

    let active_profile_id = transaction.load_state().ok().flatten()?.active_profile_id;
    let live_profile = import_live_profile(paths).ok()?;
    infer_unvalidated_live_legacy_window(
        active_profile_id,
        Some(&live_profile),
        profiles,
        config_modified_unix_ms(&paths.codex_config),
        known_windows,
    )
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

#[derive(Debug)]
enum DialogAction {
    None,
    Delete(ProfileId),
    ChangeSelection(usize),
    Export,
    RestoreConfirm,
    ApplyProcess {
        activation: Activation,
        desktop_executable: Option<PathBuf>,
        desktop_only: bool,
    },
    ApplyConflict {
        activation: Activation,
        desktop_executable: Option<PathBuf>,
    },
    ContextProcess {
        profile_id: ProfileId,
        settings: ContextSettings,
        desktop_executable: Option<PathBuf>,
        desktop_only: bool,
        completion: ContextCompletion,
    },
    ContextConflict {
        profile_id: ProfileId,
        settings: ContextSettings,
        desktop_executable: Option<PathBuf>,
        completion: ContextCompletion,
    },
    RestoreProcess {
        desktop_executable: Option<PathBuf>,
        desktop_only: bool,
    },
    Close,
}

#[derive(Clone, Copy, Debug, Default)]
enum ContextCompletion {
    #[default]
    None,
    Select(usize),
    Hide,
}

#[derive(Debug)]
struct ContextUpdateRequest {
    profile_id: ProfileId,
    settings: ContextSettings,
    completion: ContextCompletion,
    quit_desktop: bool,
    desktop_executable: Option<PathBuf>,
    policy: ConflictPolicy,
}

struct Controller {
    paths: AppPaths,
    store: ProfileStore,
    transaction: TransactionManager,
    profiles: ProfilesDocument,
    selected: Option<ProfileId>,
    active: Option<ProfileId>,
    dialog: DialogAction,
    startup_status: String,
    startup_tone: i32,
    usage_period: UsagePeriod,
    usage_report: Option<UsageReport>,
    usage_refresh_generation: u64,
    usage_refreshed_at: Option<String>,
    usage_includes_inferred_legacy_history: bool,
    _instance_lock: durable_fs::ExclusiveLock,
}

enum ApplyWorkerResult {
    Applied {
        state: ManagedState,
        relaunch_error: Option<String>,
        validation_skipped: bool,
    },
    Conflict {
        detail: String,
        desktop_executable: Option<PathBuf>,
    },
    Failed {
        message: String,
        recovery_required: bool,
    },
}

enum RestoreWorkerResult {
    Restored {
        relaunch_error: Option<String>,
    },
    Failed {
        message: String,
        recovery_required: bool,
    },
}

enum ContextWorkerResult {
    Updated {
        relaunch_error: Option<String>,
        validation_skipped: bool,
    },
    Conflict {
        detail: String,
        desktop_executable: Option<PathBuf>,
    },
    Failed {
        message: String,
        recovery_required: bool,
    },
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let paths = AppPaths::discover()?;
    durable_fs::ensure_private_dir(&paths.tool_dir)?;
    let instance_lock = durable_fs::acquire_lock(&paths.instance_lock)?;
    durable_fs::ensure_private_dir(&paths.model_cache_dir)?;
    durable_fs::ensure_private_dir(&paths.backups_dir)?;

    let transaction = TransactionManager::new(paths.clone());
    let recovery = transaction.recover_if_needed()?;
    let store = ProfileStore::new(paths.profiles.clone());
    let first_run = !paths.profiles.exists();
    let mut profiles = store.load()?;

    let mut startup_status = match recovery {
        crate::transaction::RecoveryOutcome::None => "就绪".to_owned(),
        crate::transaction::RecoveryOutcome::RolledBack { .. } => {
            "检测到未完成的切换，已恢复原配置".to_owned()
        }
    };
    let mut startup_tone = if matches!(
        recovery,
        crate::transaction::RecoveryOutcome::RolledBack { .. }
    ) {
        2
    } else {
        0
    };

    if first_run {
        match import_live_profile(&paths) {
            Ok(profile) => {
                let imported_id = profile.id;
                profiles.insert(profile)?;
                store.save(&profiles)?;
                match transaction.adopt_current(Some(imported_id)) {
                    Ok(_) => {
                        startup_status = "已导入当前 Codex 配置".to_owned();
                        startup_tone = 1;
                    }
                    Err(error) => {
                        startup_status = format!("已导入当前配置，但无法建立冲突检测基线：{error}");
                        startup_tone = 2;
                    }
                }
            }
            Err(_) => {
                store.save(&profiles)?;
                startup_status = "未能自动导入当前配置，可手动新建中转站".to_owned();
                startup_tone = 2;
            }
        }
    }

    let active = recognize_active_profile(&paths, &transaction, &profiles);
    let selected = active.or_else(|| profiles.profiles.first().map(|profile| profile.id));
    let controller = Arc::new(Mutex::new(Controller {
        paths,
        store,
        transaction,
        profiles,
        selected,
        active,
        dialog: DialogAction::None,
        startup_status,
        startup_tone,
        usage_period: UsagePeriod::Today,
        usage_report: None,
        usage_refresh_generation: 0,
        usage_refreshed_at: None,
        usage_includes_inferred_legacy_history: false,
        _instance_lock: instance_lock,
    }));

    let ui = AppWindow::new()?;
    {
        let controller = controller.lock().expect("controller mutex poisoned");
        controller.sync_all(&ui);
    }
    install_callbacks(&ui, controller.clone());
    {
        let mut controller_guard = controller.lock().expect("controller mutex poisoned");
        controller_guard.refresh_usage(&ui, controller.clone());
    }
    ui.run()?;
    Ok(())
}

fn install_callbacks(ui: &AppWindow, controller: SharedController) {
    macro_rules! bind {
        ($handler:ident, |$window:ident, $state:ident, $shared:ident| $body:block) => {{
            let weak = ui.as_weak();
            let shared = controller.clone();
            ui.$handler(move || {
                let Some($window) = weak.upgrade() else {
                    return;
                };
                let $shared = shared.clone();
                #[allow(unused_mut)]
                let mut $state = shared.lock().expect("controller mutex poisoned");
                $body
            });
        }};
        ($handler:ident, |$window:ident, $state:ident| $body:block) => {{
            let weak = ui.as_weak();
            let shared = controller.clone();
            ui.$handler(move || {
                let Some($window) = weak.upgrade() else {
                    return;
                };
                #[allow(unused_mut)]
                let mut $state = shared.lock().expect("controller mutex poisoned");
                $body
            });
        }};
    }

    {
        let weak = ui.as_weak();
        let controller = controller.clone();
        ui.on_profile_selected(move |index| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let target = usize::try_from(index).ok();
            let Some(target) = target else {
                return;
            };
            let shared = controller.clone();
            let mut state = controller.lock().expect("controller mutex poisoned");
            if ui.get_draft_dirty() || ui.get_context_dirty() {
                state.dialog = DialogAction::ChangeSelection(target);
                open_dialog(
                    &ui,
                    "切换中转站",
                    "当前中转站有未保存的配置修改。",
                    "保存并切换",
                    "放弃并切换",
                    "取消",
                    false,
                );
            } else {
                state.select_index(&ui, target, shared);
            }
        });
    }

    bind!(on_new_profile, |ui, state, shared| {
        state.new_profile(&ui, shared.clone());
    });
    bind!(on_duplicate_profile, |ui, state, shared| {
        state.duplicate_profile(&ui, shared.clone());
    });
    bind!(on_delete_profile, |ui, state| {
        state.confirm_delete(&ui);
    });
    bind!(on_import_profiles, |ui, state, shared| {
        state.import_profiles(&ui, shared.clone());
    });
    bind!(on_import_current, |ui, state, shared| {
        state.import_current(&ui, shared.clone());
    });
    bind!(on_export_profiles, |ui, state| {
        state.dialog = DialogAction::Export;
        open_dialog(
            &ui,
            "导出中转站",
            "默认不导出 API Key。只有在你明确需要完整迁移时才包含密钥。",
            "不含密钥",
            "包含密钥",
            "取消",
            false,
        );
    });
    bind!(on_restore_last, |ui, state| {
        state.confirm_restore(&ui);
    });
    {
        let weak = ui.as_weak();
        let shared = controller.clone();
        ui.on_refresh_models(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut state = shared.lock().expect("controller mutex poisoned");
            state.refresh_models(&ui, shared.clone());
        });
    }
    bind!(on_save_profile, |ui, state| {
        if state.save_draft(&ui) {
            ui.set_editor_open(false);
        }
    });

    {
        let weak = ui.as_weak();
        let controller_for_callback = controller.clone();
        ui.on_apply_profile(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let shared = controller_for_callback.clone();
            let mut controller = shared.lock().expect("controller mutex poisoned");
            controller.begin_apply(&ui, shared.clone());
        });
    }

    bind!(on_discard_draft, |ui, state| {
        state.reload_profile_draft(&ui);
        set_status(&ui, "已放弃未保存的中转站修改", 0);
    });

    {
        let weak = ui.as_weak();
        let shared = controller.clone();
        ui.on_save_context(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut state = shared.lock().expect("controller mutex poisoned");
            state.save_context(&ui, shared.clone(), ContextCompletion::None);
        });
    }
    bind!(on_restore_context_defaults, |ui, state| {
        state.restore_context_defaults(&ui);
    });
    bind!(on_context_changed, |ui, state| {
        state.materialize_context_window(&ui);
        ui.set_context_defaults_selected(false);
    });
    {
        let weak = ui.as_weak();
        let shared = controller.clone();
        ui.on_usage_period_selected(move |period| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let Some(period) = usage_period_from_index(period) else {
                return;
            };
            let mut state = shared.lock().expect("controller mutex poisoned");
            state.usage_period = period;
            state.refresh_usage(&ui, shared.clone());
        });
    }
    {
        let weak = ui.as_weak();
        let shared = controller.clone();
        ui.on_refresh_usage(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut state = shared.lock().expect("controller mutex poisoned");
            state.refresh_usage(&ui, shared.clone());
        });
    }
    {
        let weak = ui.as_weak();
        let shared = controller.clone();
        ui.on_usage_trend_hovered(move |ratio, point_count| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let point_count = usize::try_from(point_count).unwrap_or_default();
            let hover_index = usage_trend_hover_index(point_count, ratio).unwrap_or(-1);
            ui.set_usage_trend_hover_index(hover_index);
            let controller = shared.lock().expect("controller mutex poisoned");
            if let Some(report) = controller.usage_report.as_ref() {
                sync_usage_daily_metrics(&ui, report, hover_index);
            }
        });
    }
    bind!(on_export_usage, |ui, state| {
        state.export_usage(&ui);
    });

    {
        let weak = ui.as_weak();
        let controller_for_callback = controller.clone();
        ui.on_dialog_primary(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            handle_dialog_primary(&ui, controller_for_callback.clone());
        });
    }
    {
        let weak = ui.as_weak();
        let controller_for_callback = controller.clone();
        ui.on_dialog_secondary(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            handle_dialog_secondary(&ui, controller_for_callback.clone());
        });
    }
    {
        let weak = ui.as_weak();
        let controller_for_callback = controller.clone();
        ui.on_dialog_tertiary(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            handle_dialog_tertiary(&ui, controller_for_callback.clone());
        });
    }
    {
        let weak = ui.as_weak();
        let controller = controller.clone();
        ui.window().on_close_requested(move || {
            let Some(ui) = weak.upgrade() else {
                return slint::CloseRequestResponse::HideWindow;
            };
            if ui.get_busy() {
                set_status(&ui, "操作正在进行，请等待完成后再关闭", 2);
                return slint::CloseRequestResponse::KeepWindowShown;
            }
            if ui.get_draft_dirty() || ui.get_context_dirty() {
                let mut controller = controller.lock().expect("controller mutex poisoned");
                controller.dialog = DialogAction::Close;
                open_dialog(
                    &ui,
                    "关闭 Codex Switch",
                    "当前中转站有未保存的配置修改。",
                    "保存并关闭",
                    "放弃并关闭",
                    "取消",
                    false,
                );
                slint::CloseRequestResponse::KeepWindowShown
            } else {
                slint::CloseRequestResponse::HideWindow
            }
        });
    }
}

impl Controller {
    fn sync_all(&self, ui: &AppWindow) {
        self.sync_profiles(ui);
        self.reload_selected(ui);
        if let Some(report) = &self.usage_report {
            self.sync_usage(ui, report);
        }
        ui.set_can_restore(self.transaction.has_backup().unwrap_or(false));
        ui.set_status_text(self.startup_status.clone().into());
        ui.set_status_tone(self.startup_tone);
    }

    fn sync_context(&self, ui: &AppWindow) {
        let selected_profile = self.selected.and_then(|id| self.profiles.get(id));
        let live_config = read_config_text(&self.paths).unwrap_or_default();
        let live_context = codex_config::inspect_context_settings(&live_config)
            .map(ProfileContext::from)
            .unwrap_or_default();
        let context = match selected_profile.and_then(|profile| profile.context) {
            Some(context) => context,
            None if self.selected == self.active => live_context,
            None => ProfileContext::default(),
        };
        let uses_defaults = context == ProfileContext::default();
        let explicit_window = context.model_context_window;
        let latest = (self.selected == self.active)
            .then(|| {
                self.usage_report
                    .as_ref()
                    .and_then(|report| report.latest_context.as_ref())
            })
            .flatten();

        let compact_percent = context
            .model_auto_compact_token_limit
            .zip(
                context
                    .model_context_window
                    .or_else(|| latest.map(|usage| usage.model_context_window)),
            )
            .and_then(|(limit, window)| rounded_percent(limit, window))
            .and_then(|percent| i32::try_from(percent).ok())
            .map(|percent| percent.clamp(50, 95))
            .unwrap_or(80);

        ui.set_context_window_k(
            explicit_window
                .map(format_context_window_k)
                .unwrap_or_default()
                .into(),
        );
        ui.set_compact_percent(compact_percent);
        ui.set_context_defaults_selected(uses_defaults);
        ui.set_context_dirty(false);
        ui.set_context_summary(context_summary(context).into());
        ui.set_context_sync_error(false);
        if self.selected.is_none() {
            ui.set_context_status("没有选中的中转站".into());
            ui.set_context_status_tone(0);
        } else if self.selected != self.active {
            ui.set_context_status("上下文配置 · 已保存，切换后生效".into());
            ui.set_context_status_tone(0);
        } else if selected_profile.is_some_and(|profile| profile.context.is_none()) {
            ui.set_context_status("上下文配置 · 沿用当前 Codex 配置".into());
            ui.set_context_status_tone(0);
        } else if context == live_context {
            ui.set_context_status("上下文配置 · 已同步到 Codex".into());
            ui.set_context_status_tone(1);
        } else {
            ui.set_context_status("上下文配置 · 尚未同步到 Codex".into());
            ui.set_context_status_tone(2);
            ui.set_context_sync_error(true);
        }

        let cwd = latest.and_then(|usage| usage.cwd.as_deref());
        let instructions =
            context::discover_instruction_sources(&self.paths.codex_dir, cwd, &live_config);
        let instruction_rows: Vec<InstructionRow> = instructions
            .sources
            .iter()
            .map(|source| InstructionRow {
                name: source.name.clone().into(),
                detail: format!(
                    "约 {} tokens · {}",
                    format_compact_tokens(source.estimated_tokens),
                    instruction_source_label(source.scope, &source.path, &self.paths)
                )
                .into(),
                enabled: true,
            })
            .collect();
        ui.set_instructions(ModelRc::new(VecModel::from(instruction_rows)));

        let effective_window = latest
            .map(|usage| usage.model_context_window)
            .filter(|window| *window > 0)
            .or(explicit_window)
            .unwrap_or(0);
        let active_context = latest.map(|usage| usage.total_tokens).unwrap_or(0);
        let estimated_instructions = instructions.estimated_tokens.min(active_context);
        let other_input = active_context.saturating_sub(estimated_instructions);
        if effective_window == 0 {
            ui.set_context_history_ratio(0.0);
            ui.set_context_instruction_ratio(0.0);
            ui.set_context_remaining_ratio(1.0);
        } else {
            let capacity = effective_window as f64;
            let other_ratio = (other_input as f64 / capacity).clamp(0.0, 1.0);
            let instruction_ratio =
                (estimated_instructions as f64 / capacity).clamp(0.0, 1.0 - other_ratio);
            ui.set_context_history_ratio(other_ratio as f32);
            ui.set_context_instruction_ratio(instruction_ratio as f32);
            ui.set_context_remaining_ratio((1.0 - other_ratio - instruction_ratio) as f32);
        }
    }

    fn materialize_context_window(&self, ui: &AppWindow) {
        if !ui.get_context_window_k().trim().is_empty() {
            return;
        }
        let recent_window = (self.selected == self.active)
            .then(|| {
                self.usage_report
                    .as_ref()
                    .and_then(|report| report.latest_context.as_ref())
                    .map(|usage| usage.model_context_window)
                    .filter(|window| *window > 0)
            })
            .flatten();
        let window = recent_window.unwrap_or(272_000);
        ui.set_context_window_k(format_context_window_k(window).into());
    }

    fn restore_context_defaults(&self, ui: &AppWindow) {
        ui.set_context_window_k("".into());
        ui.set_compact_percent(80);
        ui.set_context_defaults_selected(true);
        ui.set_context_dirty(true);
        ui.set_context_summary("自动窗口 · 输出不限 · 自动压缩".into());
        ui.set_context_sync_error(false);
        ui.set_context_status("上下文配置 · 有未保存修改".into());
        ui.set_context_status_tone(2);
    }

    fn save_context(
        &mut self,
        ui: &AppWindow,
        shared: SharedController,
        completion: ContextCompletion,
    ) -> bool {
        let Some(id) = self.selected else {
            return false;
        };
        let context = if ui.get_context_defaults_selected() {
            ProfileContext::default()
        } else {
            let window = match parse_context_window_k(ui.get_context_window_k().as_str()) {
                Ok(window) => window,
                Err(()) => {
                    set_context_status(ui, "上下文窗口请输入不小于 0.02K 的有效数值", 3);
                    return false;
                }
            };
            let percent = ui.get_compact_percent().clamp(50, 95) as u64;
            ProfileContext {
                model_context_window: Some(window),
                model_auto_compact_token_limit: Some(compact_limit_for_percent(window, percent)),
                model_auto_compact_token_limit_scope: Some(AutoCompactScope::Total),
            }
        };
        if let Err(error) = context.validate() {
            set_context_status(ui, format!("上下文配置无效：{error}"), 3);
            return false;
        }

        let mut updated = self.profiles.clone();
        let Some(profile) = updated.get_mut(id) else {
            return false;
        };
        profile.context = Some(context);
        if let Err(error) = updated.validate().and_then(|()| self.store.save(&updated)) {
            set_context_status(ui, format!("无法保存上下文配置：{error}"), 3);
            return false;
        }
        self.profiles = updated;
        ui.set_context_dirty(false);
        self.sync_context(ui);

        if self.active == Some(id) {
            ui.set_context_sync_error(false);
            ui.set_context_status("上下文配置 · 已保存，等待同步".into());
            ui.set_context_status_tone(0);
            self.continue_context_after_process_check(ui, shared, id, context.into(), completion);
        } else {
            set_status(ui, "上下文配置已保存，下次切换到此中转站时生效", 1);
            self.complete_context_action(ui, completion, shared);
        }
        true
    }

    fn continue_context_after_process_check(
        &mut self,
        ui: &AppWindow,
        shared: SharedController,
        profile_id: ProfileId,
        settings: ContextSettings,
        completion: ContextCompletion,
    ) {
        let report = process::detect_codex_processes();
        if report.is_clear() {
            spawn_context_update(
                ui,
                shared,
                self.paths.clone(),
                ContextUpdateRequest {
                    profile_id,
                    settings,
                    completion,
                    quit_desktop: false,
                    desktop_executable: None,
                    policy: ConflictPolicy::Reject,
                },
            );
            return;
        }

        let desktop_only = report.has_desktop() && !report.has_command_line();
        let desktop_executable = report.desktop_executable();
        self.dialog = DialogAction::ContextProcess {
            profile_id,
            settings,
            desktop_executable,
            desktop_only,
            completion,
        };
        open_dialog(
            ui,
            "Codex 正在运行",
            if desktop_only {
                "推荐先退出 Codex Desktop。上下文同步完成后工具会重新打开它。"
            } else {
                "检测到 Codex 命令行任务或多个 Codex 进程。请先结束相关任务，避免会话继续使用旧上下文配置。"
            },
            if desktop_only {
                "退出并同步"
            } else {
                "重新检测"
            },
            "仍然同步",
            "取消",
            false,
        );
    }

    fn complete_context_action(
        &mut self,
        ui: &AppWindow,
        completion: ContextCompletion,
        shared: SharedController,
    ) {
        match completion {
            ContextCompletion::None => {}
            ContextCompletion::Select(index) => self.select_index(ui, index, shared),
            ContextCompletion::Hide => {
                let _ = ui.hide();
            }
        }
    }

    fn refresh_usage(&mut self, ui: &AppWindow, shared: SharedController) {
        self.usage_refresh_generation = self.usage_refresh_generation.saturating_add(1);
        let generation = self.usage_refresh_generation;
        let period = self.usage_period;
        let selected = self.selected;
        let sessions_dir = self.paths.codex_sessions.clone();
        let archived_sessions_dir = self.paths.codex_archived_sessions.clone();
        let usage_database = self.paths.usage_database.clone();
        let paths = self.paths.clone();
        let profiles = self.profiles.clone();
        ui.set_usage_period(usage_period_index(period));
        ui.set_usage_loading(true);
        ui.set_usage_error(false);
        ui.set_usage_status("正在读取本地用量数据".into());
        let weak = ui.as_weak();
        thread::spawn(move || {
            let store = UsageStore::new(usage_database);
            let transaction = TransactionManager::new(paths.clone());
            let timeline = transaction
                .legacy_usage_history()
                .map(|history| reconstruct_legacy_usage(&history))
                .unwrap_or_default();
            let durable_windows = store
                .remember_legacy_windows(&timeline.durable_windows)
                .unwrap_or_else(|_| timeline.durable_windows.clone());
            let mut legacy_windows = durable_windows;
            legacy_windows.extend(timeline.live_windows.iter().copied());
            legacy_windows = normalize_profile_windows(legacy_windows);
            if timeline.live_windows.is_empty()
                && let Some(window) =
                    inferred_live_legacy_window(&paths, &transaction, &profiles, &legacy_windows)
            {
                legacy_windows.push(window);
            }
            let legacy_windows = normalize_profile_windows(legacy_windows);
            let usage_scope = usage_scope(selected, &legacy_windows);
            let includes_inferred_legacy_history =
                includes_inferred_legacy_usage(selected, &legacy_windows);
            let result = store
                .refresh_scoped(&sessions_dir, &archived_sessions_dir, period, &usage_scope)
                .map(|report| (report, includes_inferred_legacy_history));
            let refreshed_at = Local::now().format("%H:%M").to_string();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                let mut controller = shared.lock().expect("controller mutex poisoned");
                if controller.usage_refresh_generation != generation {
                    return;
                }
                ui.set_usage_loading(false);
                match result {
                    Ok((report, includes_inferred_legacy_history)) => {
                        ui.set_usage_error(false);
                        controller.usage_refreshed_at = Some(refreshed_at);
                        controller.usage_includes_inferred_legacy_history =
                            includes_inferred_legacy_history;
                        controller.usage_report = Some(report);
                        if let Some(report) = controller.usage_report.clone() {
                            controller.sync_usage(&ui, &report);
                        }
                        if !ui.get_context_dirty() && !ui.get_context_sync_error() && !ui.get_busy()
                        {
                            controller.sync_context(&ui);
                        }
                    }
                    Err(error) => {
                        ui.set_usage_error(true);
                        if controller.usage_report.is_none() {
                            ui.set_usage_has_data(false);
                        }
                        ui.set_usage_status(format!("本地用量读取失败：{error}").into());
                        set_status(&ui, format!("用量读取失败：{error}"), 3);
                    }
                }
            });
        });
    }

    fn sync_usage(&self, ui: &AppWindow, report: &UsageReport) {
        let current = report.current;
        sync_usage_daily_metrics(ui, report, -1);
        ui.set_usage_period_total_label(usage_period_total_label(report.period).into());
        ui.set_usage_period_models_label(usage_period_models_label(report.period).into());
        ui.set_usage_trend_label(usage_trend_label(report.period).into());
        ui.set_usage_trend_unit_label(usage_trend_unit_label(report.period).into());
        ui.set_usage_total_input(format_compact_tokens(current.input_tokens).into());
        ui.set_usage_total_cached(format_compact_tokens(current.cached_input_tokens).into());
        ui.set_usage_total_output(format_compact_tokens(current.output_tokens).into());
        ui.set_usage_total_calls(format!("{} 次", format_integer(current.calls)).into());

        ui.set_usage_input_path(
            usage_line_path(report.trend.iter().map(|point| point.usage.input_tokens)).into(),
        );
        ui.set_usage_output_path(
            usage_line_path(report.trend.iter().map(|point| point.usage.output_tokens)).into(),
        );
        let trend_rows: Vec<UsageTrendRow> = report
            .trend
            .iter()
            .map(|point| UsageTrendRow {
                date: point.label.clone().into(),
                input: format_integer(point.usage.input_tokens).into(),
                cached: format_integer(point.usage.cached_input_tokens).into(),
                output: format_integer(point.usage.output_tokens).into(),
                calls: format_integer(point.usage.calls).into(),
            })
            .collect();
        ui.set_usage_trend_points(ModelRc::new(VecModel::from(trend_rows)));
        ui.set_usage_trend_hover_index(-1);

        let model_rows: Vec<UsageModelRow> = report
            .model_distribution
            .iter()
            .map(|model| UsageModelRow {
                model: model.model.clone().into(),
                input: format_compact_tokens(model.usage.input_tokens).into(),
                cached: format_compact_tokens(model.usage.cached_input_tokens).into(),
                output: format_compact_tokens(model.usage.output_tokens).into(),
                calls: format_integer(model.usage.calls).into(),
            })
            .collect();
        ui.set_usage_models(ModelRc::new(VecModel::from(model_rows)));
        ui.set_usage_has_data(current.calls > 0);
        ui.set_usage_error(false);

        let skipped = report.skipped_lines.saturating_add(report.skipped_files);
        let selected_name = self
            .selected
            .and_then(|id| self.profiles.get(id))
            .map(|profile| profile.name.as_str())
            .unwrap_or("未选择中转站");
        let legacy_note = self
            .usage_includes_inferred_legacy_history
            .then_some(" · 含已归属的迁移前记录");
        let unattributed_note = (report.unattributed_legacy.calls > 0).then(|| {
            format!(
                " · 另有 {} 次迁移前共享调用未归属",
                format_integer(report.unattributed_legacy.calls)
            )
        });
        let status = match (&self.usage_refreshed_at, skipped) {
            (Some(time), 0) => format!(
                "数据更新于 {time} · {selected_name}{}{} · 每 15 分钟自动更新",
                legacy_note.unwrap_or(""),
                unattributed_note.as_deref().unwrap_or("")
            ),
            (Some(time), skipped) => format!(
                "数据更新于 {time} · {selected_name}{}{} · 已略过 {skipped} 条无效记录",
                legacy_note.unwrap_or(""),
                unattributed_note.as_deref().unwrap_or("")
            ),
            (None, _) => "本地用量数据已读取".to_owned(),
        };
        ui.set_usage_status(status.into());

        let today = report.daily.last().map(|day| day.usage).unwrap_or_default();
        ui.set_today_usage_summary(if today.calls == 0 {
            "今日暂无本地记录".into()
        } else {
            format!(
                "{} tokens · {} 次调用",
                format_compact_tokens(today.total_tokens()),
                format_integer(today.calls)
            )
            .into()
        });
    }

    fn export_usage(&self, ui: &AppWindow) {
        let Some(report) = &self.usage_report else {
            set_status(ui, "没有可导出的用量数据", 2);
            return;
        };
        let Some(path) = rfd::FileDialog::new()
            .add_filter("CSV", &["csv"])
            .set_file_name(format!(
                "codex-switch-usage-{}-{}.csv",
                match report.period {
                    UsagePeriod::Today => "today",
                    UsagePeriod::Last7Days => "last-7-days",
                    UsagePeriod::Last30Days => "last-30-days",
                },
                Local::now().format("%Y-%m-%d")
            ))
            .save_file()
        else {
            return;
        };
        match durable_fs::atomic_write(&path, report.model_distribution_csv().as_bytes()) {
            Ok(()) => set_status(ui, "用量明细已导出", 1),
            Err(error) => set_status(ui, format!("用量导出失败：{error}"), 3),
        }
    }

    fn sync_profiles(&self, ui: &AppWindow) {
        let rows: Vec<ProfileRow> = self
            .profiles
            .profiles
            .iter()
            .map(|profile| ProfileRow {
                id: profile.id.to_string().into(),
                name: profile.name.clone().into(),
                base_url: profile.base_url.clone().into(),
                model: profile.model.clone().into(),
                is_active: self.active == Some(profile.id),
            })
            .collect();
        ui.set_profiles(ModelRc::new(VecModel::from(rows)));

        let selected_index = self
            .selected
            .and_then(|id| {
                self.profiles
                    .profiles
                    .iter()
                    .position(|profile| profile.id == id)
            })
            .and_then(|index| i32::try_from(index).ok())
            .unwrap_or(-1);
        ui.set_selected_index(selected_index);

        let active_name = self
            .active
            .and_then(|id| self.profiles.get(id))
            .map(|profile| profile.name.clone())
            .unwrap_or_default();
        ui.set_active_profile_name(active_name.into());
    }

    fn select_index(&mut self, ui: &AppWindow, index: usize, shared: SharedController) {
        let selected = self.profiles.profiles.get(index).map(|profile| profile.id);
        self.select_profile(ui, selected, shared);
    }

    fn select_profile(
        &mut self,
        ui: &AppWindow,
        selected: Option<ProfileId>,
        shared: SharedController,
    ) {
        self.selected = selected;
        self.sync_profiles(ui);
        self.invalidate_usage(ui);
        self.reload_selected(ui);
        self.refresh_usage(ui, shared);
    }

    fn invalidate_usage(&mut self, ui: &AppWindow) {
        self.usage_report = None;
        self.usage_refreshed_at = None;
        self.usage_includes_inferred_legacy_history = false;
        ui.set_usage_has_data(false);
        ui.set_usage_error(false);
        ui.set_usage_trend_points(empty_usage_trend_model());
        ui.set_usage_models(empty_usage_model());
    }

    fn reload_selected(&self, ui: &AppWindow) {
        self.reload_profile_draft(ui);
        self.sync_context(ui);
    }

    fn reload_profile_draft(&self, ui: &AppWindow) {
        let Some(profile) = self.selected.and_then(|id| self.profiles.get(id)) else {
            ui.set_editor_open(false);
            ui.set_selected_index(-1);
            ui.set_draft_name(SharedString::default());
            ui.set_draft_base_url(SharedString::default());
            ui.set_draft_api_key(SharedString::default());
            ui.set_draft_model(SharedString::default());
            ui.set_draft_review_model(SharedString::default());
            ui.set_model_names(empty_string_model());
            ui.set_model_cache_label(SharedString::default());
            ui.set_draft_dirty(false);
            return;
        };

        ui.set_draft_name(profile.name.clone().into());
        ui.set_draft_base_url(profile.base_url.clone().into());
        ui.set_draft_api_key(
            profile
                .api_key
                .as_ref()
                .map(|key| key.expose_secret())
                .unwrap_or_default()
                .into(),
        );
        ui.set_draft_model(profile.model.clone().into());
        ui.set_draft_review_model(profile.review_model.clone().unwrap_or_default().into());
        ui.set_advanced_open(profile.review_model.is_some());
        self.load_model_cache(ui, profile);
        ui.set_draft_dirty(false);
    }

    fn load_model_cache(&self, ui: &AppWindow, profile: &Profile) {
        match models::load_cache(&self.paths.model_cache_dir, profile.id) {
            Ok(Some(cache)) => {
                set_model_list(ui, &cache.models, &profile.model);
                ui.set_model_cache_label(
                    format!("已缓存 {} 个模型，点击刷新可重新获取", cache.models.len()).into(),
                );
            }
            Ok(None) => {
                ui.set_model_names(empty_string_model());
                ui.set_model_index(-1);
                ui.set_model_cache_label("尚未获取模型列表".into());
            }
            Err(_) => {
                ui.set_model_names(empty_string_model());
                ui.set_model_index(-1);
                ui.set_model_cache_label("模型缓存不可用，可点击刷新重建".into());
            }
        }
    }

    fn new_profile(&mut self, ui: &AppWindow, shared: SharedController) {
        if !self.save_before_navigation(ui) {
            return;
        }
        let name = unique_name(&self.profiles, "新中转站");
        let profile =
            match Profile::without_api_key(name, "https://relay.example/v1", "gpt-5", None) {
                Ok(profile) => profile,
                Err(error) => {
                    set_status(ui, format!("无法新建中转站：{error}"), 3);
                    return;
                }
            };
        let id = profile.id;
        let mut updated = self.profiles.clone();
        if let Err(error) = updated
            .insert(profile)
            .and_then(|()| self.store.save(&updated))
        {
            set_status(ui, format!("无法保存中转站：{error}"), 3);
            return;
        }
        self.profiles = updated;
        self.select_profile(ui, Some(id), shared);
        set_status(ui, "已新建中转站，请填写连接信息", 0);
    }

    fn duplicate_profile(&mut self, ui: &AppWindow, shared: SharedController) {
        if !self.save_before_navigation(ui) {
            return;
        }
        let Some(mut profile) = self.selected.and_then(|id| self.profiles.get(id)).cloned() else {
            return;
        };
        profile.id = ProfileId::new();
        profile.name = unique_name(&self.profiles, &format!("{} 副本", profile.name));
        let id = profile.id;
        let mut updated = self.profiles.clone();
        if let Err(error) = updated
            .insert(profile)
            .and_then(|()| self.store.save(&updated))
        {
            set_status(ui, format!("无法复制中转站：{error}"), 3);
            return;
        }
        self.profiles = updated;
        self.select_profile(ui, Some(id), shared);
        set_status(ui, "中转站已复制", 1);
    }

    fn confirm_delete(&mut self, ui: &AppWindow) {
        let Some(id) = self.selected else {
            return;
        };
        self.dialog = DialogAction::Delete(id);
        open_dialog(
            ui,
            "删除中转站",
            "只会删除工具保存的中转站，不会修改当前 Codex 配置。",
            "删除",
            "",
            "取消",
            true,
        );
    }

    fn delete_profile(&mut self, ui: &AppWindow, id: ProfileId, shared: SharedController) {
        let Some(index) = self
            .profiles
            .profiles
            .iter()
            .position(|profile| profile.id == id)
        else {
            return;
        };
        let mut updated = self.profiles.clone();
        updated.remove(id);
        if let Err(error) = self.store.save(&updated) {
            set_status(ui, format!("无法删除中转站：{error}"), 3);
            return;
        }
        self.profiles = updated;
        let _ = models::remove_cache(&self.paths.model_cache_dir, id);
        let selected = self
            .profiles
            .profiles
            .get(index.min(self.profiles.profiles.len().saturating_sub(1)))
            .map(|profile| profile.id);
        self.select_profile(ui, selected, shared);
        ui.set_editor_open(false);
        set_status(ui, "中转站已删除，当前 Codex 配置未改动", 1);
    }

    fn save_draft(&mut self, ui: &AppWindow) -> bool {
        let Some(id) = self.selected else {
            return false;
        };
        let context = self.profiles.get(id).and_then(|profile| profile.context);
        let profile = match profile_from_ui(ui, id, context) {
            Ok(profile) => profile,
            Err(error) => {
                set_status(ui, format!("中转站信息不完整：{error}"), 3);
                return false;
            }
        };
        let invalidate_model_cache = self
            .profiles
            .get(id)
            .is_some_and(|saved| !same_model_catalog(saved, &profile));
        let mut updated = self.profiles.clone();
        let Some(existing) = updated.get_mut(id) else {
            return false;
        };
        *existing = profile;
        if let Err(error) = updated.validate().and_then(|()| self.store.save(&updated)) {
            set_status(ui, format!("无法保存中转站：{error}"), 3);
            return false;
        }
        self.profiles = updated;
        self.active = recognize_active_profile(&self.paths, &self.transaction, &self.profiles);
        self.sync_profiles(ui);
        if invalidate_model_cache {
            let _ = models::remove_cache(&self.paths.model_cache_dir, id);
            ui.set_model_names(empty_string_model());
            ui.set_model_index(-1);
            ui.set_model_cache_label("连接信息已更改，请重新获取模型列表".into());
        }
        ui.set_draft_dirty(false);
        set_status(ui, "中转站已保存", 1);
        true
    }

    fn save_before_navigation(&mut self, ui: &AppWindow) -> bool {
        if ui.get_context_dirty() {
            ui.set_current_page(1);
            set_status(ui, "请先保存或恢复上下文配置", 2);
            return false;
        }
        !ui.get_draft_dirty() || self.save_draft(ui)
    }

    fn begin_apply(&mut self, ui: &AppWindow, shared: SharedController) {
        if ui.get_context_dirty() {
            ui.set_current_page(1);
            set_status(ui, "请先保存上下文配置，再切换中转站", 2);
            return;
        }
        if !self.save_draft(ui) {
            return;
        }
        let Some(profile) = self.selected.and_then(|id| self.profiles.get(id)) else {
            return;
        };
        let activation = match profile.activation() {
            Ok(activation) => activation,
            Err(error) => {
                set_status(ui, format!("无法切换：{error}"), 3);
                return;
            }
        };
        self.continue_apply_after_process_check(ui, shared, activation);
    }

    fn continue_apply_after_process_check(
        &mut self,
        ui: &AppWindow,
        shared: SharedController,
        activation: Activation,
    ) {
        let report = process::detect_codex_processes();
        if report.is_clear() {
            spawn_apply(
                ui,
                shared,
                self.paths.clone(),
                activation,
                ConflictPolicy::Reject,
                false,
                None,
            );
            return;
        }

        let desktop_only = report.has_desktop() && !report.has_command_line();
        let desktop_executable = report.desktop_executable();
        let message = if desktop_only {
            "Codex Desktop 正在运行。推荐先退出，切换完成后工具会重新打开它。"
        } else {
            "检测到 Codex 命令行任务或多个 Codex 进程。请先结束相关任务，避免运行中的会话继续使用旧配置。"
        };
        let primary = if desktop_only {
            "退出并切换"
        } else {
            "重新检测"
        };
        self.dialog = DialogAction::ApplyProcess {
            activation,
            desktop_executable,
            desktop_only,
        };
        open_dialog(
            ui,
            "Codex 正在运行",
            message,
            primary,
            "仍然切换",
            "取消",
            false,
        );
    }

    fn refresh_models(&mut self, ui: &AppWindow, shared: SharedController) {
        let Some(id) = self.selected else {
            return;
        };
        let context = self.profiles.get(id).and_then(|profile| profile.context);
        let profile = match profile_from_ui(ui, id, context) {
            Ok(profile) => profile,
            Err(error) => {
                set_status(ui, format!("无法获取模型：{error}"), 3);
                return;
            }
        };
        if profile.api_key.is_none() {
            set_status(ui, "请先填写 API Key", 3);
            return;
        }

        ui.set_busy(true);
        set_status(ui, "正在获取模型列表", 0);
        let weak = ui.as_weak();
        let cache_dir = self.paths.model_cache_dir.clone();
        thread::spawn(move || {
            let result = models::fetch_models(&profile).map_err(|error| error.to_string());
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.set_busy(false);
                let controller = shared.lock().expect("controller mutex poisoned");
                match result {
                    Ok(cache) if controller.selected == Some(cache.profile_id) => {
                        set_model_list(&ui, &cache.models, &ui.get_draft_model());
                        if profile_owns_model_cache(
                            controller.profiles.get(cache.profile_id),
                            &profile,
                        ) {
                            match models::save_cache(&cache_dir, &cache) {
                                Ok(()) => {
                                    ui.set_model_cache_label(
                                        format!("刚刚获取了 {} 个模型", cache.models.len()).into(),
                                    );
                                    set_status(&ui, "模型列表已更新", 1);
                                }
                                Err(error) => set_status(
                                    &ui,
                                    format!("模型已获取，但缓存保存失败：{error}"),
                                    2,
                                ),
                            }
                        } else {
                            ui.set_model_cache_label(
                                format!("已获取 {} 个模型；保存连接后可缓存", cache.models.len())
                                    .into(),
                            );
                            set_status(&ui, "模型列表已更新，未保存草稿不会写入缓存", 2);
                        }
                    }
                    Ok(cache) => {
                        if profile_owns_model_cache(
                            controller.profiles.get(cache.profile_id),
                            &profile,
                        ) {
                            match models::save_cache(&cache_dir, &cache) {
                                Ok(()) => set_status(&ui, "模型列表已缓存到对应中转站", 1),
                                Err(error) => set_status(
                                    &ui,
                                    format!("模型已获取，但缓存保存失败：{error}"),
                                    2,
                                ),
                            }
                        }
                    }
                    Err(error) => {
                        set_status(&ui, format!("模型获取失败：{error}"), 3);
                    }
                }
                controller.sync_profiles(&ui);
            });
        });
    }

    fn import_current(&mut self, ui: &AppWindow, shared: SharedController) {
        if !self.save_before_navigation(ui) {
            return;
        }
        let _ = self.import_current_into(ui, shared);
    }

    fn import_current_into(&mut self, ui: &AppWindow, shared: SharedController) -> bool {
        match import_live_profile(&self.paths) {
            Ok(mut imported) => {
                let mut updated = self.profiles.clone();
                let imported_id = imported.id;
                updated.remove(imported_id);
                imported.name = unique_name(&updated, &imported.name);
                if let Err(error) = updated.insert(imported) {
                    set_status(ui, format!("无法导入当前配置：{error}"), 3);
                    return false;
                }
                if let Err(error) = self.store.save(&updated) {
                    set_status(ui, format!("无法保存导入的中转站：{error}"), 3);
                    return false;
                }
                self.active = Some(imported_id);
                self.profiles = updated;
                self.select_profile(ui, Some(imported_id), shared);
                match self.transaction.adopt_current(Some(imported_id)) {
                    Ok(_) => set_status(ui, "已导入当前 Codex 配置", 1),
                    Err(error) => set_status(
                        ui,
                        format!("中转站已导入，但无法建立冲突检测基线：{error}"),
                        2,
                    ),
                }
                true
            }
            Err(error) => {
                set_status(ui, format!("当前配置无法导入：{error}"), 3);
                false
            }
        }
    }

    fn import_profiles(&mut self, ui: &AppWindow, shared: SharedController) {
        if !self.save_before_navigation(ui) {
            return;
        }
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Codex Switch profiles", &["toml"])
            .pick_file()
        else {
            return;
        };
        let result = fs::read(&path)
            .map_err(|error| error.to_string())
            .and_then(|bytes| ProfileStore::deserialize(&bytes).map_err(|error| error.to_string()));
        let imported = match result {
            Ok(document) => document,
            Err(error) => {
                set_status(ui, format!("导入失败：{error}"), 3);
                return;
            }
        };

        let mut updated = self.profiles.clone();
        let mut last_id = None;
        for mut profile in imported.profiles {
            if updated.get(profile.id).is_some() {
                profile.id = ProfileId::new();
            }
            profile.name = unique_name(&updated, &profile.name);
            last_id = Some(profile.id);
            if let Err(error) = updated.insert(profile) {
                set_status(ui, format!("导入失败：{error}"), 3);
                return;
            }
        }
        if let Err(error) = self.store.save(&updated) {
            set_status(ui, format!("无法保存导入结果：{error}"), 3);
            return;
        }
        self.profiles = updated;
        self.select_profile(ui, last_id.or(self.selected), shared);
        set_status(ui, "中转站已导入；未包含密钥的中转站需补填 API Key", 1);
    }

    fn export_profiles(&mut self, ui: &AppWindow, include_keys: bool) {
        if !self.save_before_navigation(ui) {
            return;
        }
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Codex Switch profiles", &["toml"])
            .set_file_name("codex-switch-profiles.toml")
            .save_file()
        else {
            return;
        };
        let mut exported = self.profiles.clone();
        if !include_keys {
            for profile in &mut exported.profiles {
                profile.api_key = None;
            }
        }
        let result = ProfileStore::serialize(&exported)
            .map_err(|error| error.to_string())
            .and_then(|bytes| {
                durable_fs::atomic_write(&path, &bytes).map_err(|error| error.to_string())
            });
        match result {
            Ok(()) => set_status(
                ui,
                if include_keys {
                    "中转站已导出，文件中包含明文 API Key"
                } else {
                    "中转站已导出，未包含 API Key"
                },
                if include_keys { 2 } else { 1 },
            ),
            Err(error) => set_status(ui, format!("导出失败：{error}"), 3),
        }
    }

    fn confirm_restore(&mut self, ui: &AppWindow) {
        self.dialog = DialogAction::RestoreConfirm;
        open_dialog(
            ui,
            "恢复最近备份",
            "恢复会同时还原 config.toml 和 auth.json，并先为当前文件再创建一份备份。",
            "继续恢复",
            "",
            "取消",
            false,
        );
    }

    fn begin_restore(&mut self, ui: &AppWindow, shared: SharedController) {
        let report = process::detect_codex_processes();
        if report.is_clear() {
            spawn_restore(ui, shared, self.paths.clone(), false, None);
            return;
        }
        let desktop_only = report.has_desktop() && !report.has_command_line();
        let desktop_executable = report.desktop_executable();
        self.dialog = DialogAction::RestoreProcess {
            desktop_executable,
            desktop_only,
        };
        open_dialog(
            ui,
            "Codex 正在运行",
            if desktop_only {
                "推荐先退出 Codex Desktop，恢复完成后工具会重新打开它。"
            } else {
                "请先结束 Codex 命令行任务，再恢复配置。"
            },
            if desktop_only {
                "退出并恢复"
            } else {
                "重新检测"
            },
            "仍然恢复",
            "取消",
            false,
        );
    }
}

fn handle_dialog_primary(ui: &AppWindow, shared: SharedController) {
    let action = {
        let mut controller = shared.lock().expect("controller mutex poisoned");
        std::mem::replace(&mut controller.dialog, DialogAction::None)
    };
    close_dialog(ui);
    match action {
        DialogAction::Delete(id) => {
            shared
                .lock()
                .expect("controller mutex poisoned")
                .delete_profile(ui, id, shared.clone());
        }
        DialogAction::ChangeSelection(index) => {
            let mut controller = shared.lock().expect("controller mutex poisoned");
            let relay_saved = !ui.get_draft_dirty() || controller.save_draft(ui);
            if !relay_saved {
                return;
            }
            if ui.get_context_dirty() {
                controller.save_context(ui, shared.clone(), ContextCompletion::Select(index));
            } else {
                controller.select_index(ui, index, shared.clone());
            }
        }
        DialogAction::Export => {
            shared
                .lock()
                .expect("controller mutex poisoned")
                .export_profiles(ui, false);
        }
        DialogAction::RestoreConfirm => {
            shared
                .lock()
                .expect("controller mutex poisoned")
                .begin_restore(ui, shared.clone());
        }
        DialogAction::ApplyProcess {
            activation,
            desktop_executable,
            desktop_only,
        } => {
            if desktop_only {
                let paths = shared
                    .lock()
                    .expect("controller mutex poisoned")
                    .paths
                    .clone();
                spawn_apply(
                    ui,
                    shared,
                    paths,
                    activation,
                    ConflictPolicy::Reject,
                    true,
                    desktop_executable,
                );
            } else {
                shared
                    .lock()
                    .expect("controller mutex poisoned")
                    .continue_apply_after_process_check(ui, shared.clone(), activation);
            }
        }
        DialogAction::ApplyConflict {
            activation: _,
            desktop_executable,
        } => {
            shared
                .lock()
                .expect("controller mutex poisoned")
                .import_current_into(ui, shared.clone());
            relaunch_if_needed(ui, desktop_executable);
        }
        DialogAction::ContextProcess {
            profile_id,
            settings,
            desktop_executable,
            desktop_only,
            completion,
        } => {
            if desktop_only {
                let paths = shared
                    .lock()
                    .expect("controller mutex poisoned")
                    .paths
                    .clone();
                spawn_context_update(
                    ui,
                    shared,
                    paths,
                    ContextUpdateRequest {
                        profile_id,
                        settings,
                        completion,
                        quit_desktop: true,
                        desktop_executable,
                        policy: ConflictPolicy::Reject,
                    },
                );
            } else {
                shared
                    .lock()
                    .expect("controller mutex poisoned")
                    .continue_context_after_process_check(
                        ui,
                        shared.clone(),
                        profile_id,
                        settings,
                        completion,
                    );
            }
        }
        DialogAction::RestoreProcess {
            desktop_executable,
            desktop_only,
        } => {
            if desktop_only {
                let paths = shared
                    .lock()
                    .expect("controller mutex poisoned")
                    .paths
                    .clone();
                spawn_restore(ui, shared, paths, true, desktop_executable);
            } else {
                shared
                    .lock()
                    .expect("controller mutex poisoned")
                    .begin_restore(ui, shared.clone());
            }
        }
        DialogAction::ContextConflict {
            desktop_executable,
            completion,
            ..
        } => {
            let imported = {
                let mut controller = shared.lock().expect("controller mutex poisoned");
                let imported = controller.import_current_into(ui, shared.clone());
                if imported {
                    controller.complete_context_action(ui, completion, shared.clone());
                }
                imported
            };
            relaunch_if_needed(ui, desktop_executable);
            if !imported {
                ui.set_context_sync_error(true);
            }
        }
        DialogAction::Close => {
            let mut controller = shared.lock().expect("controller mutex poisoned");
            let context_was_dirty = ui.get_context_dirty();
            let relay_saved = !ui.get_draft_dirty() || controller.save_draft(ui);
            if !relay_saved {
                return;
            }
            if context_was_dirty {
                controller.save_context(ui, shared.clone(), ContextCompletion::Hide);
            } else {
                let _ = ui.hide();
            }
        }
        DialogAction::None => {}
    }
}

fn handle_dialog_secondary(ui: &AppWindow, shared: SharedController) {
    let action = {
        let mut controller = shared.lock().expect("controller mutex poisoned");
        std::mem::replace(&mut controller.dialog, DialogAction::None)
    };
    close_dialog(ui);
    match action {
        DialogAction::ChangeSelection(index) => {
            shared
                .lock()
                .expect("controller mutex poisoned")
                .select_index(ui, index, shared.clone());
        }
        DialogAction::Export => {
            shared
                .lock()
                .expect("controller mutex poisoned")
                .export_profiles(ui, true);
        }
        DialogAction::ApplyProcess { activation, .. } => {
            let paths = shared
                .lock()
                .expect("controller mutex poisoned")
                .paths
                .clone();
            spawn_apply(
                ui,
                shared,
                paths,
                activation,
                ConflictPolicy::Reject,
                false,
                None,
            );
        }
        DialogAction::ApplyConflict {
            activation,
            desktop_executable,
        } => {
            let paths = shared
                .lock()
                .expect("controller mutex poisoned")
                .paths
                .clone();
            spawn_apply(
                ui,
                shared,
                paths,
                activation,
                ConflictPolicy::Overwrite,
                false,
                desktop_executable,
            );
        }
        DialogAction::ContextProcess {
            profile_id,
            settings,
            completion,
            ..
        } => {
            let paths = shared
                .lock()
                .expect("controller mutex poisoned")
                .paths
                .clone();
            spawn_context_update(
                ui,
                shared,
                paths,
                ContextUpdateRequest {
                    profile_id,
                    settings,
                    completion,
                    quit_desktop: false,
                    desktop_executable: None,
                    policy: ConflictPolicy::Reject,
                },
            );
        }
        DialogAction::ContextConflict {
            profile_id,
            settings,
            desktop_executable,
            completion,
        } => {
            let paths = shared
                .lock()
                .expect("controller mutex poisoned")
                .paths
                .clone();
            spawn_context_update(
                ui,
                shared,
                paths,
                ContextUpdateRequest {
                    profile_id,
                    settings,
                    completion,
                    quit_desktop: false,
                    desktop_executable,
                    policy: ConflictPolicy::Overwrite,
                },
            );
        }
        DialogAction::RestoreProcess { .. } => {
            let paths = shared
                .lock()
                .expect("controller mutex poisoned")
                .paths
                .clone();
            spawn_restore(ui, shared, paths, false, None);
        }
        DialogAction::Close => {
            ui.set_draft_dirty(false);
            ui.set_context_dirty(false);
            let _ = ui.hide();
        }
        other => {
            let mut controller = shared.lock().expect("controller mutex poisoned");
            controller.dialog = other;
        }
    }
}

fn handle_dialog_tertiary(ui: &AppWindow, shared: SharedController) {
    let action = {
        let mut controller = shared.lock().expect("controller mutex poisoned");
        std::mem::replace(&mut controller.dialog, DialogAction::None)
    };
    close_dialog(ui);
    match action {
        DialogAction::ApplyConflict {
            desktop_executable, ..
        } => relaunch_if_needed(ui, desktop_executable),
        DialogAction::ContextConflict {
            profile_id,
            desktop_executable,
            ..
        } => {
            relaunch_if_needed(ui, desktop_executable);
            let controller = shared.lock().expect("controller mutex poisoned");
            if controller.selected == Some(profile_id) {
                ui.set_context_sync_error(true);
                ui.set_context_status("上下文配置 · 已保存，尚未同步到 Codex".into());
                ui.set_context_status_tone(2);
            }
            set_status(ui, "上下文配置已保存，但尚未同步到 Codex", 2);
        }
        DialogAction::ContextProcess { profile_id, .. } => {
            let controller = shared.lock().expect("controller mutex poisoned");
            if controller.selected == Some(profile_id) {
                ui.set_context_sync_error(true);
                ui.set_context_status("上下文配置 · 已保存，尚未同步到 Codex".into());
                ui.set_context_status_tone(2);
            }
            set_status(ui, "上下文配置已保存，但尚未同步到 Codex", 2);
        }
        _ => {}
    }
}

fn spawn_context_update(
    ui: &AppWindow,
    shared: SharedController,
    paths: AppPaths,
    request: ContextUpdateRequest,
) {
    let ContextUpdateRequest {
        profile_id,
        settings,
        completion,
        quit_desktop,
        desktop_executable,
        policy,
    } = request;
    ui.set_busy(true);
    ui.set_context_sync_error(false);
    ui.set_context_status("上下文配置 · 正在同步到 Codex".into());
    ui.set_context_status_tone(0);
    set_status(ui, "正在同步上下文配置", 0);
    let weak = ui.as_weak();
    thread::spawn(move || {
        let process_result = if quit_desktop {
            process::quit_desktop_safely(Duration::from_secs(8)).map_err(|error| error.to_string())
        } else {
            Ok(())
        };
        let result = match process_result {
            Err(error) => {
                let _ = relaunch_desktop_if_closed(desktop_executable.as_deref());
                ContextWorkerResult::Failed {
                    message: error,
                    recovery_required: false,
                }
            }
            Ok(()) => {
                let manager = TransactionManager::new(paths.clone());
                let validator =
                    CodexStagedValidator::discover_for_desktop(desktop_executable.as_deref());
                let validation_skipped = validator.is_none();
                let staged_validator = validator
                    .as_ref()
                    .map(|validator| validator as &dyn crate::transaction::StagedValidator);
                match manager.update_context_with_policy(settings, policy, staged_validator) {
                    Ok(_) => ContextWorkerResult::Updated {
                        relaunch_error: relaunch_desktop_if_closed(desktop_executable.as_deref()),
                        validation_skipped,
                    },
                    Err(TransactionError::ExternalConflict(conflict)) => {
                        ContextWorkerResult::Conflict {
                            detail: conflict.to_string(),
                            desktop_executable,
                        }
                    }
                    Err(error) => {
                        let recovery_required =
                            matches!(&error, TransactionError::RollbackFailed { .. });
                        let _ = relaunch_desktop_if_closed(desktop_executable.as_deref());
                        ContextWorkerResult::Failed {
                            message: error.to_string(),
                            recovery_required,
                        }
                    }
                }
            }
        };
        let _ = weak.upgrade_in_event_loop(move |ui| {
            ui.set_busy(false);
            let mut controller = shared.lock().expect("controller mutex poisoned");
            match result {
                ContextWorkerResult::Updated {
                    relaunch_error,
                    validation_skipped,
                } => {
                    controller.active = recognize_active_profile(
                        &controller.paths,
                        &controller.transaction,
                        &controller.profiles,
                    );
                    controller.sync_profiles(&ui);
                    ui.set_can_restore(true);
                    if controller.selected == Some(profile_id) {
                        controller.sync_context(&ui);
                    }
                    match (relaunch_error, validation_skipped) {
                        (Some(error), _) => set_status(
                            &ui,
                            format!("上下文配置已同步，但 Codex Desktop 未能重新打开：{error}"),
                            2,
                        ),
                        (None, true) => set_status(
                            &ui,
                            "上下文配置已同步；未找到 Codex 校验器，仅完成结构校验",
                            2,
                        ),
                        (None, false) => set_status(&ui, "上下文配置已同步，新会话生效", 1),
                    }
                    controller.complete_context_action(&ui, completion, shared.clone());
                }
                ContextWorkerResult::Conflict {
                    detail,
                    desktop_executable,
                } => {
                    controller.dialog = DialogAction::ContextConflict {
                        profile_id,
                        settings,
                        desktop_executable,
                        completion,
                    };
                    ui.set_context_sync_error(true);
                    ui.set_context_status("上下文配置 · 检测到外部修改".into());
                    ui.set_context_status_tone(2);
                    open_dialog(
                        &ui,
                        "检测到外部修改",
                        format!(
                            "Codex 配置已在工具外发生变化。{}\n请选择如何继续。",
                            if detail.is_empty() {
                                String::new()
                            } else {
                                format!("\n{detail}")
                            }
                        ),
                        "导入当前",
                        "保留外部并同步",
                        "取消",
                        false,
                    );
                }
                ContextWorkerResult::Failed {
                    message,
                    recovery_required,
                } => {
                    controller.active = recognize_active_profile(
                        &controller.paths,
                        &controller.transaction,
                        &controller.profiles,
                    );
                    controller.sync_profiles(&ui);
                    if controller.selected == Some(profile_id) {
                        ui.set_context_sync_error(true);
                        ui.set_context_status(
                            "上下文配置 · 已保存，但同步到 Codex 失败".into(),
                        );
                        ui.set_context_status_tone(3);
                    }
                    if recovery_required {
                        set_status(
                            &ui,
                            "上下文同步失败且自动回滚未完成。请先不要启动 Codex，重启本工具以再次恢复。",
                            3,
                        );
                    } else {
                        set_status(
                            &ui,
                            format!("配置已保存到中转站，但未能同步到 Codex：{message}"),
                            3,
                        );
                    }
                }
            }
        });
    });
}

fn spawn_apply(
    ui: &AppWindow,
    shared: SharedController,
    paths: AppPaths,
    activation: Activation,
    policy: ConflictPolicy,
    quit_desktop: bool,
    desktop_executable: Option<PathBuf>,
) {
    ui.set_busy(true);
    set_status(ui, "正在切换 Codex 配置", 0);
    let weak = ui.as_weak();
    thread::spawn(move || {
        let process_result = if quit_desktop {
            process::quit_desktop_safely(Duration::from_secs(8)).map_err(|error| error.to_string())
        } else {
            Ok(())
        };

        let worker_result = match process_result {
            Err(error) => {
                let _ = relaunch_desktop_if_closed(desktop_executable.as_deref());
                ApplyWorkerResult::Failed {
                    message: error,
                    recovery_required: false,
                }
            }
            Ok(()) => {
                let manager = TransactionManager::new(paths);
                let validator =
                    CodexStagedValidator::discover_for_desktop(desktop_executable.as_deref());
                let validation_skipped = validator.is_none();
                let staged_validator = validator
                    .as_ref()
                    .map(|validator| validator as &dyn crate::transaction::StagedValidator);
                match manager.apply_validated(&activation, policy, staged_validator) {
                    Ok(outcome) => {
                        let relaunch_error =
                            relaunch_desktop_if_closed(desktop_executable.as_deref());
                        ApplyWorkerResult::Applied {
                            state: outcome.state,
                            relaunch_error,
                            validation_skipped,
                        }
                    }
                    Err(TransactionError::ExternalConflict(conflict)) => {
                        ApplyWorkerResult::Conflict {
                            detail: conflict.to_string(),
                            desktop_executable,
                        }
                    }
                    Err(error) => {
                        let recovery_required =
                            matches!(&error, TransactionError::RollbackFailed { .. });
                        let _ = relaunch_desktop_if_closed(desktop_executable.as_deref());
                        ApplyWorkerResult::Failed {
                            message: error.to_string(),
                            recovery_required,
                        }
                    }
                }
            }
        };

        let _ = weak.upgrade_in_event_loop(move |ui| {
            ui.set_busy(false);
            let mut controller = shared.lock().expect("controller mutex poisoned");
            match worker_result {
                ApplyWorkerResult::Applied {
                    state,
                    relaunch_error,
                    validation_skipped,
                } => {
                    controller.active = state.active_profile_id;
                    controller.sync_profiles(&ui);
                    ui.set_can_restore(true);
                    match (relaunch_error, validation_skipped) {
                        (Some(error), _) => set_status(
                            &ui,
                            format!("切换完成，但 Codex Desktop 未能重新打开：{error}"),
                            2,
                        ),
                        (None, true) => {
                            set_status(&ui, "切换完成；未找到 Codex 校验器，仅完成结构校验", 2)
                        }
                        (None, false) => set_status(&ui, "切换完成", 1),
                    }
                }
                ApplyWorkerResult::Conflict {
                    detail,
                    desktop_executable,
                } => {
                    controller.dialog = DialogAction::ApplyConflict {
                        activation,
                        desktop_executable,
                    };
                    open_dialog(
                        &ui,
                        "检测到外部修改",
                        format!(
                            "Codex 的中转站或模型配置已在工具外发生变化。{}\n请选择如何继续。",
                            if detail.is_empty() {
                                String::new()
                            } else {
                                format!("\n{detail}")
                            }
                        ),
                        "导入当前",
                        "覆盖",
                        "取消",
                        false,
                    );
                }
                ApplyWorkerResult::Failed {
                    message,
                    recovery_required,
                } => {
                    if recovery_required {
                        set_status(
                            &ui,
                            "切换失败且自动回滚未完成。请先不要启动 Codex，重启本工具以再次恢复；若仍失败，请检查备份。",
                            3,
                        );
                    } else {
                        set_status(&ui, format!("切换失败，未提交新配置：{message}"), 3);
                    }
                }
            }
        });
    });
}

fn spawn_restore(
    ui: &AppWindow,
    shared: SharedController,
    paths: AppPaths,
    quit_desktop: bool,
    desktop_executable: Option<PathBuf>,
) {
    ui.set_busy(true);
    set_status(ui, "正在恢复最近备份", 0);
    let weak = ui.as_weak();
    thread::spawn(move || {
        let process_result = if quit_desktop {
            process::quit_desktop_safely(Duration::from_secs(8)).map_err(|error| error.to_string())
        } else {
            Ok(())
        };
        let worker_result = match process_result {
            Err(error) => {
                let _ = relaunch_desktop_if_closed(desktop_executable.as_deref());
                RestoreWorkerResult::Failed {
                    message: error,
                    recovery_required: false,
                }
            }
            Ok(()) => match TransactionManager::new(paths).restore_latest() {
                Ok(_) => {
                    let relaunch_error = relaunch_desktop_if_closed(desktop_executable.as_deref());
                    RestoreWorkerResult::Restored { relaunch_error }
                }
                Err(error) => {
                    let recovery_required =
                        matches!(&error, TransactionError::RollbackFailed { .. });
                    let _ = relaunch_desktop_if_closed(desktop_executable.as_deref());
                    RestoreWorkerResult::Failed {
                        message: error.to_string(),
                        recovery_required,
                    }
                }
            },
        };
        let _ = weak.upgrade_in_event_loop(move |ui| {
            ui.set_busy(false);
            let mut controller = shared.lock().expect("controller mutex poisoned");
            match worker_result {
                RestoreWorkerResult::Restored { relaunch_error } => {
                    controller.active = recognize_active_profile(
                        &controller.paths,
                        &controller.transaction,
                        &controller.profiles,
                    );
                    controller.sync_profiles(&ui);
                    ui.set_can_restore(true);
                    match relaunch_error {
                        Some(error) => set_status(
                            &ui,
                            format!("备份已恢复，但 Codex Desktop 未能重新打开：{error}"),
                            2,
                        ),
                        None => set_status(&ui, "最近备份已恢复", 1),
                    }
                }
                RestoreWorkerResult::Failed {
                    message,
                    recovery_required,
                } => {
                    if recovery_required {
                        set_status(
                            &ui,
                            "恢复失败且自动回滚未完成。请重启本工具再次恢复，并检查备份。",
                            3,
                        );
                    } else {
                        set_status(&ui, format!("恢复失败：{message}"), 3);
                    }
                }
            }
        });
    });
}

fn profile_from_ui(
    ui: &AppWindow,
    id: ProfileId,
    context: Option<crate::domain::ProfileContext>,
) -> Result<Profile, String> {
    let api_key = {
        let value = ui.get_draft_api_key().to_string();
        if value.is_empty() {
            None
        } else {
            Some(ApiKey::new(value).map_err(|error| error.to_string())?)
        }
    };
    let review_model = if ui.get_advanced_open() {
        let value = ui.get_draft_review_model().trim().to_owned();
        (!value.is_empty()).then_some(value)
    } else {
        None
    };
    let profile = Profile {
        id,
        name: ui.get_draft_name().trim().to_owned(),
        base_url: ui.get_draft_base_url().to_string(),
        api_key,
        model: ui.get_draft_model().trim().to_owned(),
        review_model,
        context,
    };
    profile.validate().map_err(|error| error.to_string())?;
    Ok(profile)
}

fn import_live_profile(paths: &AppPaths) -> Result<Profile, String> {
    let config_bytes = durable_fs::read_optional(&paths.codex_config)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "config.toml 不存在".to_owned())?;
    let config = std::str::from_utf8(&config_bytes)
        .map_err(|_| "config.toml 不是有效的 UTF-8".to_owned())?;
    let auth = durable_fs::read_optional(&paths.codex_auth).map_err(|error| error.to_string())?;
    codex_config::import_current_profile(config, auth.as_deref()).map_err(config_error_for_user)
}

fn config_error_for_user(error: CodexConfigError) -> String {
    match error {
        CodexConfigError::ProviderNotDefined(_) => {
            "当前 model_provider 不是可导入的自定义中转站".to_owned()
        }
        CodexConfigError::MissingApiKey => "auth.json 中没有 OPENAI_API_KEY".to_owned(),
        other => other.to_string(),
    }
}

fn recognize_active_profile(
    paths: &AppPaths,
    transaction: &TransactionManager,
    profiles: &ProfilesDocument,
) -> Option<ProfileId> {
    // Profiles written by current versions carry a stable UUID in `model_provider`. It remains
    // the source of truth for the active relay even when the saved profile was edited later.
    if let Some(profile_id) = managed_live_profile_id(paths) {
        return profiles.get(profile_id).map(|_| profile_id);
    }

    let imported = import_live_profile(paths).ok()?;
    if let Ok(Some(state)) = transaction.load_state()
        && current_fingerprint_matches(paths, &state.relevant_fingerprint)
        && let Some(active_id) = state.active_profile_id
        && profiles
            .get(active_id)
            .is_some_and(|profile| same_connection(profile, &imported))
    {
        return Some(active_id);
    }
    profiles
        .profiles
        .iter()
        .find(|profile| same_connection(profile, &imported))
        .map(|profile| profile.id)
}

fn managed_live_profile_id(paths: &AppPaths) -> Option<ProfileId> {
    let config_bytes = durable_fs::read_optional(&paths.codex_config)
        .ok()
        .flatten()?;
    let config = std::str::from_utf8(&config_bytes).ok()?;
    codex_config::inspect_codex_config(config)
        .ok()?
        .model_provider
        .as_deref()
        .and_then(profile_id_from_provider_id)
}

fn current_fingerprint_matches(paths: &AppPaths, expected: &str) -> bool {
    let Some(config_bytes) = durable_fs::read_optional(&paths.codex_config)
        .ok()
        .flatten()
    else {
        return false;
    };
    let Some(config) = std::str::from_utf8(&config_bytes).ok() else {
        return false;
    };
    let Some(auth) = durable_fs::read_optional(&paths.codex_auth).ok() else {
        return false;
    };
    let Some(projection) = codex_config::relevant_projection(config, auth.as_deref()).ok() else {
        return false;
    };
    codex_config::relevant_fingerprint(config, auth.as_deref()).is_ok_and(|value| value == expected)
        || codex_config::pre_context_relevant_fingerprint(&projection)
            .is_ok_and(|value| value == expected)
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

fn same_model_catalog(left: &Profile, right: &Profile) -> bool {
    left.base_url == right.base_url && left.api_key == right.api_key
}

fn profile_owns_model_cache(saved: Option<&Profile>, fetched_from: &Profile) -> bool {
    saved
        .is_some_and(|saved| saved.id == fetched_from.id && same_model_catalog(saved, fetched_from))
}

fn unique_name(document: &ProfilesDocument, requested: &str) -> String {
    if !document
        .profiles
        .iter()
        .any(|profile| profile.name.eq_ignore_ascii_case(requested))
    {
        return requested.to_owned();
    }
    for suffix in 2.. {
        let candidate = format!("{requested} {suffix}");
        if !document
            .profiles
            .iter()
            .any(|profile| profile.name.eq_ignore_ascii_case(&candidate))
        {
            return candidate;
        }
    }
    unreachable!()
}

fn set_model_list(ui: &AppWindow, models: &[String], selected: &str) {
    let mut available = models.to_vec();
    if !selected.is_empty() && !available.iter().any(|model| model == selected) {
        available.insert(0, selected.to_owned());
    }
    let values: Vec<SharedString> = available.iter().cloned().map(Into::into).collect();
    let index = available
        .iter()
        .position(|model| model == selected)
        .and_then(|index| i32::try_from(index).ok())
        .unwrap_or(-1);
    ui.set_model_names(ModelRc::new(VecModel::from(values)));
    ui.set_model_index(index);
}

fn empty_string_model() -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(Vec::<SharedString>::new()))
}

fn empty_usage_trend_model() -> ModelRc<UsageTrendRow> {
    ModelRc::new(VecModel::from(Vec::<UsageTrendRow>::new()))
}

fn empty_usage_model() -> ModelRc<UsageModelRow> {
    ModelRc::new(VecModel::from(Vec::<UsageModelRow>::new()))
}

fn open_dialog(
    ui: &AppWindow,
    title: impl Into<SharedString>,
    message: impl Into<SharedString>,
    primary: impl Into<SharedString>,
    secondary: impl Into<SharedString>,
    tertiary: impl Into<SharedString>,
    primary_danger: bool,
) {
    ui.set_dialog_title(title.into());
    ui.set_dialog_message(message.into());
    ui.set_dialog_primary_text(primary.into());
    ui.set_dialog_secondary_text(secondary.into());
    ui.set_dialog_tertiary_text(tertiary.into());
    ui.set_dialog_primary_danger(primary_danger);
    ui.set_dialog_open(true);
}

fn close_dialog(ui: &AppWindow) {
    ui.set_dialog_open(false);
}

fn set_status(ui: &AppWindow, message: impl Into<SharedString>, tone: i32) {
    ui.set_status_text(message.into());
    ui.set_status_tone(tone);
}

fn set_context_status(ui: &AppWindow, message: impl Into<SharedString>, tone: i32) {
    ui.set_context_status(message.into());
    ui.set_context_status_tone(tone);
}

fn read_config_text(paths: &AppPaths) -> Option<String> {
    let bytes = durable_fs::read_optional(&paths.codex_config).ok()??;
    String::from_utf8(bytes).ok()
}

fn context_summary(context: ProfileContext) -> String {
    let window = context
        .model_context_window
        .map(|value| format!("{} 窗口", format_compact_tokens(value)))
        .unwrap_or_else(|| "自动窗口".to_owned());
    let compact = match (
        context.model_auto_compact_token_limit,
        context.model_context_window,
    ) {
        (Some(limit), Some(window)) if window > 0 => {
            let percent = rounded_percent(limit, window).unwrap_or(0);
            if context.model_auto_compact_token_limit_scope
                == Some(AutoCompactScope::BodyAfterPrefix)
            {
                format!("增长压缩 {percent}%")
            } else {
                format!("压缩 {percent}%")
            }
        }
        (Some(limit), None) => format!("{} 自动压缩", format_compact_tokens(limit)),
        _ => "自动压缩".to_owned(),
    };
    format!("{window} · 输出不限 · {compact}")
}

fn parse_context_window_k(input: &str) -> Result<u64, ()> {
    let input = input.trim();
    if input.is_empty() {
        return Err(());
    }

    let mut parts = input.split('.');
    let whole = parts.next().ok_or(())?;
    let fraction = parts.next();
    if parts.next().is_some()
        || whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(());
    }

    let whole_tokens = whole
        .parse::<u64>()
        .map_err(|_| ())?
        .checked_mul(1_000)
        .ok_or(())?;
    let fraction_tokens = match fraction {
        None => 0,
        Some(fraction)
            if !fraction.is_empty()
                && fraction.len() <= 3
                && fraction.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            let value = fraction.parse::<u64>().map_err(|_| ())?;
            value
                * match fraction.len() {
                    1 => 100,
                    2 => 10,
                    3 => 1,
                    _ => unreachable!(),
                }
        }
        Some(_) => return Err(()),
    };
    let tokens = whole_tokens.checked_add(fraction_tokens).ok_or(())?;

    (MIN_CONTEXT_WINDOW_TOKENS..=MAX_CONTEXT_WINDOW_TOKENS)
        .contains(&tokens)
        .then_some(tokens)
        .ok_or(())
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

fn usage_period_from_index(index: i32) -> Option<UsagePeriod> {
    match index {
        0 => Some(UsagePeriod::Today),
        1 => Some(UsagePeriod::Last7Days),
        2 => Some(UsagePeriod::Last30Days),
        _ => None,
    }
}

fn usage_period_index(period: UsagePeriod) -> i32 {
    match period {
        UsagePeriod::Today => 0,
        UsagePeriod::Last7Days => 1,
        UsagePeriod::Last30Days => 2,
    }
}

fn usage_period_total_label(period: UsagePeriod) -> &'static str {
    match period {
        UsagePeriod::Today => "今日总计",
        UsagePeriod::Last7Days => "近 7 天总计",
        UsagePeriod::Last30Days => "近 30 天总计",
    }
}

fn usage_period_models_label(period: UsagePeriod) -> &'static str {
    match period {
        UsagePeriod::Today => "今日模型分布",
        UsagePeriod::Last7Days => "所选范围模型分布 · 近 7 天",
        UsagePeriod::Last30Days => "所选范围模型分布 · 近 30 天",
    }
}

fn usage_trend_label(period: UsagePeriod) -> &'static str {
    match period {
        UsagePeriod::Today => "今日用量趋势 · 红=输入 / 深灰=输出 · 各自按峰值缩放",
        UsagePeriod::Last7Days => "每日用量趋势 · 近 7 天 · 红=输入 / 深灰=输出 · 各自按峰值缩放",
        UsagePeriod::Last30Days => "每日用量趋势 · 近 30 天 · 红=输入 / 深灰=输出 · 各自按峰值缩放",
    }
}

fn usage_trend_unit_label(period: UsagePeriod) -> &'static str {
    match period {
        UsagePeriod::Today => "单小时",
        UsagePeriod::Last7Days | UsagePeriod::Last30Days => "单日",
    }
}

fn daily_usage_selection(
    days: &[DailyUsage],
    hover_index: i32,
) -> Option<(&DailyUsage, TokenUsage)> {
    let index = usize::try_from(hover_index)
        .ok()
        .filter(|index| *index < days.len())
        .unwrap_or(days.len().checked_sub(1)?);
    let previous = index
        .checked_sub(1)
        .map(|index| days[index].usage)
        .unwrap_or_default();
    Some((&days[index], previous))
}

fn sync_usage_daily_metrics(ui: &AppWindow, report: &UsageReport, hover_index: i32) {
    let Some((day, previous)) = daily_usage_selection(&report.daily, hover_index) else {
        ui.set_usage_day_label("暂无日数据".into());
        ui.set_usage_input("0".into());
        ui.set_usage_cached("0".into());
        ui.set_usage_output("0".into());
        ui.set_usage_calls("0 次".into());
        ui.set_usage_input_delta("".into());
        ui.set_usage_cached_delta("".into());
        ui.set_usage_output_delta("".into());
        ui.set_usage_calls_delta("".into());
        ui.set_usage_input_delta_tone(0);
        ui.set_usage_cached_delta_tone(0);
        ui.set_usage_output_delta_tone(0);
        ui.set_usage_calls_delta_tone(0);
        return;
    };

    let current = day.usage;
    ui.set_usage_day_label(day.date.to_string().into());
    ui.set_usage_input(format_compact_tokens(current.input_tokens).into());
    ui.set_usage_cached(format_compact_tokens(current.cached_input_tokens).into());
    ui.set_usage_output(format_compact_tokens(current.output_tokens).into());
    ui.set_usage_calls(format!("{} 次", format_integer(current.calls)).into());
    let (delta, tone) = usage_delta(current.input_tokens, previous.input_tokens);
    ui.set_usage_input_delta(delta.into());
    ui.set_usage_input_delta_tone(tone);
    let (delta, tone) = usage_delta(current.cached_input_tokens, previous.cached_input_tokens);
    ui.set_usage_cached_delta(delta.into());
    ui.set_usage_cached_delta_tone(tone);
    let (delta, tone) = usage_delta(current.output_tokens, previous.output_tokens);
    ui.set_usage_output_delta(delta.into());
    ui.set_usage_output_delta_tone(tone);
    let (delta, tone) = usage_delta(current.calls, previous.calls);
    ui.set_usage_calls_delta(delta.into());
    ui.set_usage_calls_delta_tone(tone);
}

fn usage_line_path(values: impl IntoIterator<Item = u64>) -> String {
    const PATH_SIZE: u64 = 1_000;

    let values: Vec<u64> = values.into_iter().collect();
    let maximum = values.iter().copied().max().unwrap_or(0);
    let count = values.len().saturating_sub(1) as u64;
    let mut path = String::new();
    for (index, value) in values.into_iter().enumerate() {
        let x = (index as u64 * PATH_SIZE)
            .checked_div(count)
            .unwrap_or(PATH_SIZE / 2);
        let scaled = if maximum == 0 {
            0
        } else {
            (u128::from(value) * u128::from(PATH_SIZE) / u128::from(maximum))
                .min(u128::from(PATH_SIZE)) as u64
        };
        let y = PATH_SIZE - scaled;
        let command = if index == 0 { "M" } else { "L" };
        let _ = write!(path, "{command} {x} {y} ");
    }
    path
}

fn usage_trend_hover_index(point_count: usize, ratio: f32) -> Option<i32> {
    if point_count == 0 || !ratio.is_finite() {
        return None;
    }
    let last_index = point_count.saturating_sub(1);
    let index = if last_index == 0 {
        0
    } else {
        (ratio.clamp(0.0, 1.0) * last_index as f32).round() as usize
    };
    i32::try_from(index).ok()
}

fn usage_delta(current: u64, previous: u64) -> (String, i32) {
    match current.cmp(&previous) {
        std::cmp::Ordering::Equal => ("与前一日持平".to_owned(), 0),
        std::cmp::Ordering::Greater if previous == 0 => ("新增用量".to_owned(), 1),
        ordering => {
            let difference = current.abs_diff(previous);
            let percent = difference.saturating_mul(100).saturating_add(previous / 2) / previous;
            if ordering == std::cmp::Ordering::Greater {
                (format!("↑ {percent}% 较前一日"), 1)
            } else {
                (format!("↓ {percent}% 较前一日"), -1)
            }
        }
    }
}

fn format_integer(value: u64) -> String {
    let digits = value.to_string();
    let mut formatted = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            formatted.push(',');
        }
        formatted.push(character);
    }
    formatted
}

fn format_compact_tokens(value: u64) -> String {
    const THOUSAND: u64 = 1_000;
    const MILLION: u64 = 1_000_000;
    const BILLION: u64 = 1_000_000_000;
    if value < THOUSAND {
        return value.to_string();
    }
    let (divisor, suffix) = if value < MILLION {
        (THOUSAND, "K")
    } else if value < BILLION {
        (MILLION, "M")
    } else {
        (BILLION, "B")
    };
    let whole = value / divisor;
    let decimal = value % divisor * 10 / divisor;
    if decimal == 0 {
        format!("{whole}{suffix}")
    } else {
        format!("{whole}.{decimal}{suffix}")
    }
}

fn relaunch_if_needed(ui: &AppWindow, executable: Option<PathBuf>) {
    if let Some(error) = relaunch_desktop_if_closed(executable.as_deref()) {
        set_status(ui, format!("Codex Desktop 未能重新打开：{error}"), 2);
    }
}

fn relaunch_desktop_if_closed(executable: Option<&Path>) -> Option<String> {
    let executable = executable?;
    if process::detect_codex_processes().has_desktop() {
        return None;
    }
    process::relaunch_desktop(Some(executable))
        .err()
        .map(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashSet;
    use std::rc::Rc;

    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
    use slint::platform::{Platform, PlatformError, WindowAdapter, WindowEvent};
    use slint::{PhysicalSize, Rgb8Pixel, SharedPixelBuffer};

    use super::*;

    thread_local! {
        static SNAPSHOT_WINDOW: RefCell<Option<Rc<MinimalSoftwareWindow>>> = const { RefCell::new(None) };
    }

    struct SnapshotPlatform;

    impl Platform for SnapshotPlatform {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
            SNAPSHOT_WINDOW.with(|window| {
                let adapter = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
                *window.borrow_mut() = Some(adapter.clone());
                let adapter: Rc<dyn WindowAdapter> = adapter;
                Ok(adapter)
            })
        }
    }

    fn control_rule_horizontal_edges(
        pixels: &SharedPixelBuffer<Rgb8Pixel>,
        minimum_width: u32,
    ) -> Vec<(u32, u32, u32)> {
        const CONTROL_RULE: (u8, u8, u8) = (154, 147, 138);
        let width = pixels.width();
        let mut edges = Vec::new();

        for y in 0..pixels.height() {
            let mut start = None;
            for x in 0..width {
                let pixel = pixels.as_slice()[(y * width + x) as usize];
                let matches = (pixel.r, pixel.g, pixel.b) == CONTROL_RULE;
                if matches && start.is_none() {
                    start = Some(x);
                }
                if (!matches || x + 1 == width) && start.is_some() {
                    let start = start.take().unwrap();
                    let end = if matches { x } else { x - 1 };
                    if end - start + 1 >= minimum_width {
                        edges.push((y, start, end));
                    }
                }
            }
        }

        edges
    }

    fn assert_context_control_geometry(pixels: &SharedPixelBuffer<Rgb8Pixel>, scale_factor: u32) {
        let edges = control_rule_horizontal_edges(pixels, 160 * scale_factor);
        let mut edge_bands: Vec<(u32, u32, u32, u32)> = Vec::new();
        for (y, start, end) in edges {
            if let Some((_, last_y, left, right)) = edge_bands.last_mut()
                && y <= *last_y + 1
            {
                *last_y = y;
                *left = (*left).min(start);
                *right = (*right).max(end);
            } else {
                edge_bands.push((y, y, start, end));
            }
        }
        assert_eq!(
            edge_bands.len(),
            6,
            "expected top and bottom borders for exactly three context controls, got {edge_bands:?}"
        );

        let expected_height = 36 * scale_factor;
        let expected_row_distance = 57 * scale_factor;
        let mut top_edges = Vec::new();
        for pair in edge_bands.as_chunks::<2>().0 {
            let (top_y, _, top_start, top_end) = pair[0];
            let (_, bottom_y, bottom_start, bottom_end) = pair[1];
            assert_eq!(top_start, bottom_start);
            assert_eq!(top_end, bottom_end);
            assert_eq!(bottom_y - top_y + 1, expected_height);
            assert!(top_end - top_start + 1 >= 160 * scale_factor);
            top_edges.push((top_y, top_start, top_end));
        }

        assert_eq!(top_edges[0].1, top_edges[1].1);
        assert_eq!(top_edges[1].1, top_edges[2].1);
        assert_eq!(top_edges[0].2, top_edges[1].2);
        assert_eq!(top_edges[1].2, top_edges[2].2);
        assert_eq!(top_edges[1].0 - top_edges[0].0, expected_row_distance);
        assert_eq!(top_edges[2].0 - top_edges[1].0, expected_row_distance);
    }

    #[test]
    fn unique_names_are_case_insensitive() {
        let mut document = ProfilesDocument::default();
        document
            .insert(
                Profile::without_api_key("Relay", "https://relay.example/v1", "gpt-5", None)
                    .unwrap(),
            )
            .unwrap();

        assert_eq!(unique_name(&document, "relay"), "relay 2");
    }

    #[test]
    fn unsaved_connection_draft_does_not_own_the_saved_profiles_model_cache() {
        let saved = Profile::new(
            "Relay",
            "https://relay-a.example/v1",
            ApiKey::new("sk-a").unwrap(),
            "model-a",
            None,
        )
        .unwrap();
        let mut draft = saved.clone();
        draft.base_url = "https://relay-b.example/v1".to_owned();
        draft.api_key = Some(ApiKey::new("sk-b").unwrap());

        assert!(profile_owns_model_cache(Some(&saved), &saved));
        assert!(!profile_owns_model_cache(Some(&saved), &draft));
        assert!(!profile_owns_model_cache(None, &draft));
    }

    #[test]
    fn usage_scope_keeps_new_provider_ids_exact_and_selects_only_its_legacy_windows() {
        let openai = ProfileId::from_uuid(
            uuid::Uuid::parse_str("761a7f20-bbeb-463c-8606-b4ac09d92853").unwrap(),
        );
        let casdao = ProfileId::from_uuid(
            uuid::Uuid::parse_str("e519bc8f-120c-43c3-96b5-a7799f6eec18").unwrap(),
        );
        let windows = vec![
            ProfileLegacyUsageWindow {
                profile_id: casdao,
                start_unix_ms: 100,
                end_exclusive_unix_ms: 200,
            },
            ProfileLegacyUsageWindow {
                profile_id: openai,
                start_unix_ms: 200,
                end_exclusive_unix_ms: 300,
            },
        ];

        let casdao_provider_id = provider_id_for_profile(casdao);
        assert_eq!(
            usage_scope(Some(casdao), &windows).provider_filter(),
            Some(casdao_provider_id.as_str())
        );
        assert!(includes_inferred_legacy_usage(Some(casdao), &windows));
        assert!(includes_inferred_legacy_usage(Some(openai), &windows));
        assert!(!includes_inferred_legacy_usage(None, &windows));
        assert_eq!(usage_scope(None, &windows).provider_filter(), None);
    }

    #[test]
    fn model_changed_live_legacy_usage_starts_at_the_current_config_revision() {
        let profile = Profile::new(
            "Relay",
            "https://relay.example/v1",
            ApiKey::new("sk-test").unwrap(),
            "gpt-5.6-sol",
            None,
        )
        .unwrap();
        let profile_id = profile.id;
        let mut profiles = ProfilesDocument::default();
        profiles.insert(profile.clone()).unwrap();

        let mut live = profile.clone();
        live.model = "gpt-5.6-terra".to_owned();
        let existing = [ProfileLegacyUsageWindow {
            profile_id,
            start_unix_ms: 100,
            end_exclusive_unix_ms: 400,
        }];

        assert_eq!(
            infer_unvalidated_live_legacy_window(
                Some(profile_id),
                Some(&live),
                &profiles,
                Some(300),
                &existing,
            ),
            Some(ProfileLegacyUsageWindow {
                profile_id,
                start_unix_ms: 400,
                end_exclusive_unix_ms: u64::MAX,
            })
        );

        let mut duplicate = profile.clone();
        duplicate.id = ProfileId::new();
        profiles.profiles.push(duplicate);
        assert_eq!(
            infer_unvalidated_live_legacy_window(
                Some(profile_id),
                Some(&live),
                &profiles,
                Some(400),
                &existing,
            ),
            None
        );
        profiles.profiles.pop();

        live.base_url = "https://different.example/v1".to_owned();
        assert_eq!(
            infer_unvalidated_live_legacy_window(
                Some(profile_id),
                Some(&live),
                &profiles,
                Some(400),
                &existing,
            ),
            None
        );

        let mut live = profile.clone();
        live.model = "gpt-5.6-terra".to_owned();
        live.api_key = Some(ApiKey::new("sk-different").unwrap());
        assert_eq!(
            infer_unvalidated_live_legacy_window(
                Some(profile_id),
                Some(&live),
                &profiles,
                Some(400),
                &existing,
            ),
            None
        );

        let open_window = [ProfileLegacyUsageWindow {
            profile_id,
            start_unix_ms: 400,
            end_exclusive_unix_ms: u64::MAX,
        }];
        assert_eq!(
            infer_unvalidated_live_legacy_window(
                Some(profile_id),
                Some(&profile),
                &profiles,
                Some(400),
                &open_window,
            ),
            None
        );
    }

    #[test]
    fn shared_provider_model_override_builds_a_transient_live_usage_window() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_home(temp.path());
        let transaction = TransactionManager::new(paths.clone());
        let profile = Profile::new(
            "Relay",
            "https://relay.example/v1",
            ApiKey::new("sk-test").unwrap(),
            "gpt-5.6-sol",
            None,
        )
        .unwrap();
        let profile_id = profile.id;
        let mut profiles = ProfilesDocument::default();
        profiles.insert(profile).unwrap();

        let config = r#"model_provider = "codex_switch"
model = "gpt-5.6-terra"

[model_providers.codex_switch]
name = "Relay"
base_url = "https://relay.example/v1"
wire_api = "responses"
requires_openai_auth = true
"#;
        durable_fs::atomic_write(&paths.codex_config, config.as_bytes()).unwrap();
        durable_fs::atomic_write(&paths.codex_auth, br#"{"OPENAI_API_KEY":"sk-test"}"#).unwrap();
        let state = ManagedState {
            schema_version: 1,
            active_profile_id: Some(profile_id),
            relevant_fingerprint: "0".repeat(64),
        };
        durable_fs::atomic_write(&paths.state, &serde_json::to_vec(&state).unwrap()).unwrap();

        let config_modified_at_unix_ms = config_modified_unix_ms(&paths.codex_config).unwrap();
        let window = inferred_live_legacy_window(
            &paths,
            &transaction,
            &profiles,
            &[ProfileLegacyUsageWindow {
                profile_id,
                start_unix_ms: 10,
                end_exclusive_unix_ms: 20,
            }],
        );
        assert_eq!(
            window,
            Some(ProfileLegacyUsageWindow {
                profile_id,
                start_unix_ms: config_modified_at_unix_ms.max(20),
                end_exclusive_unix_ms: u64::MAX,
            })
        );
    }

    #[test]
    fn managed_provider_identity_keeps_an_edited_profile_reported_as_active() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_home(temp.path());
        durable_fs::atomic_write(&paths.codex_config, b"").unwrap();
        let manager = TransactionManager::new(paths.clone());
        let profile = Profile::new(
            "Relay",
            "https://relay.example/v1",
            ApiKey::new("sk-test").unwrap(),
            "model-before",
            None,
        )
        .unwrap();
        manager
            .apply(&profile.activation().unwrap(), ConflictPolicy::Overwrite)
            .unwrap();

        let config = fs::read_to_string(&paths.codex_config).unwrap();
        let auth = fs::read(&paths.codex_auth).unwrap();
        let projection = codex_config::relevant_projection(&config, Some(&auth)).unwrap();
        let mut legacy_state = manager.load_state().unwrap().unwrap();
        legacy_state.relevant_fingerprint =
            codex_config::pre_context_relevant_fingerprint(&projection).unwrap();
        let mut legacy_state_bytes = serde_json::to_vec_pretty(&legacy_state).unwrap();
        legacy_state_bytes.push(b'\n');
        durable_fs::atomic_write(&paths.state, &legacy_state_bytes).unwrap();

        let mut document = ProfilesDocument::default();
        document.insert(profile).unwrap();
        assert!(recognize_active_profile(&paths, &manager, &document).is_some());

        document.profiles[0].model = "model-after".to_owned();
        assert_eq!(
            recognize_active_profile(&paths, &manager, &document),
            Some(document.profiles[0].id)
        );
    }

    #[test]
    fn unknown_managed_provider_is_not_matched_by_connection_details() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_home(temp.path());
        let manager = TransactionManager::new(paths.clone());
        let profile = Profile::new(
            "Relay",
            "https://relay.example/v1",
            ApiKey::new("sk-test").unwrap(),
            "gpt-5.6-sol",
            None,
        )
        .unwrap();
        let mut document = ProfilesDocument::default();
        document.insert(profile).unwrap();

        let unknown_provider = provider_id_for_profile(ProfileId::new());
        let config = format!(
            r#"model_provider = "{unknown_provider}"
model = "gpt-5.6-sol"

[model_providers.{unknown_provider}]
name = "Relay"
base_url = "https://relay.example/v1"
wire_api = "responses"
requires_openai_auth = true
"#
        );
        durable_fs::atomic_write(&paths.codex_config, config.as_bytes()).unwrap();
        durable_fs::atomic_write(&paths.codex_auth, br#"{"OPENAI_API_KEY":"sk-test"}"#).unwrap();

        assert_eq!(recognize_active_profile(&paths, &manager, &document), None);
    }

    #[test]
    fn explicit_context_must_match_before_a_profile_is_reported_active() {
        let mut saved = Profile::new(
            "Relay",
            "https://relay.example/v1",
            ApiKey::new("sk-test").unwrap(),
            "gpt-5",
            None,
        )
        .unwrap();
        let mut live = saved.clone();
        saved.context = Some(ProfileContext {
            model_context_window: Some(272_000),
            model_auto_compact_token_limit: Some(217_600),
            model_auto_compact_token_limit_scope: Some(AutoCompactScope::Total),
        });
        live.context = Some(ProfileContext::default());

        assert!(!same_connection(&saved, &live));
        saved.context = None;
        assert!(same_connection(&saved, &live));
    }

    #[test]
    fn context_window_k_input_converts_to_positive_token_counts() {
        assert_eq!(parse_context_window_k("500"), Ok(500_000));
        assert_eq!(parse_context_window_k(" 272.5 "), Ok(272_500));
        assert_eq!(parse_context_window_k("0.02"), Ok(20));
        assert_eq!(parse_context_window_k("2000.001"), Ok(2_000_001));
        assert_eq!(
            parse_context_window_k("9223372036854775.807"),
            Ok(MAX_CONTEXT_WINDOW_TOKENS)
        );

        for invalid in [
            "",
            "0",
            "0.000",
            "0.001",
            "0.019",
            "500K",
            "1.2.3",
            "9223372036854775.808",
            "18446744073709551615",
        ] {
            assert_eq!(
                parse_context_window_k(invalid),
                Err(()),
                "accepted {invalid}"
            );
        }
    }

    #[test]
    fn context_window_tokens_are_formatted_in_k_without_losing_precision() {
        assert_eq!(format_context_window_k(500_000), "500");
        assert_eq!(format_context_window_k(272_500), "272.5");
        assert_eq!(format_context_window_k(272_050), "272.05");
        assert_eq!(format_context_window_k(272_005), "272.005");
    }

    #[test]
    fn context_percent_handles_the_largest_persistable_window() {
        let limit = (u128::from(MAX_CONTEXT_WINDOW_TOKENS) * 80 / 100) as u64;
        assert_eq!(rounded_percent(limit, MAX_CONTEXT_WINDOW_TOKENS), Some(80));
        assert_eq!(rounded_percent(1, 0), None);
    }

    #[test]
    fn smallest_context_window_round_trips_every_compact_percent_step() {
        let window = parse_context_window_k("0.02").unwrap();
        for percent in (50..=95).step_by(5) {
            let limit = compact_limit_for_percent(window, percent);
            let context = ProfileContext {
                model_context_window: Some(window),
                model_auto_compact_token_limit: Some(limit),
                model_auto_compact_token_limit_scope: Some(AutoCompactScope::Total),
            };

            assert!(context.validate().is_ok());
            assert_eq!(rounded_percent(limit, window), Some(percent));
        }
    }

    #[test]
    fn renders_each_dashboard_page_to_a_distinct_nonblank_frame() {
        let _ = slint::platform::set_platform(Box::new(SnapshotPlatform));
        let ui = AppWindow::new().unwrap();
        let window = SNAPSHOT_WINDOW.with(|window| window.borrow().clone().unwrap());
        ui.set_profiles(ModelRc::new(VecModel::from(vec![ProfileRow {
            id: "profile-1".into(),
            name: "日常站点".into(),
            base_url: "https://relay.example/v1".into(),
            model: "gpt-5.2-codex".into(),
            is_active: true,
        }])));
        ui.set_selected_index(0);
        ui.set_draft_name("日常站点".into());
        ui.set_draft_base_url("https://relay.example/v1".into());
        ui.set_draft_api_key("sk-test-preview".into());
        ui.set_draft_model("gpt-5.2-codex".into());
        ui.set_draft_review_model("gpt-5.2".into());
        ui.set_advanced_open(true);
        ui.set_model_cache_label("已缓存 4 个模型，可刷新".into());
        ui.set_context_window_k("272".into());
        ui.set_compact_percent(80);
        ui.set_context_defaults_selected(false);
        ui.set_context_summary("272K 窗口 · 输出不限 · 压缩 80%".into());
        ui.set_context_history_ratio(0.38);
        ui.set_context_instruction_ratio(0.08);
        ui.set_context_remaining_ratio(0.54);
        ui.set_instructions(ModelRc::new(VecModel::from(vec![InstructionRow {
            name: "AGENTS.md".into(),
            detail: "约 4.2K tokens · 全局生效".into(),
            enabled: true,
        }])));
        ui.set_today_usage_summary("1.24M tokens · 24 次调用".into());
        ui.set_usage_period(1);
        ui.set_usage_day_label("2026-08-27".into());
        ui.set_usage_input("18.4M".into());
        ui.set_usage_cached("6.8M".into());
        ui.set_usage_output("2.1M".into());
        ui.set_usage_calls("326 次".into());
        ui.set_usage_input_delta("↑ 12% 较前一日".into());
        ui.set_usage_cached_delta("↑ 9% 较前一日".into());
        ui.set_usage_output_delta("↑ 8% 较前一日".into());
        ui.set_usage_calls_delta("↓ 5% 较前一日".into());
        ui.set_usage_period_total_label("近 7 天总计".into());
        ui.set_usage_period_models_label("所选范围模型分布 · 近 7 天".into());
        ui.set_usage_total_input("36M".into());
        ui.set_usage_total_cached("34.5M".into());
        ui.set_usage_total_output("187.8K".into());
        ui.set_usage_total_calls("297 次".into());
        ui.set_usage_input_path(
            usage_line_path([
                28, 56, 49, 82, 64, 112, 94, 138, 128, 166, 151, 188, 176, 224,
            ])
            .into(),
        );
        ui.set_usage_output_path(
            usage_line_path([12, 31, 19, 45, 27, 52, 36, 72, 43, 61, 34, 84, 58, 96]).into(),
        );
        ui.set_usage_trend_points(ModelRc::new(VecModel::from(vec![
            UsageTrendRow {
                date: "2026-08-14".into(),
                input: "28,000".into(),
                cached: "10,000".into(),
                output: "12,000".into(),
                calls: "4".into(),
            },
            UsageTrendRow {
                date: "2026-08-15".into(),
                input: "56,000".into(),
                cached: "22,000".into(),
                output: "31,000".into(),
                calls: "8".into(),
            },
        ])));
        ui.set_usage_models(ModelRc::new(VecModel::from(vec![UsageModelRow {
            model: "gpt-5.2-codex".into(),
            input: "12.8M".into(),
            cached: "5.4M".into(),
            output: "1.4M".into(),
            calls: "218".into(),
        }])));
        ui.set_usage_has_data(true);
        ui.set_usage_status("数据更新于 10:42 · 日常站点".into());
        ui.show().unwrap();

        for (logical_width, logical_height, scale_factor) in [
            (1_200, 760, 1_u32),
            (840, 600, 1),
            (720, 500, 1),
            (840, 600, 2),
        ] {
            ui.window().dispatch_event(WindowEvent::ScaleFactorChanged {
                scale_factor: scale_factor as f32,
            });
            let width = logical_width * scale_factor;
            let height = logical_height * scale_factor;
            window.set_size(PhysicalSize::new(width, height));
            let mut checksums = HashSet::new();
            for page in 0..3 {
                ui.set_current_page(page);
                ui.window().request_redraw();
                let mut pixels = SharedPixelBuffer::<Rgb8Pixel>::new(width, height);
                let rendered = window.draw_if_needed(|renderer| {
                    let stride = pixels.width() as usize;
                    renderer.render(pixels.make_mut_slice(), stride);
                });
                assert!(rendered, "page {page} did not render at {width}x{height}");

                let colors: HashSet<_> = pixels
                    .as_slice()
                    .iter()
                    .map(|pixel| (pixel.r, pixel.g, pixel.b))
                    .collect();
                assert!(colors.len() > 12, "page {page} rendered too few colors");
                let checksum = pixels.as_bytes().iter().fold(0_u64, |sum, byte| {
                    sum.wrapping_mul(16_777_619).wrapping_add(u64::from(*byte))
                });
                assert!(
                    checksums.insert(checksum),
                    "page {page} duplicated another frame at {width}x{height}"
                );

                if page == 1 {
                    assert_context_control_geometry(&pixels, scale_factor);
                }

                if let Ok(directory) = std::env::var("CODEX_SWITCH_SNAPSHOT_DIR") {
                    std::fs::create_dir_all(&directory).unwrap();
                    let mut ppm =
                        format!("P6\n{} {}\n255\n", pixels.width(), pixels.height()).into_bytes();
                    ppm.extend_from_slice(pixels.as_bytes());
                    std::fs::write(
                        Path::new(&directory).join(format!(
                            "page-{page}-{logical_width}x{logical_height}@{scale_factor}x.ppm"
                        )),
                        ppm,
                    )
                    .unwrap();
                }
            }
        }
    }

    #[test]
    fn usage_line_path_spans_the_chart_and_normalizes_to_its_own_peak() {
        assert_eq!(
            usage_line_path([0, 50, 100]),
            "M 0 1000 L 500 500 L 1000 0 "
        );
        assert_eq!(usage_line_path([0, 0]), "M 0 1000 L 1000 1000 ");
    }

    #[test]
    fn usage_trend_hover_index_snaps_to_the_nearest_day_and_clamps_boundaries() {
        assert_eq!(usage_trend_hover_index(0, 0.5), None);
        assert_eq!(usage_trend_hover_index(1, 0.5), Some(0));
        assert_eq!(usage_trend_hover_index(14, -0.2), Some(0));
        assert_eq!(usage_trend_hover_index(14, 0.0), Some(0));
        assert_eq!(usage_trend_hover_index(14, 0.5), Some(7));
        assert_eq!(usage_trend_hover_index(14, 1.0), Some(13));
        assert_eq!(usage_trend_hover_index(14, 1.2), Some(13));
        assert_eq!(usage_trend_hover_index(14, f32::NAN), None);
    }

    #[test]
    fn usage_trend_label_matches_the_selected_period() {
        assert!(usage_trend_label(UsagePeriod::Today).starts_with("今日"));
        assert!(usage_trend_label(UsagePeriod::Last7Days).contains("近 7 天"));
        assert!(usage_trend_label(UsagePeriod::Last30Days).contains("近 30 天"));
        assert_eq!(usage_trend_unit_label(UsagePeriod::Today), "单小时");
        assert_eq!(usage_trend_unit_label(UsagePeriod::Last7Days), "单日");
    }

    #[test]
    fn daily_usage_cards_follow_the_hovered_day_and_default_to_the_latest_day() {
        let days = vec![
            crate::usage::DailyUsage {
                date: chrono::NaiveDate::from_ymd_opt(2026, 8, 26).unwrap(),
                usage: crate::usage::TokenUsage {
                    input_tokens: 10,
                    calls: 1,
                    ..Default::default()
                },
            },
            crate::usage::DailyUsage {
                date: chrono::NaiveDate::from_ymd_opt(2026, 8, 27).unwrap(),
                usage: crate::usage::TokenUsage {
                    input_tokens: 20,
                    calls: 2,
                    ..Default::default()
                },
            },
        ];

        let (latest, latest_previous) = daily_usage_selection(&days, -1).unwrap();
        assert_eq!(
            latest.date,
            chrono::NaiveDate::from_ymd_opt(2026, 8, 27).unwrap()
        );
        assert_eq!(latest.usage.input_tokens, 20);
        assert_eq!(latest_previous.input_tokens, 10);

        let (hovered, hovered_previous) = daily_usage_selection(&days, 0).unwrap();
        assert_eq!(
            hovered.date,
            chrono::NaiveDate::from_ymd_opt(2026, 8, 26).unwrap()
        );
        assert_eq!(hovered_previous.input_tokens, 0);

        let (fallback, fallback_previous) = daily_usage_selection(&days, 99).unwrap();
        assert_eq!(fallback.date, latest.date);
        assert_eq!(fallback_previous.input_tokens, 10);

        assert_eq!(usage_period_total_label(UsagePeriod::Today), "今日总计");
        assert_eq!(
            usage_period_total_label(UsagePeriod::Last7Days),
            "近 7 天总计"
        );
        assert_eq!(
            usage_period_total_label(UsagePeriod::Last30Days),
            "近 30 天总计"
        );
        assert_eq!(
            usage_period_models_label(UsagePeriod::Last7Days),
            "所选范围模型分布 · 近 7 天"
        );
    }
}
