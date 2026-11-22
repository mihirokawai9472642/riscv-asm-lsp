use std::fs;
use std::path::{Path, PathBuf};

use lsp_types::Uri;

pub fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    let parsed = url::Url::parse(uri.as_str()).ok()?;
    parsed.to_file_path().ok()
}

/// Convert a filesystem path to a `file://` URI without dereferencing symlinks.
pub fn path_to_uri(path: &Path) -> Option<Uri> {
    let abs = if path.is_absolute() {
        lexical_normalize_path(path)
    } else {
        let cwd = std::env::current_dir().ok()?;
        lexical_normalize_path(&cwd.join(path))
    };
    let url = url::Url::from_file_path(&abs).ok()?;
    url.as_str().parse().ok()
}

/// Map a path back under the client's workspace root when it lives inside the workspace,
/// avoiding exposing canonicalized symlink targets in LSP responses.
pub fn display_path_for_client(
    path: &Path,
    client_root: Option<&Path>,
    canon_root: Option<&Path>,
) -> PathBuf {
    if let (Some(c_root), Some(k_root)) = (client_root, canon_root) {
        let norm_p = normalize_path(path);
        if let Ok(rel) = norm_p.strip_prefix(k_root) {
            return lexical_normalize_path(&c_root.join(rel));
        }
    }
    lexical_normalize_path(path)
}

pub(super) fn resolve_path(base: &Path, target: &str) -> PathBuf {
    let p = PathBuf::from(target);
    let joined = if p.is_absolute() { p } else { base.join(p) };
    normalize_path(&joined)
}

pub fn lexical_normalize_path(path: &Path) -> PathBuf {
    let mut stack: Vec<std::path::Component<'_>> = Vec::new();
    for comp in path.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => match stack.last() {
                Some(std::path::Component::Normal(_)) => {
                    stack.pop();
                }
                Some(std::path::Component::RootDir) | Some(std::path::Component::Prefix(_)) => {}
                _ => stack.push(comp),
            },
            other => stack.push(other),
        }
    }
    stack.iter().collect()
}

pub fn normalize_path(path: &Path) -> PathBuf {
    if let Ok(canon) = fs::canonicalize(path) {
        return canon;
    }
    lexical_normalize_path(path)
}
