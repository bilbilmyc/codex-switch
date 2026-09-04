use std::sync::Arc;

use codex_switch::v2::{
    AppService, ApplyResponse, BackupCenterView, BackupPreviewView, Bootstrap, ContextDraft,
    ContextView, DeepValidationView, ModelListView, ProfileDraft, ProfileSummary, UsageView,
};
use tauri::State;

pub struct AppState {
    pub service: Arc<AppService>,
}

#[tauri::command]
pub async fn bootstrap(state: State<'_, AppState>) -> Result<Bootstrap, String> {
    let service = state.service.clone();
    tauri::async_runtime::spawn_blocking(move || service.bootstrap())
        .await
        .map_err(|_| "初始化任务已中断".to_owned())?
        .map_err(|error| error.to_string())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn create_profile(
    draft: ProfileDraft,
    state: State<'_, AppState>,
) -> Result<ProfileSummary, String> {
    let service = state.service.clone();
    tauri::async_runtime::spawn_blocking(move || service.create_profile(draft))
        .await
        .map_err(|_| "保存任务已中断".to_owned())?
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn new_profile(state: State<'_, AppState>) -> Result<ProfileSummary, String> {
    let service = state.service.clone();
    tauri::async_runtime::spawn_blocking(move || service.new_profile())
        .await
        .map_err(|_| "新建任务已中断".to_owned())?
        .map_err(|error| error.to_string())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn update_profile(
    profile_id: String,
    draft: ProfileDraft,
    state: State<'_, AppState>,
) -> Result<ProfileSummary, String> {
    let service = state.service.clone();
    tauri::async_runtime::spawn_blocking(move || service.update_profile(profile_id, draft))
        .await
        .map_err(|_| "保存任务已中断".to_owned())?
        .map_err(|error| error.to_string())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn duplicate_profile(
    profile_id: String,
    state: State<'_, AppState>,
) -> Result<ProfileSummary, String> {
    let service = state.service.clone();
    tauri::async_runtime::spawn_blocking(move || service.duplicate_profile(profile_id))
        .await
        .map_err(|_| "复制任务已中断".to_owned())?
        .map_err(|error| error.to_string())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn delete_profile(profile_id: String, state: State<'_, AppState>) -> Result<(), String> {
    let service = state.service.clone();
    tauri::async_runtime::spawn_blocking(move || service.delete_profile(profile_id))
        .await
        .map_err(|_| "删除任务已中断".to_owned())?
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn import_profiles(state: State<'_, AppState>) -> Result<Bootstrap, String> {
    let service = state.service.clone();
    tauri::async_runtime::spawn_blocking(move || service.import_profiles_interactive())
        .await
        .map_err(|_| "导入任务已中断".to_owned())?
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn import_current(state: State<'_, AppState>) -> Result<ProfileSummary, String> {
    let service = state.service.clone();
    tauri::async_runtime::spawn_blocking(move || service.import_current())
        .await
        .map_err(|_| "导入任务已中断".to_owned())?
        .map_err(|error| error.to_string())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn export_profiles(include_keys: bool, state: State<'_, AppState>) -> Result<(), String> {
    let service = state.service.clone();
    tauri::async_runtime::spawn_blocking(move || service.export_profiles_interactive(include_keys))
        .await
        .map_err(|_| "导出任务已中断".to_owned())?
        .map_err(|error| error.to_string())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn load_model_cache(
    profile_id: String,
    state: State<'_, AppState>,
) -> Result<ModelListView, String> {
    let service = state.service.clone();
    tauri::async_runtime::spawn_blocking(move || service.load_model_cache(profile_id))
        .await
        .map_err(|_| "模型缓存读取任务已中断".to_owned())?
        .map_err(|error| error.to_string())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn refresh_models(
    profile_id: String,
    draft: ProfileDraft,
    state: State<'_, AppState>,
) -> Result<ModelListView, String> {
    let service = state.service.clone();
    tauri::async_runtime::spawn_blocking(move || service.refresh_models(profile_id, draft))
        .await
        .map_err(|_| "模型刷新任务已中断".to_owned())?
        .map_err(|error| error.to_string())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn deep_validate_profile(
    profile_id: String,
    state: State<'_, AppState>,
) -> Result<DeepValidationView, String> {
    let service = state.service.clone();
    tauri::async_runtime::spawn_blocking(move || service.deep_validate_profile(profile_id))
        .await
        .map_err(|_| "深度验证任务已中断".to_owned())?
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn load_backup_center(state: State<'_, AppState>) -> Result<BackupCenterView, String> {
    let service = state.service.clone();
    tauri::async_runtime::spawn_blocking(move || service.load_backup_center())
        .await
        .map_err(|_| "备份列表读取任务已中断".to_owned())?
        .map_err(|error| error.to_string())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn load_backup_preview(
    backup_id: String,
    state: State<'_, AppState>,
) -> Result<BackupPreviewView, String> {
    let service = state.service.clone();
    tauri::async_runtime::spawn_blocking(move || service.load_backup_preview(backup_id))
        .await
        .map_err(|_| "备份预览读取任务已中断".to_owned())?
        .map_err(|error| error.to_string())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn prepare_backup_restore(
    backup_id: String,
    live_revision: String,
    state: State<'_, AppState>,
) -> Result<ApplyResponse, String> {
    let service = state.service.clone();
    tauri::async_runtime::spawn_blocking(move || {
        service.prepare_backup_restore(backup_id, live_revision)
    })
    .await
    .map_err(|_| "恢复任务已中断".to_owned())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn prepare_restore(state: State<'_, AppState>) -> Result<ApplyResponse, String> {
    let service = state.service.clone();
    tauri::async_runtime::spawn_blocking(move || service.prepare_restore())
        .await
        .map_err(|_| "恢复任务已中断".to_owned())?
        .map_err(|error| error.to_string())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn load_context(
    profile_id: String,
    state: State<'_, AppState>,
) -> Result<ContextView, String> {
    let service = state.service.clone();
    tauri::async_runtime::spawn_blocking(move || service.load_context(profile_id))
        .await
        .map_err(|_| "上下文读取任务已中断".to_owned())?
        .map_err(|error| error.to_string())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn save_context(
    profile_id: String,
    draft: ContextDraft,
    state: State<'_, AppState>,
) -> Result<ApplyResponse, String> {
    let service = state.service.clone();
    tauri::async_runtime::spawn_blocking(move || service.save_context(profile_id, draft))
        .await
        .map_err(|_| "上下文保存任务已中断".to_owned())?
        .map_err(|error| error.to_string())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn refresh_usage(
    profile_id: String,
    period: String,
    state: State<'_, AppState>,
) -> Result<UsageView, String> {
    let service = state.service.clone();
    tauri::async_runtime::spawn_blocking(move || service.refresh_usage(profile_id, period))
        .await
        .map_err(|_| "用量读取任务已中断".to_owned())?
        .map_err(|error| error.to_string())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn export_usage(
    profile_id: String,
    period: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let service = state.service.clone();
    tauri::async_runtime::spawn_blocking(move || service.export_usage(profile_id, period))
        .await
        .map_err(|_| "用量导出任务已中断".to_owned())?
        .map_err(|error| error.to_string())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn prepare_apply(
    profile_id: String,
    state: State<'_, AppState>,
) -> Result<ApplyResponse, String> {
    let service = state.service.clone();
    tauri::async_runtime::spawn_blocking(move || service.prepare_apply(profile_id))
        .await
        .map_err(|_| "切换任务已中断".to_owned())?
        .map_err(|error| error.to_string())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn continue_apply(
    token: String,
    choice: String,
    state: State<'_, AppState>,
) -> Result<ApplyResponse, String> {
    let service = state.service.clone();
    tauri::async_runtime::spawn_blocking(move || service.continue_apply(token, choice))
        .await
        .map_err(|_| "切换任务已中断".to_owned())?
        .map_err(|error| error.to_string())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn dismiss_confirmation(token: String, state: State<'_, AppState>) -> Result<(), String> {
    let service = state.service.clone();
    tauri::async_runtime::spawn_blocking(move || service.dismiss_confirmation(token))
        .await
        .map_err(|_| "确认取消任务已中断".to_owned())?
        .map_err(|error| error.to_string())
}
