use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;

use crate::process::hide_child_window;

pub use crate::transaction::StagedValidator;

const CODEX_BINARY_OVERRIDE: &str = "CODEX_SWITCH_CODEX_BINARY";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2);
const VALIDATION_REJECTED: &str = "Codex 拒绝了候选配置；未写入任何文件";
const VALIDATION_SETUP_FAILED: &str = "无法准备隔离的 Codex 配置校验";
const VALIDATION_LAUNCH_FAILED: &str = "无法启动隔离的 Codex 配置校验器";
const VALIDATION_PROCESS_FAILED: &str = "Codex 配置校验器异常退出；未写入任何文件";
const VALIDATION_REFERENCE_MISSING: &str = "候选配置引用的本地文件不存在；未写入任何文件";
const VALIDATION_REFERENCE_INVALID: &str = "候选配置引用的本地文件不安全或无法读取；未写入任何文件";
const MAX_STAGED_REFERENCE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValidationAvailability {
    Validated,
    SkippedNoCodexBinary,
}

#[derive(Clone, Debug)]
pub struct CodexStagedValidator {
    binary: PathBuf,
    source_codex_home: Option<PathBuf>,
    timeout: Duration,
}

impl CodexStagedValidator {
    pub fn discover() -> Option<Self> {
        Self::discover_for_desktop(None)
    }

    pub fn discover_for_desktop(desktop_executable: Option<&Path>) -> Option<Self> {
        let override_value = env::var_os(CODEX_BINARY_OVERRIDE);
        let path_value = env::var_os("PATH");
        let source_codex_home = crate::paths::AppPaths::discover()
            .ok()
            .map(|paths| paths.codex_dir);
        discover_with_desktop(
            override_value.as_deref(),
            path_value.as_deref(),
            known_binaries(),
            desktop_executable,
        )
        .map(|binary| Self {
            binary,
            source_codex_home,
            timeout: DEFAULT_TIMEOUT,
        })
    }

    pub fn from_binary(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            source_codex_home: None,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    pub fn binary(&self) -> &Path {
        &self.binary
    }

    fn validate_candidate(&self, config_toml: &str, auth_json: &[u8]) -> Result<(), String> {
        let staged = StagedCodexHome::create(
            config_toml.as_bytes(),
            auth_json,
            self.source_codex_home.as_deref(),
        )
        .map_err(|error| validation_setup_error(&error))?;
        let run = staged
            .run(&self.binary, self.timeout)
            .map_err(|error| format!("{VALIDATION_LAUNCH_FAILED}: {error}"))?;
        classify_validation_run(run.exit_success, &run.diagnostic)
    }
}

fn validation_setup_error(error: &io::Error) -> String {
    match error.kind() {
        io::ErrorKind::NotFound => VALIDATION_REFERENCE_MISSING.to_owned(),
        io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => {
            VALIDATION_REFERENCE_INVALID.to_owned()
        }
        _ => VALIDATION_SETUP_FAILED.to_owned(),
    }
}

impl StagedValidator for CodexStagedValidator {
    fn validate(&self, config_toml: &str, auth_json: &[u8]) -> Result<(), String> {
        self.validate_candidate(config_toml, auth_json)
    }
}

pub fn validate_if_available(
    config_toml: &str,
    auth_json: &[u8],
) -> Result<ValidationAvailability, String> {
    let Some(validator) = CodexStagedValidator::discover() else {
        return Ok(ValidationAvailability::SkippedNoCodexBinary);
    };
    validator.validate(config_toml, auth_json)?;
    Ok(ValidationAvailability::Validated)
}

struct StagedCodexHome {
    _root: TempDir,
    codex_home: PathBuf,
    sqlite_home: PathBuf,
    working_directory: PathBuf,
    diagnostic_log: PathBuf,
}

struct ValidationRun {
    diagnostic: String,
    /// `None` means the validator stayed alive until the timeout and was then stopped.
    exit_success: Option<bool>,
}

impl StagedCodexHome {
    fn create(
        config_toml: &[u8],
        auth_json: &[u8],
        source_codex_home: Option<&Path>,
    ) -> io::Result<Self> {
        let root = tempfile::Builder::new()
            .prefix("codex-switch-validate-")
            .tempdir()?;
        let codex_home = root.path().join("codex-home");
        let sqlite_home = root.path().join("sqlite-home");
        let working_directory = root.path().join("workdir");
        let diagnostic_log = root.path().join("diagnostic.log");

        create_private_dir(&codex_home)?;
        create_private_dir(&sqlite_home)?;
        create_private_dir(&working_directory)?;
        write_private(&codex_home.join("config.toml"), config_toml)?;
        write_private(&codex_home.join("auth.json"), auth_json)?;
        stage_relative_model_catalog(config_toml, source_codex_home, &codex_home)?;

        Ok(Self {
            _root: root,
            codex_home,
            sqlite_home,
            working_directory,
            diagnostic_log,
        })
    }

