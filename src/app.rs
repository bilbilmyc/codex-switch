use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::codex_config::{self, CodexConfigError};
use crate::codex_validator::CodexStagedValidator;
use crate::domain::{Activation, ApiKey, Profile, ProfileId};
use crate::durable_fs;
use crate::models;
use crate::paths::AppPaths;
use crate::process;
use crate::profiles::{ProfileStore, ProfilesDocument};
use crate::transaction::{ConflictPolicy, ManagedState, TransactionError, TransactionManager};

slint::include_modules!();

type SharedController = Arc<Mutex<Controller>>;

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
    RestoreProcess {
        desktop_executable: Option<PathBuf>,
        desktop_only: bool,
    },
    Close,
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
    _instance_lock: durable_fs::ExclusiveLock,
}

enum ApplyWorkerResult {
    Applied {
        state: ManagedState,
        relaunch_error: Option<String>,
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
        _instance_lock: instance_lock,
    }));

    let ui = AppWindow::new()?;
    {
        let controller = controller.lock().expect("controller mutex poisoned");
        controller.sync_all(&ui);
    }
    install_callbacks(&ui, controller);
    ui.run()?;
    Ok(())
}

fn install_callbacks(ui: &AppWindow, controller: SharedController) {
    macro_rules! bind {
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
            let mut controller = controller.lock().expect("controller mutex poisoned");
            if ui.get_draft_dirty() {
                controller.dialog = DialogAction::ChangeSelection(target);
                open_dialog(
                    &ui,
                    "切换中转站",
                    "当前中转站有未保存的修改。",
                    "保存并切换",
                    "放弃并切换",
                    "取消",
                    false,
                );
            } else {
                controller.select_index(&ui, target);
            }
        });
    }

    bind!(on_new_profile, |ui, state| {
        state.new_profile(&ui);
    });
    bind!(on_duplicate_profile, |ui, state| {
        state.duplicate_profile(&ui);
    });
    bind!(on_delete_profile, |ui, state| {
        state.confirm_delete(&ui);
    });
    bind!(on_import_profiles, |ui, state| {
        state.import_profiles(&ui);
    });
    bind!(on_import_current, |ui, state| {
        state.import_current(&ui);
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
        state.reload_selected(&ui);
        set_status(&ui, "已放弃未保存的修改", 0);
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
            if ui.get_draft_dirty() {
                let mut controller = controller.lock().expect("controller mutex poisoned");
                controller.dialog = DialogAction::Close;
                open_dialog(
                    &ui,
                    "关闭 Codex Switch",
                    "当前中转站有未保存的修改。",
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
        ui.set_can_restore(self.transaction.has_backup().unwrap_or(false));
        ui.set_status_text(self.startup_status.clone().into());
        ui.set_status_tone(self.startup_tone);
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

    fn select_index(&mut self, ui: &AppWindow, index: usize) {
        self.selected = self.profiles.profiles.get(index).map(|profile| profile.id);
        self.sync_profiles(ui);
        self.reload_selected(ui);
    }

    fn reload_selected(&self, ui: &AppWindow) {
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

    fn new_profile(&mut self, ui: &AppWindow) {
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
        self.selected = Some(id);
        self.sync_profiles(ui);
        self.reload_selected(ui);
        set_status(ui, "已新建中转站，请填写连接信息", 0);
    }

    fn duplicate_profile(&mut self, ui: &AppWindow) {
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
        self.selected = Some(id);
        self.sync_profiles(ui);
        self.reload_selected(ui);
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

    fn delete_profile(&mut self, ui: &AppWindow, id: ProfileId) {
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
        self.selected = self
            .profiles
            .profiles
            .get(index.min(self.profiles.profiles.len().saturating_sub(1)))
            .map(|profile| profile.id);
        self.sync_profiles(ui);
        self.reload_selected(ui);
        ui.set_editor_open(false);
        set_status(ui, "中转站已删除，当前 Codex 配置未改动", 1);
    }

    fn save_draft(&mut self, ui: &AppWindow) -> bool {
        let Some(id) = self.selected else {
            return false;
        };
        let profile = match profile_from_ui(ui, id) {
            Ok(profile) => profile,
            Err(error) => {
                set_status(ui, format!("中转站信息不完整：{error}"), 3);
                return false;
            }
        };
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
        ui.set_draft_dirty(false);
        set_status(ui, "中转站已保存", 1);
        true
    }

    fn save_before_navigation(&mut self, ui: &AppWindow) -> bool {
        !ui.get_draft_dirty() || self.save_draft(ui)
    }

    fn begin_apply(&mut self, ui: &AppWindow, shared: SharedController) {
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
        let profile = match profile_from_ui(ui, id) {
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
            let result = models::fetch_models(&profile)
                .and_then(|cache| {
                    models::save_cache(&cache_dir, &cache)?;
                    Ok(cache)
                })
                .map_err(|error| error.to_string());
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.set_busy(false);
                let controller = shared.lock().expect("controller mutex poisoned");
                match result {
                    Ok(cache) if controller.selected == Some(cache.profile_id) => {
                        set_model_list(&ui, &cache.models, &ui.get_draft_model());
                        ui.set_model_cache_label(
                            format!("刚刚获取了 {} 个模型", cache.models.len()).into(),
                        );
                        set_status(&ui, "模型列表已更新", 1);
                    }
                    Ok(_) => {
                        set_status(&ui, "模型列表已缓存到对应中转站", 1);
                    }
                    Err(error) => {
                        set_status(&ui, format!("模型获取失败：{error}"), 3);
                    }
                }
                controller.sync_profiles(&ui);
            });
        });
    }

    fn import_current(&mut self, ui: &AppWindow) {
        if !self.save_before_navigation(ui) {
            return;
        }
        self.import_current_into(ui, None);
    }

    fn import_current_into(&mut self, ui: &AppWindow, target: Option<ProfileId>) {
        match import_live_profile(&self.paths) {
            Ok(mut imported) => {
                let mut updated = self.profiles.clone();
                let imported_id = if let Some(target_id) = target {
                    updated.remove(target_id);
                    imported.id = target_id;
                    imported.name = unique_name(&updated, &imported.name);
                    let id = imported.id;
                    if let Err(error) = updated.insert(imported) {
                        set_status(ui, format!("无法导入当前配置：{error}"), 3);
                        return;
                    }
                    id
                } else {
                    imported.name = unique_name(&updated, &imported.name);
                    let id = imported.id;
                    if let Err(error) = updated.insert(imported) {
                        set_status(ui, format!("无法导入当前配置：{error}"), 3);
                        return;
                    }
                    id
                };
                if let Err(error) = self.store.save(&updated) {
                    set_status(ui, format!("无法保存导入的中转站：{error}"), 3);
                    return;
                }
                self.selected = Some(imported_id);
                self.active = Some(imported_id);
                self.profiles = updated;
                self.sync_profiles(ui);
                self.reload_selected(ui);
                match self.transaction.adopt_current(Some(imported_id)) {
                    Ok(_) => set_status(ui, "已导入当前 Codex 配置", 1),
                    Err(error) => set_status(
                        ui,
                        format!("中转站已导入，但无法建立冲突检测基线：{error}"),
                        2,
                    ),
                }
            }
            Err(error) => {
                set_status(ui, format!("当前配置无法导入：{error}"), 3);
            }
        }
    }

    fn import_profiles(&mut self, ui: &AppWindow) {
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
        self.selected = last_id.or(self.selected);
        self.sync_profiles(ui);
        self.reload_selected(ui);
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
                .delete_profile(ui, id);
        }
        DialogAction::ChangeSelection(index) => {
            let mut controller = shared.lock().expect("controller mutex poisoned");
            if controller.save_draft(ui) {
                controller.select_index(ui, index);
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
            activation,
            desktop_executable,
        } => {
            shared
                .lock()
                .expect("controller mutex poisoned")
                .import_current_into(ui, Some(activation.profile_id));
            relaunch_if_needed(ui, desktop_executable);
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
        DialogAction::Close => {
            let saved = shared
                .lock()
                .expect("controller mutex poisoned")
                .save_draft(ui);
            if saved {
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
                .select_index(ui, index);
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
    if let DialogAction::ApplyConflict {
        desktop_executable, ..
    } = action
    {
        relaunch_if_needed(ui, desktop_executable);
    }
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
            process::request_desktop_quit()
                .and_then(|()| process::wait_until_clear(Duration::from_secs(8)))
                .map_err(|error| error.to_string())
        } else {
            Ok(())
        };

        let worker_result = match process_result {
            Err(error) => {
                if desktop_executable.is_some() {
                    let _ = process::relaunch_desktop(desktop_executable.as_deref());
                }
                ApplyWorkerResult::Failed {
                    message: error,
                    recovery_required: false,
                }
            }
            Ok(()) => {
                let manager = TransactionManager::new(paths);
                let validator = CodexStagedValidator::discover();
                let staged_validator = validator
                    .as_ref()
                    .map(|validator| validator as &dyn crate::transaction::StagedValidator);
                match manager.apply_validated(&activation, policy, staged_validator) {
                    Ok(outcome) => {
                        let relaunch_error = desktop_executable
                            .as_deref()
                            .and_then(|path| process::relaunch_desktop(Some(path)).err())
                            .map(|error| error.to_string());
                        ApplyWorkerResult::Applied {
                            state: outcome.state,
                            relaunch_error,
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
                        if desktop_executable.is_some() {
                            let _ = process::relaunch_desktop(desktop_executable.as_deref());
                        }
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
                } => {
                    controller.active = state.active_profile_id;
                    controller.sync_profiles(&ui);
                    ui.set_can_restore(true);
                    match relaunch_error {
                        Some(error) => set_status(
                            &ui,
                            format!("切换完成，但 Codex Desktop 未能重新打开：{error}"),
                            2,
                        ),
                        None => set_status(&ui, "切换完成", 1),
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
            process::request_desktop_quit()
                .and_then(|()| process::wait_until_clear(Duration::from_secs(8)))
                .map_err(|error| error.to_string())
        } else {
            Ok(())
        };
        let worker_result = match process_result {
            Err(error) => {
                if desktop_executable.is_some() {
                    let _ = process::relaunch_desktop(desktop_executable.as_deref());
                }
                RestoreWorkerResult::Failed {
                    message: error,
                    recovery_required: false,
                }
            }
            Ok(()) => match TransactionManager::new(paths).restore_latest() {
                Ok(_) => {
                    let relaunch_error = desktop_executable
                        .as_deref()
                        .and_then(|path| process::relaunch_desktop(Some(path)).err())
                        .map(|error| error.to_string());
                    RestoreWorkerResult::Restored { relaunch_error }
                }
                Err(error) => {
                    let recovery_required =
                        matches!(&error, TransactionError::RollbackFailed { .. });
                    if desktop_executable.is_some() {
                        let _ = process::relaunch_desktop(desktop_executable.as_deref());
                    }
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

fn profile_from_ui(ui: &AppWindow, id: ProfileId) -> Result<Profile, String> {
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
    let imported = import_live_profile(paths).ok()?;
    if let Ok(Some(state)) = transaction.load_state()
        && current_fingerprint(paths).as_deref() == Some(state.relevant_fingerprint.as_str())
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

fn current_fingerprint(paths: &AppPaths) -> Option<String> {
    let config_bytes = durable_fs::read_optional(&paths.codex_config).ok()??;
    let config = std::str::from_utf8(&config_bytes).ok()?;
    let auth = durable_fs::read_optional(&paths.codex_auth).ok()?;
    codex_config::relevant_fingerprint(config, auth.as_deref()).ok()
}

fn same_connection(left: &Profile, right: &Profile) -> bool {
    left.name == right.name
        && left.base_url == right.base_url
        && left.api_key == right.api_key
        && left.model == right.model
        && left.review_model == right.review_model
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

fn relaunch_if_needed(ui: &AppWindow, executable: Option<PathBuf>) {
    if let Some(executable) = executable
        && let Err(error) = process::relaunch_desktop(Some(&executable))
    {
        set_status(ui, format!("Codex Desktop 未能重新打开：{error}"), 2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn edited_profile_is_not_reported_as_the_live_configuration() {
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

        let mut document = ProfilesDocument::default();
        document.insert(profile).unwrap();
        assert!(recognize_active_profile(&paths, &manager, &document).is_some());

        document.profiles[0].model = "model-after".to_owned();
        assert_eq!(recognize_active_profile(&paths, &manager, &document), None);
    }
}
