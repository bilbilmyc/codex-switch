use std::fmt::Display;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use chrono::Local;

use crate::paths::AppPaths;

static LOGS_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Starts best-effort diagnostic logging. A logging failure must never block a switch.
pub fn initialize(paths: &AppPaths) -> io::Result<()> {
    fs::create_dir_all(&paths.logs_dir)?;
    crate::paths::set_private_dir_mode(&paths.logs_dir)?;
    let _ = LOGS_DIR.set(paths.logs_dir.clone());
    write("INFO", "startup", "application started")
}

pub fn info(event: &str, details: impl Display) {
    let _ = write("INFO", event, details);
}

pub fn warn(event: &str, details: impl Display) {
    let _ = write("WARN", event, details);
}

pub fn error(event: &str, details: impl Display) {
    let _ = write("ERROR", event, details);
}

/// Records failures that happen before `app::run` can initialize the logger.
pub fn record_startup_error(details: impl Display) {
    let Ok(paths) = AppPaths::discover() else {
        return;
    };
    if LOGS_DIR.get().is_none() && initialize(&paths).is_err() {
        return;
    }
    error("startup", details);
}

fn write(level: &str, event: &str, details: impl Display) -> io::Result<()> {
    let Some(logs_dir) = LOGS_DIR.get() else {
        return Ok(());
    };
    write_record(logs_dir, level, event, &redact(&details.to_string()))
}

fn write_record(logs_dir: &Path, level: &str, event: &str, details: &str) -> io::Result<()> {
    let now = Local::now();
    let path = logs_dir.join(format!("codex-switch-{}.log", now.format("%Y-%m-%d")));
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    crate::paths::set_private_file_mode(&path)?;
    writeln!(
        file,
        "{} [{}] {}: {}",
        now.format("%Y-%m-%d %H:%M:%S%.3f%:z"),
        level,
        clean(event),
        clean(details),
    )?;
    file.flush()
}

fn clean(value: &str) -> String {
    value.replace(['\r', '\n'], " ")
}

fn redact(value: &str) -> String {
    let mut redacted = value.to_owned();
    for prefix in ["sk-", "sk_", "sess-"] {
        redacted = redact_prefixed_token(&redacted, prefix);
    }
    redacted
}

fn redact_prefixed_token(value: &str, prefix: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut remainder = value;
    while let Some(index) = remainder.find(prefix) {
        result.push_str(&remainder[..index]);
        result.push_str("[redacted]");
        let token = &remainder[index..];
        let end = token
            .find(|character: char| {
                character.is_whitespace() || matches!(character, '\"' | '\'' | ',' | ')' | ']')
            })
            .unwrap_or(token.len());
        remainder = &token[end..];
    }
    result.push_str(remainder);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_a_dated_log_file() {
        let temp = tempfile::tempdir().unwrap();
        write_record(temp.path(), "ERROR", "switch", "validation failed").unwrap();

        let path = temp.path().join(format!(
            "codex-switch-{}.log",
            Local::now().format("%Y-%m-%d")
        ));
        let contents = fs::read_to_string(path).unwrap();
        assert!(contents.contains("[ERROR] switch: validation failed"));
    }

    #[test]
    fn redacts_common_secret_prefixes() {
        let value = redact("key sk-secret-token and sess-private-value");
        assert!(!value.contains("secret-token"));
        assert!(!value.contains("private-value"));
        assert_eq!(value, "key [redacted] and [redacted]");
    }
}
