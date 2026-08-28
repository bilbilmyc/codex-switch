use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessKind {
    Desktop,
    CommandLine,
}

#[derive(Debug, Clone)]
pub struct RunningCodexProcess {
    pub pid: u32,
    pub kind: ProcessKind,
    pub label: String,
    pub executable: Option<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct ProcessReport {
    pub processes: Vec<RunningCodexProcess>,
}

impl ProcessReport {
    pub fn is_clear(&self) -> bool {
        self.processes.is_empty()
    }

    fn is_safe_after_desktop_shutdown(&self, known_helper_pids: &[u32]) -> bool {
        !self.has_desktop()
            && self.processes.iter().all(|process| {
                process.kind != ProcessKind::CommandLine || known_helper_pids.contains(&process.pid)
            })
    }

    pub fn has_desktop(&self) -> bool {
        self.processes
            .iter()
            .any(|process| process.kind == ProcessKind::Desktop)
    }

    pub fn has_command_line(&self) -> bool {
        self.processes
            .iter()
            .any(|process| process.kind == ProcessKind::CommandLine)
    }

    pub fn summary(&self) -> String {
        let desktop = self.has_desktop();
        let command_line = self.has_command_line();
        match (desktop, command_line) {
            (true, true) => "Codex Desktop 和命令行任务正在运行".into(),
            (true, false) => "Codex Desktop 正在运行".into(),
            (false, true) => "Codex 命令行任务正在运行".into(),
            (false, false) => "未检测到运行中的 Codex".into(),
        }
    }

    pub fn desktop_executable(&self) -> Option<PathBuf> {
        self.processes
            .iter()
            .find(|process| process.kind == ProcessKind::Desktop)
            .and_then(|process| process.executable.clone())
    }

    #[cfg(any(test, target_os = "windows"))]
    fn desktop_pids(&self) -> impl Iterator<Item = u32> + '_ {
        self.processes
            .iter()
            .filter(|process| process.kind == ProcessKind::Desktop)
            .map(|process| process.pid)
    }
}

#[derive(Debug, Error)]
pub enum ProcessError {
    #[error("could not ask Codex Desktop to quit: {0}")]
    Quit(String),
    #[error("Codex Desktop did not exit before the timeout")]
    ExitTimeout,
    #[error("a Codex command-line task started while Desktop was closing")]
    CommandLineStarted,
    #[error("could not restart Codex Desktop: {0}")]
    Relaunch(String),
}

pub fn detect_codex_processes() -> ProcessReport {
    detect_codex_processes_with_helpers().0
}

fn detect_codex_processes_with_helpers() -> (ProcessReport, Vec<u32>) {
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_exe(sysinfo::UpdateKind::Always),
    );

    let desktop_pids: HashSet<Pid> = system
        .processes()
        .iter()
        .filter_map(|(pid, process)| {
            is_desktop_process(process.name(), process.exe()).then_some(*pid)
        })
        .collect();

    let mut processes = Vec::new();
    let mut desktop_helper_pids = Vec::new();
    for pid in &desktop_pids {
        if let Some(process) = system.process(*pid) {
            processes.push(RunningCodexProcess {
                pid: pid.as_u32(),
                kind: ProcessKind::Desktop,
                label: "Codex Desktop".into(),
                executable: process.exe().map(Path::to_path_buf),
            });
        }
    }

    for (pid, process) in system.processes() {
        if desktop_pids.contains(pid) || !is_codex_binary(process.name()) {
            continue;
        }
        if descends_from(*pid, &desktop_pids, &system) {
            desktop_helper_pids.push(pid.as_u32());
            continue;
        }
        processes.push(RunningCodexProcess {
            pid: pid.as_u32(),
            kind: ProcessKind::CommandLine,
            label: "Codex CLI / app-server".into(),
            executable: process.exe().map(Path::to_path_buf),
        });
    }

    processes.sort_by_key(|process| process.pid);
    desktop_helper_pids.sort_unstable();
    (ProcessReport { processes }, desktop_helper_pids)
}

fn request_desktop_quit_for(report: &ProcessReport) -> Result<(), ProcessError> {
    if !report.has_desktop() {
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        let status = Command::new("osascript")
            .args(["-e", "tell application id \"com.openai.codex\" to quit"])
            .status()
            .map_err(|error| ProcessError::Quit(error.to_string()))?;
        if !status.success() {
            return Err(ProcessError::Quit(format!(
                "osascript exited with {status}"
            )));
        }
        return Ok(());
    }

    #[cfg(target_os = "windows")]
    {
        for pid in report.desktop_pids() {
            let script = windows_close_main_window_script(pid);
            let mut command = Command::new("powershell.exe");
            command.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
            hide_child_window(&mut command);
            let status = command
                .status()
                .map_err(|error| ProcessError::Quit(error.to_string()))?;
            if !status.success() {
                return Err(ProcessError::Quit(format!(
                    "PowerShell exited with {status} while closing PID {pid}"
                )));
            }
        }
        return Ok(());
    }

    #[allow(unreachable_code)]
    Err(ProcessError::Quit(
        "automatic Desktop shutdown is unsupported on this platform".into(),
    ))
}