    fn run(&self, binary: &Path, timeout: Duration) -> io::Result<ValidationRun> {
        let diagnostic_file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&self.diagnostic_log)?;
        set_private_file_permissions(&diagnostic_file)?;

        let mut command = Command::new(binary);
        command
            .args(["app-server", "--strict-config", "--listen", "stdio://"])
            .current_dir(&self.working_directory)
            .env("CODEX_HOME", &self.codex_home)
            .env("CODEX_SQLITE_HOME", &self.sqlite_home)
            .env_remove("CODEX_MANAGED_CONFIG_PATH")
            .env_remove("CODEX_SYSTEM_CONFIG_PATH")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(diagnostic_file.try_clone()?));
        hide_child_window(&mut command);

        let mut child = command.spawn()?;
        let deadline = Instant::now() + timeout;
        let exit_success = loop {
            if let Some(status) = child.try_wait()? {
                break Some(status.success());
            }
            if Instant::now() >= deadline {
                child.kill()?;
                child.wait()?;
                break None;
            }
            thread::sleep(Duration::from_millis(25));
        };

        drop(diagnostic_file);
        let mut diagnostic = String::new();
        File::open(&self.diagnostic_log)?.read_to_string(&mut diagnostic)?;
        Ok(ValidationRun {
            diagnostic,
            exit_success,
        })
    }
}

fn stage_relative_model_catalog(
    config_toml: &[u8],
    source_codex_home: Option<&Path>,
    staged_codex_home: &Path,
) -> io::Result<()> {
    let Some(source_codex_home) = source_codex_home else {
        return Ok(());
    };
    let Ok(config_toml) = std::str::from_utf8(config_toml) else {
        return Ok(());
    };
    let Ok(document) = config_toml.parse::<toml_edit::DocumentMut>() else {
        return Ok(());
    };
    let Some(catalog_path) = document
        .get("model_catalog_json")
        .and_then(toml_edit::Item::as_str)
    else {
        return Ok(());
    };
    let catalog_path = Path::new(catalog_path);
    if catalog_path.is_absolute() {
        return Ok(());
    }

    let mut relative_path = PathBuf::new();
    for component in catalog_path.components() {
        match component {
            Component::Normal(component) => relative_path.push(component),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "model_catalog_json must remain within the Codex home",
                ));
            }
        }
    }
    if relative_path.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "model_catalog_json is empty",
        ));
    }

    let source = source_codex_home.join(&relative_path);
    let metadata = fs::symlink_metadata(&source)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "model_catalog_json must reference a regular file",
        ));
    }
    if metadata.len() > MAX_STAGED_REFERENCE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "model_catalog_json is too large to stage",
        ));
    }

    let destination = staged_codex_home.join(relative_path);
    if let Some(parent) = destination.parent() {
        create_private_dir(parent)?;
    }
    write_private(&destination, &fs::read(source)?)
}

fn classify_validation_run(exit_success: Option<bool>, diagnostic: &str) -> Result<(), String> {
    if diagnostic_indicates_config_rejection(diagnostic) {
        return Err(VALIDATION_REJECTED.to_owned());
    }
    if exit_success == Some(false) && !diagnostic_indicates_expected_shutdown(diagnostic) {
        return Err(VALIDATION_PROCESS_FAILED.to_owned());
    }
    Ok(())
}

fn diagnostic_indicates_expected_shutdown(diagnostic: &str) -> bool {
    let diagnostic = diagnostic.to_ascii_lowercase();
    diagnostic.contains("stdin reached eof")
        || diagnostic.contains("failed to read request from stdin")
        || diagnostic.contains("stdin is closed")
}

fn diagnostic_indicates_config_rejection(diagnostic: &str) -> bool {
    let diagnostic = diagnostic.to_ascii_lowercase();
    const DIRECT_MARKERS: &[&str] = &[
        "failed to load config",
        "failed to read config",
        "failed to parse config",
        "error loading config",
        "error loading configuration",
        "unable to load config",
        "invalid config",
        "configuration error",
        "toml parse error",
        "strict config error",
    ];
    if DIRECT_MARKERS
        .iter()
        .any(|marker| diagnostic.contains(marker))
    {
        return true;
    }

    diagnostic.contains("config.toml")
        && [
            "parse error",
            "invalid",
            "unknown field",
            "unknown key",
            "unsupported",
            "failed",
            "error",
        ]
        .iter()
        .any(|marker| diagnostic.contains(marker))
}

