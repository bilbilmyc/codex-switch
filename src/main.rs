#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

#[cfg(target_os = "windows")]
const SOFTWARE_RENDERER_ENV: &str = "CODEX_SWITCH_SOFTWARE_RENDERER";
#[cfg(any(target_os = "windows", test))]
const SOFTWARE_RENDERER_NAME: &str = "software";
#[cfg(any(target_os = "windows", test))]
const SOFTWARE_RELAUNCH_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

#[cfg(any(target_os = "windows", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WindowsRenderMode {
    HardwarePreferred,
    SoftwareFallback,
}

#[cfg(any(target_os = "windows", test))]
fn windows_render_mode(fallback_marker: Option<&std::ffi::OsStr>) -> WindowsRenderMode {
    if fallback_marker == Some(std::ffi::OsStr::new("1")) {
        WindowsRenderMode::SoftwareFallback
    } else {
        WindowsRenderMode::HardwarePreferred
    }
}

#[cfg(any(target_os = "windows", test))]
fn windows_renderer_name(mode: WindowsRenderMode) -> Option<&'static str> {
    match mode {
        WindowsRenderMode::HardwarePreferred => None,
        WindowsRenderMode::SoftwareFallback => Some(SOFTWARE_RENDERER_NAME),
    }
}

#[cfg(any(target_os = "windows", test))]
fn should_retry_with_software(mode: WindowsRenderMode, error: &str) -> bool {
    if mode == WindowsRenderMode::SoftwareFallback {
        return false;
    }

    let error = error.to_ascii_lowercase();
    [
        "failed to initialize opengl",
        "error creating opengl",
        "cannot create opengl",
        "opengl context",
        "opengl display",
        "opengl window surface",
        "femtovg renderer",
        "failed to find a suitable renderer",
    ]
    .iter()
    .any(|marker| error.contains(marker))
}

#[cfg(test)]
fn windows_resource_id(id: u16) -> *const u16 {
    std::ptr::without_provenance(usize::from(id))
}

#[cfg(any(target_os = "windows", test))]
fn instance_lock_timeout(mode: WindowsRenderMode) -> std::time::Duration {
    match mode {
        WindowsRenderMode::HardwarePreferred => std::time::Duration::ZERO,
        WindowsRenderMode::SoftwareFallback => SOFTWARE_RELAUNCH_LOCK_WAIT,
    }
}

fn main() {
    let result: Result<(), Box<dyn std::error::Error>> = configure_platform().map_err(Into::into);
    if let Err(error) = result.and_then(|()| {
        #[cfg(target_os = "windows")]
        {
            let mode = windows_render_mode(std::env::var_os(SOFTWARE_RENDERER_ENV).as_deref());
            codex_switch::app::run_with_instance_lock_timeout(instance_lock_timeout(mode))
        }
        #[cfg(not(target_os = "windows"))]
        {
            codex_switch::app::run()
        }
    }) {
        let error_message = error.to_string();
        #[cfg(target_os = "windows")]
        let error_message = {
            let mut error_message = error_message;
            let mode = windows_render_mode(std::env::var_os(SOFTWARE_RENDERER_ENV).as_deref());
            if should_retry_with_software(mode, &error_message) {
                match relaunch_with_software() {
                    Ok(()) => return,
                    Err(relaunch_error) => {
                        error_message.push_str(&format!(
                            "\nSoftware-renderer fallback could not be started: {relaunch_error}"
                        ));
                    }
                }
            }
            error_message
        };
        codex_switch::logging::record_startup_error(&error_message);
        show_startup_error(&error_message);
        std::process::exit(1);
    }
}

#[cfg(target_os = "windows")]
fn configure_platform() -> Result<(), slint::PlatformError> {
    let mode = windows_render_mode(std::env::var_os(SOFTWARE_RENDERER_ENV).as_deref());
    let selector = slint::BackendSelector::new().backend_name("winit".to_owned());
    match windows_renderer_name(mode) {
        Some(renderer) => selector.renderer_name(renderer.to_owned()).select(),
        None => selector.select(),
    }
}

#[cfg(not(target_os = "windows"))]
fn configure_platform() -> Result<(), slint::PlatformError> {
    Ok(())
}

#[cfg(target_os = "windows")]
fn relaunch_with_software() -> std::io::Result<()> {
    let executable = std::env::current_exe()?;
    std::process::Command::new(executable)
        .args(std::env::args_os().skip(1))
        .env(SOFTWARE_RENDERER_ENV, "1")
        .spawn()?;
    Ok(())
}

fn show_startup_error(error: &str) {
    eprintln!("Codex Switch could not start; no Codex files were changed: {error}");
    rfd::MessageDialog::new()
        .set_title("Codex Switch")
        .set_description(format!(
            "Codex Switch 无法启动，未修改 Codex 配置。\n\n原因：{error}\n\n请根据原因检查图形驱动、配置文件或是否已有另一个实例在运行。"
        ))
        .set_level(rfd::MessageLevel::Error)
        .show();
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::{
        SOFTWARE_RELAUNCH_LOCK_WAIT, SOFTWARE_RENDERER_NAME, WindowsRenderMode,
        instance_lock_timeout, should_retry_with_software, windows_render_mode,
        windows_renderer_name, windows_resource_id,
    };

    #[test]
    fn windows_prefers_hardware_and_uses_software_only_for_the_fallback_process() {
        let preferred = windows_render_mode(None);
        let fallback = windows_render_mode(Some(OsStr::new("1")));

        assert_eq!(preferred, WindowsRenderMode::HardwarePreferred);
        assert_eq!(windows_renderer_name(preferred), None);
        assert_eq!(fallback, WindowsRenderMode::SoftwareFallback);
        assert_eq!(
            windows_renderer_name(fallback),
            Some(SOFTWARE_RENDERER_NAME)
        );
    }

    #[test]
    fn windows_retries_graphics_initialization_failures_once() {
        let driver_error =
            "Failed to initialize OpenGL driver: Could not locate glCreateShader symbol";

        assert!(should_retry_with_software(
            WindowsRenderMode::HardwarePreferred,
            driver_error,
        ));
        assert!(!should_retry_with_software(
            WindowsRenderMode::SoftwareFallback,
            driver_error,
        ));
        assert!(!should_retry_with_software(
            WindowsRenderMode::HardwarePreferred,
            "profile file contains invalid TOML",
        ));
    }

    #[test]
    fn software_fallback_waits_for_the_hardware_process_lock_handoff() {
        assert_eq!(
            instance_lock_timeout(WindowsRenderMode::HardwarePreferred),
            std::time::Duration::ZERO
        );
        assert_eq!(
            instance_lock_timeout(WindowsRenderMode::SoftwareFallback),
            SOFTWARE_RELAUNCH_LOCK_WAIT
        );
    }

    #[test]
    fn windows_icon_contains_multiple_shell_sizes() {
        let icon = include_bytes!("../assets/app-icon.ico");
        let image_count = u16::from_le_bytes([icon[4], icon[5]]);

        assert!(image_count >= 7, "ICO contains only {image_count} image(s)");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_executable_contains_the_application_icon_resource() {
        use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows_sys::Win32::UI::WindowsAndMessaging::LoadIconW;

        let module = unsafe { GetModuleHandleW(std::ptr::null()) };
        assert!(!module.is_null());
        let icon = unsafe { LoadIconW(module, windows_resource_id(1)) };
        assert!(
            !icon.is_null(),
            "Windows executable has no icon resource #1"
        );
    }

    #[test]
    fn windows_icon_resource_id_remains_one() {
        assert_eq!(windows_resource_id(1).addr(), 1);
    }
}