pub fn quit_desktop_safely(timeout: Duration) -> Result<(), ProcessError> {
    let (report, desktop_helper_pids) = detect_codex_processes_with_helpers();
    if report.has_command_line() {
        return Err(ProcessError::CommandLineStarted);
    }
    request_desktop_quit_for(&report)?;

    let deadline = Instant::now() + timeout;
    loop {
        let report = detect_codex_processes();
        if report.is_safe_after_desktop_shutdown(&desktop_helper_pids) {
            return Ok(());
        }
        if !report.has_desktop() && report.has_command_line() {
            return Err(ProcessError::CommandLineStarted);
        }
        if Instant::now() >= deadline {
            return Err(ProcessError::ExitTimeout);
        }
        thread::sleep(Duration::from_millis(200));
    }
}

pub fn relaunch_desktop(previous_executable: Option<&Path>) -> Result<(), ProcessError> {
    #[cfg(target_os = "macos")]
    {
        let _ = previous_executable;
        let status = Command::new("open")
            .args(["-b", "com.openai.codex"])
            .status()
            .map_err(|error| ProcessError::Relaunch(error.to_string()))?;
        if !status.success() {
            return Err(ProcessError::Relaunch(format!("open exited with {status}")));
        }
        return Ok(());
    }

    #[cfg(target_os = "windows")]
    {
        let executable = previous_executable.ok_or_else(|| {
            ProcessError::Relaunch("the previous Desktop executable path is unavailable".into())
        })?;
        if !is_windows_desktop_identity(OsStr::new("ChatGPT.exe"), executable) {
            return Err(ProcessError::Relaunch(
                "the previous executable is not a trusted Codex Desktop target".into(),
            ));
        }
        Command::new(executable)
            .spawn()
            .map_err(|error| ProcessError::Relaunch(error.to_string()))?;
        return Ok(());
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let _ = previous_executable;

    #[allow(unreachable_code)]
    Err(ProcessError::Relaunch(
        "automatic Desktop relaunch is unsupported on this platform".into(),
    ))
}

fn descends_from(mut pid: Pid, ancestors: &HashSet<Pid>, system: &System) -> bool {
    let mut seen = HashSet::new();
    while seen.insert(pid) {
        let Some(parent) = system.process(pid).and_then(|process| process.parent()) else {
            return false;
        };
        if ancestors.contains(&parent) {
            return true;
        }
        pid = parent;
    }
    false
}

fn is_codex_binary(name: &OsStr) -> bool {
    name.to_string_lossy().eq_ignore_ascii_case("codex")
        || name.to_string_lossy().eq_ignore_ascii_case("codex.exe")
}

fn is_desktop_process(name: &OsStr, executable: Option<&Path>) -> bool {
    let Some(executable) = executable else {
        return false;
    };

    #[cfg(target_os = "macos")]
    return is_macos_desktop_identity(name, executable);

    #[cfg(target_os = "windows")]
    return is_windows_desktop_identity(name, executable);

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = (name, executable);
        false
    }
}

#[cfg(any(test, target_os = "macos"))]
fn is_macos_desktop_identity(name: &OsStr, executable: &Path) -> bool {
    let name = name.to_string_lossy();
    let normalized = normalized_path(executable);

    (name.eq_ignore_ascii_case("ChatGPT")
        && normalized.ends_with("/chatgpt.app/contents/macos/chatgpt"))
        || (name.eq_ignore_ascii_case("Codex")
            && normalized.ends_with("/codex.app/contents/macos/codex"))
}

#[cfg(any(test, target_os = "windows"))]
fn is_windows_desktop_identity(name: &OsStr, executable: &Path) -> bool {
    let name = name.to_string_lossy();
    if !(name.eq_ignore_ascii_case("ChatGPT") || name.eq_ignore_ascii_case("ChatGPT.exe")) {
        return false;
    }

    let normalized = normalized_path(executable);
    if !normalized.ends_with("/chatgpt.exe") {
        return false;
    }

    let components: Vec<_> = normalized.split('/').collect();
    let store_install = components
        .iter()
        .any(|component| component.starts_with("openai.chatgpt"));
    let openai_install = components
        .windows(2)
        .any(|parts| parts == ["openai", "chatgpt"]);
    let program_files_install = components
        .windows(2)
        .any(|parts| parts == ["program files", "chatgpt"]);
    store_install || openai_install || program_files_install
}

#[cfg(any(test, target_os = "macos", target_os = "windows"))]
fn normalized_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase()
}