fn discover_binary(
    override_value: Option<&OsStr>,
    path_value: Option<&OsStr>,
    known: &[PathBuf],
) -> Option<PathBuf> {
    if let Some(override_value) = override_value.filter(|value| !value.is_empty()) {
        return resolve_override(override_value, path_value);
    }

    known
        .iter()
        .find(|candidate| is_usable_binary(candidate))
        .cloned()
        .or_else(|| find_on_path(OsStr::new("codex"), path_value))
}

fn discover_with_desktop(
    override_value: Option<&OsStr>,
    path_value: Option<&OsStr>,
    known: &[PathBuf],
    desktop_executable: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(override_value) = override_value.filter(|value| !value.is_empty()) {
        return resolve_override(override_value, path_value);
    }

    desktop_executable
        .into_iter()
        .flat_map(bundled_codex_candidates)
        .find(|candidate| is_usable_binary(candidate))
        .or_else(|| discover_binary(None, path_value, known))
}

fn bundled_codex_candidates(desktop_executable: &Path) -> impl Iterator<Item = PathBuf> {
    let parent = desktop_executable.parent().unwrap_or_else(|| Path::new(""));
    [
        parent.join("resources").join("codex.exe"),
        parent.join("Resources").join("codex.exe"),
        parent.join("resources").join("bin").join("codex.exe"),
        parent.join("codex.exe"),
    ]
    .into_iter()
}

fn resolve_override(override_value: &OsStr, path_value: Option<&OsStr>) -> Option<PathBuf> {
    let candidate = PathBuf::from(override_value);
    if candidate.is_absolute() || candidate.components().count() > 1 {
        return is_usable_binary(&candidate).then_some(candidate);
    }

    find_on_path(override_value, path_value)
        .or_else(|| is_usable_binary(&candidate).then_some(candidate))
}

fn find_on_path(binary_name: &OsStr, path_value: Option<&OsStr>) -> Option<PathBuf> {
    find_on_path_for_platform(binary_name, path_value, cfg!(target_os = "windows"))
}

fn find_on_path_for_platform(
    binary_name: &OsStr,
    path_value: Option<&OsStr>,
    windows: bool,
) -> Option<PathBuf> {
    let path_value = path_value?;
    let names = executable_names_for_platform(binary_name, windows);
    env::split_paths(path_value)
        .flat_map(|directory| names.iter().map(move |name| directory.join(name)))
        .find(|candidate| is_usable_binary(candidate))
}

fn executable_names_for_platform(binary_name: &OsStr, windows: bool) -> Vec<OsString> {
    if !windows || Path::new(binary_name).extension().is_some() {
        return vec![binary_name.to_owned()];
    }

    [".exe", ".cmd", ".bat"]
        .into_iter()
        .map(|extension| {
            let mut executable = binary_name.to_owned();
            executable.push(extension);
            executable
        })
        .collect()
}

fn is_usable_binary(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }

    #[cfg(not(unix))]
    true
}

#[cfg(target_os = "macos")]
fn known_binaries() -> &'static [PathBuf] {
    use std::sync::OnceLock;
    static PATHS: OnceLock<Vec<PathBuf>> = OnceLock::new();
    PATHS.get_or_init(|| {
        vec![
            PathBuf::from("/Applications/ChatGPT.app/Contents/Resources/codex"),
            PathBuf::from("/Applications/Codex.app/Contents/Resources/codex"),
        ]
    })
}

#[cfg(not(target_os = "macos"))]
fn known_binaries() -> &'static [PathBuf] {
    &[]
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents)?;
    file.sync_all()
}

