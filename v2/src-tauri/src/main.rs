mod commands;

use std::error::Error;
use std::sync::Arc;

use codex_switch::{durable_fs, paths::AppPaths, v2::AppService};
use tauri::Manager;

struct SharedInstanceLock {
    _guard: durable_fs::ExclusiveLock,
}

fn main() {
    if let Err(error) = run() {
        let message = error.to_string();
        codex_switch::logging::record_startup_error(&message);
        show_startup_error(&message);
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    // V1 and V2 write the same Codex files, so they intentionally share V1's lifetime lock.
    let shared_paths = AppPaths::discover()?;
    let service = AppService::discover()?;

    tauri::Builder::default()
        .manage(commands::AppState {
            service: Arc::new(service),
        })
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .setup(move |app| {
            let guard = durable_fs::acquire_lock(&shared_paths.instance_lock)?;
            app.manage(SharedInstanceLock { _guard: guard });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::bootstrap,
            commands::create_profile,
            commands::new_profile,
            commands::update_profile,
            commands::duplicate_profile,
            commands::delete_profile,
            commands::import_profiles,
            commands::import_current,
            commands::export_profiles,
            commands::load_model_cache,
            commands::refresh_models,
            commands::prepare_restore,
            commands::load_context,
            commands::save_context,
            commands::refresh_usage,
            commands::export_usage,
            commands::prepare_apply,
            commands::continue_apply,
            commands::dismiss_confirmation,
        ])
        .run(tauri::generate_context!())
        .map_err(Into::into)
}

fn show_startup_error(error: &str) {
    eprintln!("Codex Switch V2 could not start; no Codex files were changed: {error}");
    rfd::MessageDialog::new()
        .set_title("Codex Switch V2")
        .set_description(format!(
            "Codex Switch V2 无法启动，未修改 Codex 配置。\n\n原因：{error}\n\n请检查配置文件和磁盘权限；若已有 Codex Switch V1 或 V2 正在运行，请先关闭它。"
        ))
        .set_level(rfd::MessageLevel::Error)
        .show();
}
