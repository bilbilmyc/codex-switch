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
    #[error("Codex processes did not exit before the timeout")]
    ExitTimeout,
    #[error("could not restart Codex Desktop: {0}")]
    Relaunch(String),
}

pub fn detect_codex_processes() -> ProcessReport {
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
        if desktop_pids.contains(pid)
            || !is_codex_binary(process.name())
            || descends_from(*pid, &desktop_pids, &system)
        {
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
    ProcessReport { processes }
}

pub fn request_desktop_quit() -> Result<(), ProcessError> {
    let report = detect_codex_processes();
    request_desktop_quit_for(&report)
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
            let status = Command::new("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-Command", &script])
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

pub fn wait_until_clear(timeout: Duration) -> Result<(), ProcessError> {
    let deadline = Instant::now() + timeout;
    loop {
        if detect_codex_processes().is_clear() {
            return Ok(());
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
    normalized == "chatgpt.exe" || normalized.ends_with("/chatgpt.exe")
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
    fn windows_desktop_accepts_chatgpt_but_never_codex_exe() {
        assert!(is_windows_desktop_identity(
            OsStr::new("ChatGPT.exe"),
            Path::new(r"C:\Program Files\ChatGPT\ChatGPT.exe")
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
}
