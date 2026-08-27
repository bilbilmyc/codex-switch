use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::durable_fs::{self, DurableFsError};
use crate::legacy_usage::{ProfileLegacyUsageWindow, normalize_profile_windows};
use crate::paths::set_private_file_mode;
use crate::usage::{self, UsageError, UsagePeriod, UsageReport, UsageScope};

const DATABASE_SCHEMA_VERSION: u32 = 2;

/// Private, rebuildable index for locally derived Codex usage reports.
///
/// The store never persists raw session lines, prompt text, or API keys. A report cache is reused
/// only while the JSONL source fingerprint is unchanged; otherwise it is replaced atomically.
#[derive(Clone, Debug)]
pub struct UsageStore {
    path: PathBuf,
}

impl UsageStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn refresh(
        &self,
        sessions_dir: &Path,
        archived_sessions_dir: &Path,
        period: UsagePeriod,
        provider_filter: Option<&str>,
    ) -> Result<UsageReport, UsageStoreError> {
        let scope = UsageScope::from_provider_filter(provider_filter);
        self.refresh_scoped(sessions_dir, archived_sessions_dir, period, &scope)
    }

    pub fn refresh_scoped(
        &self,
        sessions_dir: &Path,
        archived_sessions_dir: &Path,
        period: UsagePeriod,
        scope: &UsageScope,
    ) -> Result<UsageReport, UsageStoreError> {
        let sources = SourceSet::collect(sessions_dir, archived_sessions_dir)?;
        let signature = sources.signature()?;
        let scope_fingerprint = scope_fingerprint(scope)?;
        let mut connection = self.open_connection()?;
        migrate(&connection)?;

        if let Some(cached) =
            load_cached_report(&connection, period, &scope_fingerprint, &signature)?
        {
            match serde_json::from_slice(&cached) {
                Ok(report) => return Ok(report),
                Err(_) => clear_cached_report(&connection, period, &scope_fingerprint)?,
            }
        }

        let report =
            usage::collect_usage_scoped(sessions_dir, archived_sessions_dir, period, scope)?;
        let report_json = serde_json::to_vec(&report)?;
        let transaction = connection.transaction()?;
        store_sources(&transaction, &sources)?;
        store_report(
            &transaction,
            period,
            &scope_fingerprint,
            &signature,
            &report_json,
        )?;
        transaction.commit()?;
        set_private_file_mode(&self.path)?;
        Ok(report)
    }

    /// Retains completed, inferred legacy windows so backup rotation cannot silently change an
    /// already-attributed usage report. Only profile UUIDs and millisecond boundaries are stored.
    pub fn remember_legacy_windows(
        &self,
        windows: &[ProfileLegacyUsageWindow],
    ) -> Result<Vec<ProfileLegacyUsageWindow>, UsageStoreError> {
        let mut connection = self.open_connection()?;
        migrate(&connection)?;
        let transaction = connection.transaction()?;
        for window in normalize_profile_windows(windows.to_vec()) {
            transaction.execute(
                "INSERT OR IGNORE INTO legacy_usage_windows
                     (profile_id, start_unix_ms, end_exclusive_unix_ms)
                 VALUES (?1, ?2, ?3)",
                params![
                    window.profile_id.to_string(),
                    unix_ms_to_sql(window.start_unix_ms)?,
                    unix_ms_to_sql(window.end_exclusive_unix_ms)?,
                ],
            )?;
        }
        transaction.commit()?;
        let windows = load_legacy_windows(&connection)?;
        set_private_file_mode(&self.path)?;
        Ok(windows)
    }

    fn open_connection(&self) -> Result<Connection, UsageStoreError> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| UsageStoreError::MissingParent(self.path.clone()))?;
        durable_fs::ensure_private_dir(parent)?;
        let connection = Connection::open(&self.path)?;
        set_private_file_mode(&self.path)?;
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA journal_mode = DELETE;
             PRAGMA synchronous = NORMAL;",
        )?;
        Ok(connection)
    }
}

