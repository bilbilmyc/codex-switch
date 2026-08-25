#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

fn main() {
    if codex_switch::app::run().is_err() {
        eprintln!("Codex Switch could not start; no Codex files were changed.");
        rfd::MessageDialog::new()
            .set_title("Codex Switch")
            .set_description(
                "Codex Switch 无法启动，未修改 Codex 配置。\n\n请检查 ~/.codex-switch 的文件格式、权限，或确认没有另一个实例正在运行。",
            )
            .set_level(rfd::MessageLevel::Error)
            .show();
        std::process::exit(1);
    }
}
