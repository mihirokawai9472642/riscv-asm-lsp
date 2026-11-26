use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;

use super::uri::{normalize_path, path_starts_with};

pub const PROJECT_CONFIG_FILENAMES: &[&str] = &[".riscv-asm-lsp.yaml", ".riscv-asm-lsp.yml"];

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ProjectConfigFile {
    #[serde(rename = "CompileFlags", alias = "compile_flags", default)]
    pub compile_flags: ProjectCompileFlags,
    #[serde(rename = "Diagnostics", alias = "diagnostics", default)]
    pub diagnostics: ProjectDiagnostics,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ProjectCompileFlags {
    #[serde(rename = "Add", alias = "add", default)]
    pub add: Vec<String>,
    #[serde(rename = "Remove", alias = "remove", default)]
    pub remove: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ProjectDiagnostics {
    #[serde(rename = "Suppress", alias = "suppress", default)]
    pub suppress: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LoadedProjectConfig {
    pub base_dir: PathBuf,
    pub mtime: Option<SystemTime>,
    pub data: ProjectConfigFile,
}

/// Find the nearest `.riscv-asm-lsp.yaml` / `.riscv-asm-lsp.yml` walking upward
/// from `file_path`'s parent directory up to `stop_root` (inclusive).
pub(super) fn find_nearest_project_config(
    file_path: &Path,
    stop_root: Option<&Path>,
) -> Option<(PathBuf, Option<SystemTime>)> {
    let mut cur = file_path.parent();
    while let Some(dir) = cur {
        if let Some(root) = stop_root
            && !path_starts_with(&normalize_path(dir), root)
            && !path_starts_with(dir, root)
        {
            break;
        }
        for filename in PROJECT_CONFIG_FILENAMES {
            let candidate = dir.join(filename);
            if let Ok(meta) = fs::metadata(&candidate)
                && meta.is_file()
            {
                return Some((candidate, meta.modified().ok()));
            }
        }
        if let Some(root) = stop_root
            && (normalize_path(dir) == *root || dir == root)
        {
            break;
        }
        cur = dir.parent();
    }
    None
}

pub(super) fn parse_project_config_file(path: &Path) -> Option<ProjectConfigFile> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(err) => {
            eprintln!("riscv-asm-lsp: failed to read {}: {err}", path.display());
            return None;
        }
    };
    if text.trim().is_empty() {
        return Some(ProjectConfigFile::default());
    }
    match yaml_serde::from_str::<ProjectConfigFile>(&text) {
        Ok(mut cfg) => {
            cfg.diagnostics.suppress = cfg
                .diagnostics
                .suppress
                .into_iter()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect();
            Some(cfg)
        }
        Err(err) => {
            eprintln!("riscv-asm-lsp: failed to parse {}: {err}", path.display());
            None
        }
    }
}