#[derive(Debug, Error)]
pub enum UsageStoreError {
    #[error("usage database path has no parent: {0}")]
    MissingParent(PathBuf),
    #[error(transparent)]
    FileSystem(#[from] DurableFsError),
    #[error("could not read usage source metadata: {0}")]
    SourceMetadata(#[from] io::Error),
    #[error("usage database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("could not serialize cached usage report: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error(transparent)]
    Usage(#[from] UsageError),
    #[error("invalid persisted legacy usage window: {0}")]
    InvalidLegacyWindow(String),
    #[error("usage database schema {found} is newer than supported schema {supported}")]
    UnsupportedSchema { found: u32, supported: u32 },
}

fn migrate(connection: &Connection) -> Result<(), UsageStoreError> {
    let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > DATABASE_SCHEMA_VERSION {
        return Err(UsageStoreError::UnsupportedSchema {
            found: version,
            supported: DATABASE_SCHEMA_VERSION,
        });
    }
    if version == DATABASE_SCHEMA_VERSION {
        return Ok(());
    }

    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS usage_sources (
             path TEXT PRIMARY KEY NOT NULL,
             size_bytes INTEGER,
             modified_nanos TEXT,
             state TEXT NOT NULL,
             observed_at_unix_ms INTEGER NOT NULL
         );
         DROP INDEX IF EXISTS usage_reports_signature;
         DROP TABLE IF EXISTS usage_reports;
         CREATE TABLE IF NOT EXISTS usage_reports (
             scope_fingerprint TEXT NOT NULL,
             period INTEGER NOT NULL,
             source_signature TEXT NOT NULL,
             report_json BLOB NOT NULL,
             refreshed_at_unix_ms INTEGER NOT NULL,
             PRIMARY KEY (scope_fingerprint, period)
         );
         CREATE INDEX IF NOT EXISTS usage_reports_signature
             ON usage_reports (source_signature);
         CREATE TABLE IF NOT EXISTS legacy_usage_windows (
             profile_id TEXT NOT NULL,
             start_unix_ms INTEGER NOT NULL,
             end_exclusive_unix_ms INTEGER NOT NULL,
             PRIMARY KEY (profile_id, start_unix_ms, end_exclusive_unix_ms)
         );
         CREATE INDEX IF NOT EXISTS legacy_usage_windows_profile
             ON legacy_usage_windows (profile_id, start_unix_ms);",
    )?;
    connection.pragma_update(None, "user_version", DATABASE_SCHEMA_VERSION)?;
    Ok(())
}

fn load_cached_report(
    connection: &Connection,
    period: UsagePeriod,
    scope_fingerprint: &str,
    source_signature: &str,
) -> Result<Option<Vec<u8>>, UsageStoreError> {
    Ok(connection
        .query_row(
            "SELECT report_json FROM usage_reports
             WHERE scope_fingerprint = ?1 AND period = ?2 AND source_signature = ?3",
            params![scope_fingerprint, period_key(period), source_signature],
            |row| row.get(0),
        )
        .optional()?)
}

fn clear_cached_report(
    connection: &Connection,
    period: UsagePeriod,
    scope_fingerprint: &str,
) -> Result<(), UsageStoreError> {
    connection.execute(
        "DELETE FROM usage_reports WHERE scope_fingerprint = ?1 AND period = ?2",
        params![scope_fingerprint, period_key(period)],
    )?;
    Ok(())
}

fn store_sources(
    transaction: &rusqlite::Transaction<'_>,
    sources: &SourceSet,
) -> Result<(), UsageStoreError> {
    transaction.execute("DELETE FROM usage_sources", [])?;
    let observed_at = now_unix_ms();
    let mut statement = transaction.prepare(
        "INSERT INTO usage_sources (path, size_bytes, modified_nanos, state, observed_at_unix_ms)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for source in &sources.entries {
        statement.execute(params![
            source.path,
            source.size_bytes.and_then(|size| i64::try_from(size).ok()),
            source.modified_nanos.map(|value| value.to_string()),
            source.state,
            observed_at,
        ])?;
    }
    Ok(())
}

fn store_report(
    transaction: &rusqlite::Transaction<'_>,
    period: UsagePeriod,
    scope_fingerprint: &str,
    source_signature: &str,
    report_json: &[u8],
) -> Result<(), UsageStoreError> {
    transaction.execute(
        "INSERT INTO usage_reports
             (scope_fingerprint, period, source_signature, report_json, refreshed_at_unix_ms)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(scope_fingerprint, period) DO UPDATE SET
             source_signature = excluded.source_signature,
             report_json = excluded.report_json,
             refreshed_at_unix_ms = excluded.refreshed_at_unix_ms",
        params![
            scope_fingerprint,
            period_key(period),
            source_signature,
            report_json,
            now_unix_ms(),
        ],
    )?;
    Ok(())
}

fn scope_fingerprint(scope: &UsageScope) -> Result<String, UsageStoreError> {
    let serialized = serde_json::to_vec(scope)?;
    let digest = Sha256::digest(serialized);
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn unix_ms_to_sql(value: u64) -> Result<i64, UsageStoreError> {
    i64::try_from(value).map_err(|_| UsageStoreError::InvalidLegacyWindow(value.to_string()))
}

fn load_legacy_windows(
    connection: &Connection,
) -> Result<Vec<ProfileLegacyUsageWindow>, UsageStoreError> {
    let mut statement = connection.prepare(
        "SELECT profile_id, start_unix_ms, end_exclusive_unix_ms
         FROM legacy_usage_windows
         ORDER BY profile_id, start_unix_ms, end_exclusive_unix_ms",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    let mut windows = Vec::new();
    for row in rows {
        let (profile_id, start_unix_ms, end_exclusive_unix_ms) = row?;
        let profile_id = uuid::Uuid::parse_str(&profile_id)
            .map_err(|_| UsageStoreError::InvalidLegacyWindow(profile_id))?;
        let start_unix_ms = u64::try_from(start_unix_ms)
            .map_err(|_| UsageStoreError::InvalidLegacyWindow(start_unix_ms.to_string()))?;
        let end_exclusive_unix_ms = u64::try_from(end_exclusive_unix_ms)
            .map_err(|_| UsageStoreError::InvalidLegacyWindow(end_exclusive_unix_ms.to_string()))?;
        windows.push(ProfileLegacyUsageWindow {
            profile_id: crate::domain::ProfileId::from_uuid(profile_id),
            start_unix_ms,
            end_exclusive_unix_ms,
        });
    }
    Ok(normalize_profile_windows(windows))
}

fn period_key(period: UsagePeriod) -> i64 {
    match period {
        UsagePeriod::Today => 0,
        UsagePeriod::Last7Days => 1,
        UsagePeriod::Last30Days => 2,
    }
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

#[derive(Clone, Debug, Serialize)]
struct SourceSet {
    entries: Vec<SourceFingerprint>,
}

impl SourceSet {
    fn collect(sessions_dir: &Path, archived_sessions_dir: &Path) -> Result<Self, UsageStoreError> {
        let mut entries = Vec::new();
        collect_root(sessions_dir, &mut entries)?;
        collect_root(archived_sessions_dir, &mut entries)?;
        entries.sort_by(|left, right| left.path.cmp(&right.path).then(left.state.cmp(right.state)));
        Ok(Self { entries })
    }

    fn signature(&self) -> Result<String, UsageStoreError> {
        let serialized = serde_json::to_vec(self)?;
        let digest = Sha256::digest(serialized);
        Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
    }
}

#[derive(Clone, Debug, Serialize)]
struct SourceFingerprint {
    path: String,
    size_bytes: Option<u64>,
    modified_nanos: Option<u128>,
    state: &'static str,
}

fn collect_root(root: &Path, entries: &mut Vec<SourceFingerprint>) -> Result<(), UsageStoreError> {
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            entries.push(SourceFingerprint::marker(root, "missing"));
            return Ok(());
        }
        Err(_) => {
            entries.push(SourceFingerprint::marker(root, "unreadable"));
            return Ok(());
        }
    };
    if metadata.file_type().is_symlink() {
        entries.push(SourceFingerprint::marker(root, "symlink"));
    } else if metadata.is_file() {
        if is_jsonl(root) {
            entries.push(SourceFingerprint::from_file(root, metadata));
        }
    } else if metadata.is_dir() {
        collect_directory(root, entries)?;
    } else {
        entries.push(SourceFingerprint::marker(root, "unsupported"));
    }
    Ok(())
}

fn collect_directory(
    directory: &Path,
    entries: &mut Vec<SourceFingerprint>,
) -> Result<(), UsageStoreError> {
    let mut pending = vec![directory.to_path_buf()];
    while let Some(current) = pending.pop() {
        let read_dir = match fs::read_dir(&current) {
            Ok(read_dir) => read_dir,
            Err(_) => {
                entries.push(SourceFingerprint::marker(&current, "unreadable"));
                continue;
            }
        };
        let mut paths = Vec::new();
        for entry in read_dir {
            match entry {
                Ok(entry) => paths.push(entry.path()),
                Err(_) => entries.push(SourceFingerprint::marker(&current, "entry-unreadable")),
            }
        }
        paths.sort();
        for path in paths.into_iter().rev() {
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(_) => {
                    entries.push(SourceFingerprint::marker(&path, "unreadable"));
                    continue;
                }
            };
            if metadata.file_type().is_symlink() {
                if is_jsonl(&path) {
                    entries.push(SourceFingerprint::marker(&path, "symlink"));
                }
            } else if metadata.is_dir() {
                pending.push(path);
            } else if metadata.is_file() && is_jsonl(&path) {
                entries.push(SourceFingerprint::from_file(&path, metadata));
            } else if is_jsonl(&path) {
                entries.push(SourceFingerprint::marker(&path, "unsupported"));
            }
        }
    }
    Ok(())
}

impl SourceFingerprint {
    fn marker(path: &Path, state: &'static str) -> Self {
        Self {
            path: path.to_string_lossy().into_owned(),
            size_bytes: None,
            modified_nanos: None,
            state,
        }
    }

    fn from_file(path: &Path, metadata: fs::Metadata) -> Self {
        let modified_nanos = metadata
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos());
        Self {
            path: path.to_string_lossy().into_owned(),
            size_bytes: Some(metadata.len()),
            modified_nanos,
            state: "file",
        }
    }
}

fn is_jsonl(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::LegacyUsageWindow;

    fn session(provider: &str, input_tokens: u64) -> String {
        let timestamp = chrono::Local::now().to_rfc3339();
        format!(
            r#"{{"type":"session_meta","payload":{{"id":"session-{provider}","model_provider":"{provider}","cwd":"/work"}}}}
{{"type":"turn_context","payload":{{"model":"model-{provider}"}}}}
{{"type":"event_msg","timestamp":"{timestamp}","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":{input_tokens},"output_tokens":2}},"total_token_usage":{{"input_tokens":{input_tokens},"output_tokens":2}},"model_context_window":128000}}}}}}
"#
        )
    }

    fn legacy_session(timestamp: &str, input_tokens: u64) -> String {
        format!(
            r#"{{"timestamp":"{timestamp}","type":"session_meta","payload":{{"id":"legacy-session","model_provider":"codex_switch","cwd":"/work"}}}}
{{"timestamp":"{timestamp}","type":"turn_context","payload":{{"model":"legacy-model"}}}}
{{"timestamp":"{timestamp}","type":"event_msg","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":{input_tokens},"output_tokens":2}},"total_token_usage":{{"input_tokens":{input_tokens},"output_tokens":2}},"model_context_window":128000}}}}}}
"#
        )
    }

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, UsageStore) {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        let archived = temp.path().join("archived");
        fs::create_dir_all(&sessions).unwrap();
        let store = UsageStore::new(temp.path().join("usage.sqlite3"));
        (temp, sessions, archived, store)
    }

    #[test]
    fn caches_provider_scoped_reports_and_rebuilds_when_a_source_changes() {
        let (_temp, sessions, archived, store) = fixture();
        let source = sessions.join("current.jsonl");
        fs::write(&source, session("relay-a", 7)).unwrap();

        let first = store
            .refresh(&sessions, &archived, UsagePeriod::Today, Some("relay-a"))
            .unwrap();
        assert_eq!(first.current.input_tokens, 7);

        let connection = Connection::open(store.path()).unwrap();
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM usage_reports", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);

        fs::write(&source, session("relay-a", 12)).unwrap();
        let rebuilt = store
            .refresh(&sessions, &archived, UsagePeriod::Today, Some("relay-a"))
            .unwrap();
        assert_eq!(rebuilt.current.input_tokens, 12);
    }

    #[test]
    fn keeps_provider_reports_separate_in_the_same_database() {
        let (_temp, sessions, archived, store) = fixture();
        fs::write(sessions.join("a.jsonl"), session("relay-a", 7)).unwrap();
        fs::write(sessions.join("b.jsonl"), session("relay-b", 23)).unwrap();

        let first = store
            .refresh(&sessions, &archived, UsagePeriod::Today, Some("relay-a"))
            .unwrap();
        let second = store
            .refresh(&sessions, &archived, UsagePeriod::Today, Some("relay-b"))
            .unwrap();
        assert_eq!(first.current.input_tokens, 7);
        assert_eq!(second.current.input_tokens, 23);
    }

    #[test]
    fn scoped_reports_do_not_reuse_a_cache_entry_when_legacy_windows_change() {
        let (_temp, sessions, archived, store) = fixture();
        let timestamp = chrono::Local::now().to_rfc3339();
        let timestamp_unix_ms: u64 = chrono::DateTime::parse_from_rfc3339(&timestamp)
            .unwrap()
            .timestamp_millis()
            .try_into()
            .unwrap();
        fs::write(sessions.join("legacy.jsonl"), legacy_session(&timestamp, 7)).unwrap();

        let owned_window = LegacyUsageWindow::new(
            timestamp_unix_ms.saturating_sub(60_000),
            timestamp_unix_ms.saturating_add(60_000),
        );
        let selected_scope = UsageScope::profile(
            "codex_switch_profile",
            "codex_switch",
            vec![owned_window],
            vec![owned_window],
        );
        let other_scope = UsageScope::profile(
            "codex_switch_profile",
            "codex_switch",
            vec![LegacyUsageWindow::new(
                timestamp_unix_ms.saturating_add(60_001),
                timestamp_unix_ms.saturating_add(120_000),
            )],
            vec![owned_window],
        );

        let selected = store
            .refresh_scoped(&sessions, &archived, UsagePeriod::Today, &selected_scope)
            .unwrap();
        let other = store
            .refresh_scoped(&sessions, &archived, UsagePeriod::Today, &other_scope)
            .unwrap();
        assert_eq!(selected.current.input_tokens, 7);
        assert_eq!(other.current.input_tokens, 0);
        assert_eq!(other.unattributed_legacy.input_tokens, 0);

        let connection = Connection::open(store.path()).unwrap();
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM usage_reports", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn retains_completed_legacy_windows_without_storing_session_contents() {
        let (_temp, _sessions, _archived, store) = fixture();
        let profile_id = crate::domain::ProfileId::from_uuid(
            uuid::Uuid::parse_str("e519bc8f-120c-43c3-96b5-a7799f6eec18").unwrap(),
        );
        let persisted = store
            .remember_legacy_windows(&[ProfileLegacyUsageWindow {
                profile_id,
                start_unix_ms: 100,
                end_exclusive_unix_ms: 200,
            }])
            .unwrap();

        assert_eq!(
            persisted,
            vec![ProfileLegacyUsageWindow {
                profile_id,
                start_unix_ms: 100,
                end_exclusive_unix_ms: 200,
            }]
        );
        let connection = Connection::open(store.path()).unwrap();
        let definition: String = connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'legacy_usage_windows'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!definition.contains("prompt"));
        assert!(!definition.contains("content"));
    }

    #[test]
    fn invalidates_the_v1_provider_filter_cache_schema() {
        let (_temp, sessions, archived, store) = fixture();
        fs::write(sessions.join("current.jsonl"), session("relay-a", 7)).unwrap();
        let connection = Connection::open(store.path()).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE usage_reports (
                     provider_scope INTEGER NOT NULL,
                     provider_filter TEXT NOT NULL,
                     period INTEGER NOT NULL,
                     source_signature TEXT NOT NULL,
                     report_json BLOB NOT NULL,
                     refreshed_at_unix_ms INTEGER NOT NULL,
                     PRIMARY KEY (provider_scope, provider_filter, period)
                 );
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        drop(connection);

        let report = store
            .refresh(&sessions, &archived, UsagePeriod::Today, Some("relay-a"))
            .unwrap();
        assert_eq!(report.current.input_tokens, 7);
        let connection = Connection::open(store.path()).unwrap();
        let version: u32 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, DATABASE_SCHEMA_VERSION);
        let definition: String = connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'usage_reports'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(definition.contains("scope_fingerprint"));
    }

    #[test]
    fn discards_an_unreadable_cached_report_and_rebuilds_it() {
        let (_temp, sessions, archived, store) = fixture();
        fs::write(sessions.join("current.jsonl"), session("relay-a", 7)).unwrap();
        let report = store
            .refresh(&sessions, &archived, UsagePeriod::Today, Some("relay-a"))
            .unwrap();
        assert_eq!(report.current.input_tokens, 7);

        let connection = Connection::open(store.path()).unwrap();
        connection
            .execute("UPDATE usage_reports SET report_json = X'00'", [])
            .unwrap();

        let rebuilt = store
            .refresh(&sessions, &archived, UsagePeriod::Today, Some("relay-a"))
            .unwrap();
        assert_eq!(rebuilt.current.input_tokens, 7);
    }

    #[cfg(unix)]
    #[test]
    fn creates_a_private_database_file() {
        use std::os::unix::fs::PermissionsExt as _;

        let (_temp, sessions, archived, store) = fixture();
        fs::write(sessions.join("current.jsonl"), session("relay-a", 7)).unwrap();
        store
            .refresh(&sessions, &archived, UsagePeriod::Today, Some("relay-a"))
            .unwrap();
        assert_eq!(
            fs::metadata(store.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