#[cfg(any(test, target_os = "windows"))]
fn windows_close_main_window_script(pid: u32) -> String {
    format!(
        "$process = Get-Process -Id {pid} -ErrorAction SilentlyContinue; if ($null -ne $process) {{ [void]$process.CloseMainWindow() }}"
    )
}

#[cfg(any(test, target_os = "windows"))]
const fn windows_background_creation_flags() -> u32 {
    0x0800_0000
}

#[cfg(target_os = "windows")]
pub(crate) fn hide_child_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(windows_background_creation_flags());
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn hide_child_window(_command: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_binary_matching_is_exact() {
        assert!(is_codex_binary(OsStr::new("codex")));
        assert!(is_codex_binary(OsStr::new("CODEX.EXE")));
        assert!(!is_codex_binary(OsStr::new("codex-code-mode-host")));
    }

    #[test]
    fn mac_desktop_requires_matching_app_root_name_and_path() {
        assert!(is_macos_desktop_identity(
            OsStr::new("ChatGPT"),
            Path::new("/Applications/ChatGPT.app/Contents/MacOS/ChatGPT")
        ));
        assert!(is_macos_desktop_identity(
            OsStr::new("Codex"),
            Path::new("/Applications/Codex.app/Contents/MacOS/Codex")
        ));
        assert!(!is_macos_desktop_identity(
            OsStr::new("codex"),
            Path::new("/Applications/ChatGPT.app/Contents/Resources/codex")
        ));
        assert!(!is_macos_desktop_identity(
            OsStr::new("ChatGPT"),
            Path::new("/tmp/ChatGPT")
        ));
    }

    #[test]
    fn windows_desktop_requires_a_trusted_chatgpt_install_path() {
        assert!(is_windows_desktop_identity(
            OsStr::new("ChatGPT.exe"),
            Path::new(
                r"C:\Program Files\WindowsApps\OpenAI.ChatGPT-Desktop_1.0.0.0_x64__test\app\ChatGPT.exe"
            )
        ));
        assert!(is_windows_desktop_identity(
            OsStr::new("ChatGPT.exe"),
            Path::new(r"C:\Users\me\AppData\Local\Programs\OpenAI\ChatGPT\ChatGPT.exe")
        ));
        assert!(!is_windows_desktop_identity(
            OsStr::new("ChatGPT.exe"),
            Path::new(r"C:\Temp\ChatGPT.exe")
        ));
        assert!(!is_windows_desktop_identity(
            OsStr::new("Codex.exe"),
            Path::new(r"C:\Program Files\Codex\Codex.exe")
        ));
        assert!(!is_windows_desktop_identity(
            OsStr::new("ChatGPT.exe"),
            Path::new(r"C:\Program Files\Codex\codex.exe")
        ));
    }

    #[test]
    fn powershell_close_command_targets_only_the_given_pid() {
        let script = windows_close_main_window_script(4242);
        assert!(script.contains("Get-Process -Id 4242"));
        assert!(!script.contains("-Name"));
        assert!(!script.contains("Codex"));
        assert!(!script.contains("ChatGPT"));
    }

    #[test]
    fn windows_background_commands_do_not_create_a_console_window() {
        assert_eq!(windows_background_creation_flags(), 0x0800_0000);
    }

    #[test]
    fn report_exposes_only_desktop_pids_to_quit() {
        let report = ProcessReport {
            processes: vec![
                RunningCodexProcess {
                    pid: 11,
                    kind: ProcessKind::Desktop,
                    label: String::new(),
                    executable: None,
                },
                RunningCodexProcess {
                    pid: 12,
                    kind: ProcessKind::CommandLine,
                    label: String::new(),
                    executable: None,
                },
            ],
        };
        assert_eq!(report.desktop_pids().collect::<Vec<_>>(), vec![11]);
    }

    #[test]
    fn desktop_shutdown_allows_only_helpers_seen_before_quit() {
        let known_helper = ProcessReport {
            processes: vec![RunningCodexProcess {
                pid: 12,
                kind: ProcessKind::CommandLine,
                label: String::new(),
                executable: None,
            }],
        };
        let newly_started_cli = ProcessReport {
            processes: vec![RunningCodexProcess {
                pid: 13,
                kind: ProcessKind::CommandLine,
                label: String::new(),
                executable: None,
            }],
        };
        let desktop_running = ProcessReport {
            processes: vec![RunningCodexProcess {
                pid: 11,
                kind: ProcessKind::Desktop,
                label: String::new(),
                executable: None,
            }],
        };

        assert!(known_helper.is_safe_after_desktop_shutdown(&[12]));
        assert!(!newly_started_cli.is_safe_after_desktop_shutdown(&[12]));
        assert!(!desktop_running.is_safe_after_desktop_shutdown(&[12]));
    }
}
