#[cfg(unix)]
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use directories::BaseDirs;
use thiserror::Error;

use crate::domain::ProfileId;

pub const PRIVATE_DIR_MODE: u32 = 0o700;
pub const PRIVATE_FILE_MODE: u32 = 0o600;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppPaths {
    pub home_dir: PathBuf,
    pub codex_dir: PathBuf,
    pub codex_config: PathBuf,
    pub codex_auth: PathBuf,
    pub codex_sessions: PathBuf,
    pub codex_archived_sessions: PathBuf,
    pub tool_dir: PathBuf,
    pub profiles: PathBuf,
    pub state: PathBuf,
    pub journal: PathBuf,
    /// Process-lifetime lock. Transaction serialization continues to use `lock`.
    pub instance_lock: PathBuf,
    pub lock: PathBuf,
    pub model_cache_dir: PathBuf,
    pub usage_database: PathBuf,
    pub backups_dir: PathBuf,
}

impl AppPaths {
    pub fn discover() -> Result<Self, PathsError> {
        let base_dirs = BaseDirs::new().ok_or(PathsError::HomeDirectoryUnavailable)?;
        Ok(Self::from_home(base_dirs.home_dir()))
    }

    pub fn from_home(home_dir: impl AsRef<Path>) -> Self {
        let home_dir = home_dir.as_ref().to_path_buf();
        let codex_dir = home_dir.join(".codex");
        let tool_dir = home_dir.join(".codex-switch");

        Self {
            codex_config: codex_dir.join("config.toml"),
            codex_auth: codex_dir.join("auth.json"),
            codex_sessions: codex_dir.join("sessions"),
            codex_archived_sessions: codex_dir.join("archived_sessions"),
            profiles: tool_dir.join("profiles.toml"),
            state: tool_dir.join("state.json"),
            journal: tool_dir.join("transaction.json"),
            instance_lock: tool_dir.join(".instance.lock"),
            lock: tool_dir.join(".lock"),
            model_cache_dir: tool_dir.join("model-cache"),
            usage_database: tool_dir.join("usage.sqlite3"),
            backups_dir: tool_dir.join("backups"),
            home_dir,
            codex_dir,
            tool_dir,
        }
    }

    pub fn model_cache_file(&self, profile_id: ProfileId) -> PathBuf {
        self.model_cache_dir.join(format!("{profile_id}.json"))
    }
}

#[derive(Debug, Error)]
pub enum PathsError {
    #[error("the current user's home directory could not be determined")]
    HomeDirectoryUnavailable,
}

#[cfg(unix)]
pub fn set_private_dir_mode(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_DIR_MODE))
}

#[cfg(not(unix))]
pub fn set_private_dir_mode(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
pub fn set_private_file_mode(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_FILE_MODE))
}

#[cfg(not(unix))]
pub fn set_private_file_mode(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_all_paths_from_the_home_directory() {
        let home = PathBuf::from("example-home");
        let paths = AppPaths::from_home(&home);

        assert_eq!(paths.codex_config, home.join(".codex/config.toml"));
        assert_eq!(paths.codex_auth, home.join(".codex/auth.json"));
        assert_eq!(paths.codex_sessions, home.join(".codex/sessions"));
        assert_eq!(
            paths.codex_archived_sessions,
            home.join(".codex/archived_sessions")
        );
        assert_eq!(paths.profiles, home.join(".codex-switch/profiles.toml"));
        assert_eq!(
            paths.usage_database,
            home.join(".codex-switch/usage.sqlite3")
        );
        assert_eq!(
            paths.instance_lock,
            home.join(".codex-switch/.instance.lock")
        );
        assert_eq!(paths.lock, home.join(".codex-switch/.lock"));
        assert_eq!(
            paths.model_cache_file(ProfileId::from_uuid(
                uuid::Uuid::parse_str("00000000-0000-4000-8000-000000000001").unwrap()
            )),
            home.join(".codex-switch/model-cache/00000000-0000-4000-8000-000000000001.json")
        );
    }

    #[cfg(unix)]
    #[test]
    fn private_mode_helpers_apply_expected_modes() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("secret");
        fs::write(&file, b"secret").unwrap();

        set_private_dir_mode(temp.path()).unwrap();
        set_private_file_mode(&file).unwrap();

        assert_eq!(
            fs::metadata(temp.path()).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