fn set_private_file_permissions(_file: &File) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        _file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_diagnostics_are_classified_without_returning_the_diagnostic() {
        let secret = "sk-test-secret";
        let diagnostic =
            format!("failed to load config.toml: TOML parse error near OPENAI_API_KEY={secret}");
        assert!(diagnostic_indicates_config_rejection(&diagnostic));
        assert!(!VALIDATION_REJECTED.contains(secret));
        assert!(!VALIDATION_REJECTED.contains("config.toml:"));
    }

    #[test]
    fn stdin_shutdown_is_not_mistaken_for_config_rejection() {
        assert!(!diagnostic_indicates_config_rejection(
            "app-server transport stopped: stdin reached EOF"
        ));
        assert!(!diagnostic_indicates_config_rejection(
            "failed to read request from stdin"
        ));
    }

    #[test]
    fn unexpected_nonzero_exit_is_not_mistaken_for_valid_configuration() {
        assert!(classify_validation_run(Some(false), "unknown option --strict-config").is_err());
        assert!(classify_validation_run(Some(true), "").is_ok());
        assert!(classify_validation_run(None, "").is_ok());
        assert!(
            classify_validation_run(
                Some(false),
                "app-server transport stopped: stdin reached EOF"
            )
            .is_ok()
        );
    }

    #[test]
    fn desktop_bundled_codex_is_discovered_before_path() {
        let directory = tempfile::tempdir().unwrap();
        let desktop = directory.path().join("OpenAI/ChatGPT/ChatGPT.exe");
        let bundled = directory.path().join("OpenAI/ChatGPT/resources/codex.exe");
        fs::create_dir_all(desktop.parent().unwrap()).unwrap();
        fs::create_dir_all(bundled.parent().unwrap()).unwrap();
        make_executable(&desktop);
        make_executable(&bundled);

        let discovered = discover_with_desktop(None, None, &[], Some(&desktop));

        assert_eq!(discovered.as_deref(), Some(bundled.as_path()));
    }

    #[test]
    fn staged_home_copies_a_relative_model_catalog_from_the_live_codex_home() {
        let source = tempfile::tempdir().unwrap();
        let catalog = source.path().join("model-catalogs/relay-generated.json");
        fs::create_dir_all(catalog.parent().unwrap()).unwrap();
        fs::write(&catalog, br#"{"models":[{"slug":"relay-model"}]}"#).unwrap();
        let config = r#"
model_catalog_json = "model-catalogs/relay-generated.json"
"#;

        let staged =
            StagedCodexHome::create(config.as_bytes(), b"{}", Some(source.path())).unwrap();

        assert_eq!(
            fs::read(
                staged
                    .codex_home
                    .join("model-catalogs/relay-generated.json")
            )
            .unwrap(),
            fs::read(catalog).unwrap()
        );
    }

    #[test]
    fn staged_home_rejects_a_model_catalog_that_escapes_the_codex_home() {
        let source = tempfile::tempdir().unwrap();
        let config = r#"model_catalog_json = "../outside.json""#;

        let error = StagedCodexHome::create(config.as_bytes(), b"{}", Some(source.path()))
            .err()
            .unwrap();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn staged_home_reports_a_missing_relative_model_catalog() {
        let source = tempfile::tempdir().unwrap();
        let config = r#"model_catalog_json = "model-catalogs/missing.json""#;

        let error = StagedCodexHome::create(config.as_bytes(), b"{}", Some(source.path()))
            .err()
            .unwrap();

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn explicit_override_is_authoritative() {
        let directory = tempfile::tempdir().unwrap();
        let override_binary = directory.path().join("override-codex");
        make_executable(&override_binary);
        let known_binary = directory.path().join("known-codex");
        make_executable(&known_binary);

        let discovered = discover_binary(
            Some(override_binary.as_os_str()),
            None,
            std::slice::from_ref(&known_binary),
        );
        assert_eq!(discovered.as_deref(), Some(override_binary.as_path()));

        let missing = directory.path().join("missing-codex");
        assert_eq!(
            discover_binary(
                Some(missing.as_os_str()),
                None,
                std::slice::from_ref(&known_binary)
            ),
            None
        );
    }

    #[test]
    fn discovery_falls_back_from_known_location_to_path() {
        let directory = tempfile::tempdir().unwrap();
        let path_binary = directory.path().join(
            executable_names_for_platform(OsStr::new("codex"), cfg!(target_os = "windows"))[0]
                .clone(),
        );
        make_executable(&path_binary);
        let path = env::join_paths([directory.path()]).unwrap();

        let discovered = discover_binary(None, Some(path.as_os_str()), &[]);
        assert_eq!(discovered.as_deref(), Some(path_binary.as_path()));
    }

    #[test]
    fn windows_path_discovery_prefers_the_cmd_shim_over_the_unix_shim() {
        let directory = tempfile::tempdir().unwrap();
        let unix_shim = directory.path().join("codex");
        let cmd_shim = directory.path().join("codex.cmd");
        make_executable(&unix_shim);
        make_executable(&cmd_shim);
        let path = env::join_paths([directory.path()]).unwrap();

        let discovered =
            find_on_path_for_platform(OsStr::new("codex"), Some(path.as_os_str()), true);

        assert_eq!(discovered.as_deref(), Some(cmd_shim.as_path()));
    }

    #[test]
    fn staged_home_uses_only_its_isolated_paths() {
        let staged = StagedCodexHome::create(
            b"model = \"gpt-test\"\n",
            br#"{"OPENAI_API_KEY":"sk-test"}"#,
            None,
        )
        .unwrap();
        assert!(staged.codex_home.join("config.toml").is_file());
        assert!(staged.codex_home.join("auth.json").is_file());
        assert!(staged.sqlite_home.starts_with(staged._root.path()));
        assert!(staged.working_directory.starts_with(staged._root.path()));
    }

    fn make_executable(path: &Path) {
        fs::write(path, b"test").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
    }
}
