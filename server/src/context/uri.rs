use std::fs;
use std::path::{Component, Path, PathBuf};

use lsp_types::Uri;

pub fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    let parsed = url::Url::parse(uri.as_str()).ok()?;
    parsed.to_file_path().ok()
}

/// Canonical lookup key for a `Uri`.
///
/// On Windows, treats `file:///C:/dir/a.S`, `file:///c%3A/dir/a.S`, and different
/// ASCII/Unicode cases as the same document; on Unix, preserves symlink distinction.
pub fn uri_key(uri: &Uri) -> String {
    #[cfg(windows)]
    {
        if let Some(path) = uri_to_path(uri) {
            return path_key(&path).to_string_lossy().into_owned();
        }
        uri.as_str().to_lowercase()
    }
    #[cfg(not(windows))]
    {
        uri.as_str().to_owned()
    }
}

/// Convert a filesystem path to a `file://` URI without dereferencing symlinks.
pub fn path_to_uri(path: &Path) -> Option<Uri> {
    let abs = if path.is_absolute() {
        lexical_normalize_path(path)
    } else {
        let cwd = std::env::current_dir().ok()?;
        lexical_normalize_path(&cwd.join(path))
    };
    let clean = strip_windows_verbatim_prefix(abs);
    let url = url::Url::from_file_path(&clean).ok()?;
    url.as_str().parse().ok()
}

/// Map a path back under the client's workspace root when it lives inside the workspace,
/// avoiding exposing canonicalized symlink targets or Windows verbatim prefixes in LSP responses.
pub fn display_path_for_client(
    path: &Path,
    client_root: Option<&Path>,
    canon_root: Option<&Path>,
) -> PathBuf {
    if let (Some(c_root), Some(k_root)) = (client_root, canon_root) {
        let norm_p = normalize_path(path);
        if let Some(rel) = path_strip_prefix(&norm_p, k_root) {
            return strip_windows_verbatim_prefix(lexical_normalize_path(&c_root.join(rel)));
        }
    }
    strip_windows_verbatim_prefix(lexical_normalize_path(path))
}

pub(super) fn resolve_path(base: &Path, target: &str) -> PathBuf {
    let p = PathBuf::from(target);
    let joined = if p.is_absolute() { p } else { base.join(p) };
    normalize_path(&joined)
}

pub fn lexical_normalize_path(path: &Path) -> PathBuf {
    let mut stack: Vec<Component<'_>> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => match stack.last() {
                Some(Component::Normal(_)) => {
                    stack.pop();
                }
                Some(Component::RootDir) | Some(Component::Prefix(_)) => {}
                _ => stack.push(comp),
            },
            other => stack.push(other),
        }
    }
    stack.iter().collect()
}

pub fn normalize_path(path: &Path) -> PathBuf {
    if let Ok(canon) = fs::canonicalize(path) {
        return strip_windows_verbatim_prefix(canon);
    }
    strip_windows_verbatim_prefix(lexical_normalize_path(path))
}

/// Canonicalized lookup key for internal maps and sets.
///
/// Strips Windows verbatim prefixes (`\\?\`) and folds case on Windows so mixed-case
/// paths match consistently while display URIs preserve client casing.
pub fn path_key(path: &Path) -> PathBuf {
    let norm = normalize_path(path);
    #[cfg(windows)]
    {
        PathBuf::from(windows_case_fold_path(&norm.to_string_lossy()))
    }
    #[cfg(not(windows))]
    {
        norm
    }
}

pub fn path_starts_with(path: &Path, base: &Path) -> bool {
    #[cfg(windows)]
    {
        path_strip_prefix(path, base).is_some()
    }
    #[cfg(not(windows))]
    {
        path.starts_with(base)
    }
}

pub fn path_strip_prefix(path: &Path, base: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let p_str = path.to_string_lossy();
        let b_str = base.to_string_lossy();
        strip_prefix_case_insensitive_str(&p_str, &b_str).map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        path.strip_prefix(base).ok().map(Path::to_path_buf)
    }
}

#[cfg(any(windows, test))]
pub(crate) fn strip_windows_verbatim_str(s: &str) -> Option<String> {
    let rest = s.strip_prefix(r"\\?\")?;
    if rest.len() >= 4 && rest[..4].eq_ignore_ascii_case(r"UNC\") {
        return Some(format!(r"\\{}", &rest[4..]));
    }
    let bytes = rest.as_bytes();
    if bytes.len() >= 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes.len() == 2 || bytes[2] == b'\\')
    {
        return Some(rest.to_owned());
    }
    None
}

pub(crate) fn strip_windows_verbatim_prefix(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let s = path.to_string_lossy();
        if let Some(stripped) = strip_windows_verbatim_str(&s) {
            return PathBuf::from(stripped);
        }
    }
    path
}

#[cfg(any(windows, test))]
pub(crate) fn windows_case_fold_path(s: &str) -> String {
    let slashed = s.replace('/', r"\");
    let stripped = strip_windows_verbatim_str(&slashed).unwrap_or(slashed);
    stripped.to_lowercase()
}

#[cfg(any(windows, test))]
pub(crate) fn strip_prefix_case_insensitive_str(path: &str, base: &str) -> Option<String> {
    let slash_p = path.replace('/', r"\");
    let norm_p = strip_windows_verbatim_str(&slash_p).unwrap_or(slash_p);
    let slash_b = base.replace('/', r"\");
    let norm_b = strip_windows_verbatim_str(&slash_b).unwrap_or(slash_b);
    let base_trimmed = norm_b.trim_end_matches('\\');
    if base_trimmed.is_empty() || norm_p.starts_with(r"\\") != norm_b.starts_with(r"\\") {
        return None;
    }
    let p_fold = norm_p.to_lowercase();
    let b_fold = base_trimmed.to_lowercase();
    if !p_fold.starts_with(&b_fold) {
        return None;
    }
    // Check component boundary safely on UTF-8 boundary of the original string if byte lens match,
    // or compare segments split by `\`.
    let p_segs: Vec<&str> = norm_p.split('\\').filter(|s| !s.is_empty()).collect();
    let b_segs: Vec<&str> = base_trimmed.split('\\').filter(|s| !s.is_empty()).collect();
    if p_segs.len() < b_segs.len() {
        return None;
    }
    for (ps, bs) in p_segs.iter().zip(b_segs.iter()) {
        if ps.to_lowercase() != bs.to_lowercase() {
            return None;
        }
    }
    Some(p_segs[b_segs.len()..].join(r"\"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_verbatim_and_case_folding_helpers() {
        assert_eq!(
            strip_windows_verbatim_str(r"\\?\C:\Proj\Main.S").as_deref(),
            Some(r"C:\Proj\Main.S")
        );
        assert_eq!(
            strip_windows_verbatim_str(r"\\?\UNC\server\share\inc\hdr.h").as_deref(),
            Some(r"\\server\share\inc\hdr.h")
        );
        assert_eq!(strip_windows_verbatim_str(r"C:\Normal\Path.S"), None);

        assert_eq!(
            windows_case_fold_path(r"\\?\C:/Proj/Sub/File.S"),
            r"c:\proj\sub\file.s"
        );
        assert_eq!(
            strip_prefix_case_insensitive_str(r"\\?\C:\Proj\Inc\Hdr.h", r"c:/proj").as_deref(),
            Some(r"Inc\Hdr.h")
        );
        assert_eq!(
            strip_prefix_case_insensitive_str(r"C:\ProjOther\a.S", r"C:\Proj"),
            None
        );
    }
}
