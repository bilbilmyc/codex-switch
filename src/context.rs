use std::fs;
use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item};

const DEFAULT_PROJECT_DOC_MAX_BYTES: u64 = 32 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstructionScope {
    Global,
    Project,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstructionSource {
    pub name: String,
    pub path: PathBuf,
    pub scope: InstructionScope,
    pub counted_bytes: u64,
    pub estimated_tokens: u64,
    pub truncated: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InstructionSummary {
    pub sources: Vec<InstructionSource>,
    pub estimated_tokens: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DiscoveryConfig {
    project_doc_max_bytes: u64,
    fallback_filenames: Vec<String>,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            project_doc_max_bytes: DEFAULT_PROJECT_DOC_MAX_BYTES,
            fallback_filenames: Vec::new(),
        }
    }
}

pub fn discover_instruction_sources(
    codex_dir: &Path,
    cwd: Option<&Path>,
    config_toml: &str,
) -> InstructionSummary {
    let config = discovery_config(config_toml);
    let mut sources = Vec::new();

    if let Some(path) = first_non_empty_file(&[
        codex_dir.join("AGENTS.override.md"),
        codex_dir.join("AGENTS.md"),
    ]) && let Some(source) = instruction_source(path, InstructionScope::Global, None)
    {
        sources.push(source);
    }

    if let Some(cwd) = cwd.filter(|path| path.is_dir()) {
        let root = project_root(cwd);
        let mut remaining = config.project_doc_max_bytes;
        for directory in path_chain(&root, cwd) {
            if remaining == 0 {
                break;
            }
            let mut candidates = vec![
                directory.join("AGENTS.override.md"),
                directory.join("AGENTS.md"),
            ];
            candidates.extend(
                config
                    .fallback_filenames
                    .iter()
                    .map(|name| directory.join(name)),
            );
            let Some(path) = first_non_empty_file(&candidates) else {
                continue;
            };
            let Some(source) = instruction_source(path, InstructionScope::Project, Some(remaining))
            else {
                continue;
            };
            remaining = remaining.saturating_sub(source.counted_bytes);
            sources.push(source);
        }
    }

    InstructionSummary {
        estimated_tokens: sources.iter().fold(0_u64, |total, source| {
            total.saturating_add(source.estimated_tokens)
        }),
        sources,
    }
}

fn discovery_config(raw: &str) -> DiscoveryConfig {
    let Ok(document) = raw.parse::<DocumentMut>() else {
        return DiscoveryConfig::default();
    };
    let root = document.as_table();
    let project_doc_max_bytes = root
        .get("project_doc_max_bytes")
        .and_then(Item::as_integer)
        .and_then(|value| u64::try_from(value).ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_PROJECT_DOC_MAX_BYTES);
    let fallback_filenames = root
        .get("project_doc_fallback_filenames")
        .and_then(Item::as_array)
        .map(|array| {
            array
                .iter()
                .filter_map(|value| value.as_str())
                .filter(|value| is_safe_fallback_filename(value))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    DiscoveryConfig {
        project_doc_max_bytes,
        fallback_filenames,
    }
}

fn is_safe_fallback_filename(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains('/')
        && !value.contains('\\')
}

fn first_non_empty_file(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates.iter().find_map(|path| {
        let metadata = fs::symlink_metadata(path).ok()?;
        (!metadata.file_type().is_symlink() && metadata.is_file() && metadata.len() > 0)
            .then(|| path.clone())
    })
}

fn instruction_source(
    path: PathBuf,
    scope: InstructionScope,
    remaining_bytes: Option<u64>,
) -> Option<InstructionSource> {
    let metadata = fs::symlink_metadata(&path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() == 0 {
        return None;
    }
    let counted_bytes = remaining_bytes
        .map(|remaining| metadata.len().min(remaining))
        .unwrap_or(metadata.len());
    Some(InstructionSource {
        name: path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("AGENTS.md")
            .to_owned(),
        path,
        scope,
        counted_bytes,
        estimated_tokens: counted_bytes.saturating_add(3) / 4,
        truncated: counted_bytes < metadata.len(),
    })
}

fn project_root(cwd: &Path) -> PathBuf {
    cwd.ancestors()
        .find(|directory| directory.join(".git").exists())
        .unwrap_or(cwd)
        .to_path_buf()
}

fn path_chain(root: &Path, cwd: &Path) -> Vec<PathBuf> {
    let mut directories: Vec<PathBuf> = cwd
        .ancestors()
        .take_while(|directory| directory.starts_with(root))
        .map(Path::to_path_buf)
        .collect();
    directories.reverse();
    directories
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_global_and_layered_project_instructions_in_precedence_order() {
        let temp = tempfile::tempdir().unwrap();
        let codex_dir = temp.path().join(".codex");
        let project = temp.path().join("project");
        let nested = project.join("crates/app");
        fs::create_dir_all(&codex_dir).unwrap();
        fs::create_dir_all(project.join(".git")).unwrap();
        fs::create_dir_all(&nested).unwrap();
        fs::write(codex_dir.join("AGENTS.md"), "global").unwrap();
        fs::write(project.join("AGENTS.md"), "root rules").unwrap();
        fs::write(nested.join("AGENTS.override.md"), "nested rules").unwrap();

        let summary = discover_instruction_sources(&codex_dir, Some(&nested), "");

        assert_eq!(summary.sources.len(), 3);
        assert_eq!(summary.sources[0].scope, InstructionScope::Global);
        assert_eq!(summary.sources[1].path, project.join("AGENTS.md"));
        assert_eq!(summary.sources[2].path, nested.join("AGENTS.override.md"));
        assert!(summary.estimated_tokens > 0);
    }

    #[test]
    fn override_and_configured_fallback_follow_codex_discovery_rules() {
        let temp = tempfile::tempdir().unwrap();
        let codex_dir = temp.path().join(".codex");
        let project = temp.path().join("project");
        fs::create_dir_all(&codex_dir).unwrap();
        fs::create_dir_all(project.join(".git")).unwrap();
        fs::write(codex_dir.join("AGENTS.md"), "base").unwrap();
        fs::write(codex_dir.join("AGENTS.override.md"), "override").unwrap();
        fs::write(project.join("TEAM_GUIDE.md"), "fallback").unwrap();

        let summary = discover_instruction_sources(
            &codex_dir,
            Some(&project),
            "project_doc_fallback_filenames = [\"TEAM_GUIDE.md\"]\n",
        );

        assert_eq!(summary.sources.len(), 2);
        assert_eq!(summary.sources[0].name, "AGENTS.override.md");
        assert_eq!(summary.sources[1].name, "TEAM_GUIDE.md");
    }

    #[test]
    fn project_budget_truncates_without_reading_beyond_the_configured_limit() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        fs::create_dir_all(project.join(".git")).unwrap();
        fs::write(project.join("AGENTS.md"), vec![b'x'; 100]).unwrap();

        let summary = discover_instruction_sources(
            &temp.path().join(".codex"),
            Some(&project),
            "project_doc_max_bytes = 40\n",
        );

        assert_eq!(summary.sources[0].counted_bytes, 40);
        assert_eq!(summary.sources[0].estimated_tokens, 10);
        assert!(summary.sources[0].truncated);
    }

    #[cfg(unix)]
    #[test]
    fn ignores_symlinked_instruction_files() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let codex_dir = temp.path().join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();
        let target = temp.path().join("outside.md");
        fs::write(&target, "secret").unwrap();
        symlink(&target, codex_dir.join("AGENTS.md")).unwrap();

        let summary = discover_instruction_sources(&codex_dir, None, "");

        assert!(summary.sources.is_empty());
    }
}
