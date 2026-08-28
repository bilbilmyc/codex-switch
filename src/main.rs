#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

#[cfg(any(target_os = "windows", test))]
const WINDOWS_RENDERER_NAME: &str = "renderer-software";

fn main() {
    let result: Result<(), Box<dyn std::error::Error>> = configure_platform().map_err(Into::into);
    if let Err(error) = result.and_then(|()| codex_switch::app::run()) {
        eprintln!("Codex Switch could not start; no Codex files were changed: {error:#}");
        rfd::MessageDialog::new()
            .set_title("Codex Switch")
            .set_description(format!(
                "Codex Switch 无法启动，未修改 Codex 配置。\n\n原因：{error}\n\n请根据原因检查图形驱动、配置文件或是否已有另一个实例在运行。"
            ))
            .set_level(rfd::MessageLevel::Error)
            .show();
        std::process::exit(1);
    }
}

#[cfg(target_os = "windows")]
fn configure_platform() -> Result<(), slint::PlatformError> {
    // Avoid the OpenGL-only FemtoVG default on older drivers and remote sessions.
    slint::BackendSelector::new()
        .backend_name("winit".to_owned())
        .renderer_name(WINDOWS_RENDERER_NAME.to_owned())
        .select()
}

#[cfg(not(target_os = "windows"))]
fn configure_platform() -> Result<(), slint::PlatformError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::WINDOWS_RENDERER_NAME;

    #[test]
    fn windows_renderer_uses_the_software_backend() {
        assert_eq!(WINDOWS_RENDERER_NAME, "renderer-software");
    }
}
