//! Cross-platform path normalization and resolution matching Python `pathlib.Path`.

use std::path::{Component, Path, PathBuf};

/// Strip Windows verbatim UNC prefix (`\\?\` or `\\?\UNC\`) so paths match standard OS strings.
pub fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(text) = path.to_str() {
            if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
                return PathBuf::from(format!(r"\\{rest}"));
            }
            if let Some(rest) = text.strip_prefix(r"\\?\") {
                return PathBuf::from(rest);
            }
        }
    }
    path.to_path_buf()
}

/// Resolve `path` similarly to Python's `Path.resolve(strict=False)`.
///
/// Existing prefixes are canonicalized (resolving symlinks and case) and stripped of
/// Windows `\\?\` prefixes; non-existent trailing segments are lexically normalized.
pub fn resolve_path(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return strip_verbatim_prefix(&canonical);
    }

    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut existing = abs.as_path();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() {
        if let Some(name) = existing.file_name() {
            tail.push(name.to_os_string());
        }
        match existing.parent() {
            Some(parent) => existing = parent,
            None => break,
        }
    }

    let mut base = if existing.exists() {
        std::fs::canonicalize(existing)
            .map(|p| strip_verbatim_prefix(&p))
            .unwrap_or_else(|_| strip_verbatim_prefix(existing))
    } else {
        strip_verbatim_prefix(&abs)
    };

    for segment in tail.into_iter().rev() {
        base.push(segment);
    }
    normalize_components(&base)
}

fn normalize_components(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !matches!(
                    out.components().next_back(),
                    None | Some(Component::RootDir | Component::Prefix(_))
                ) {
                    out.pop();
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Render a path with forward slashes (matching Python's `Path.as_posix()`).
pub fn to_posix(path: &Path) -> String {
    #[cfg(windows)]
    {
        path.to_string_lossy().replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        path.to_string_lossy().into_owned()
    }
}

/// Compute `path` relative to `root` as a forward-slash string, resolving both first.
pub fn relative_posix(path: &Path, root: &Path) -> Option<String> {
    let resolved_path = resolve_path(path);
    let resolved_root = resolve_path(root);
    resolved_path
        .strip_prefix(&resolved_root)
        .ok()
        .map(to_posix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_verbatim_handles_drive_and_unc() {
        #[cfg(windows)]
        {
            assert_eq!(
                strip_verbatim_prefix(Path::new(r"\\?\D:\Projects\demo")),
                PathBuf::from(r"D:\Projects\demo")
            );
            assert_eq!(
                strip_verbatim_prefix(Path::new(r"\\?\UNC\server\share\file")),
                PathBuf::from(r"\\server\share\file")
            );
        }
        assert_eq!(
            strip_verbatim_prefix(Path::new("/tmp/demo")),
            PathBuf::from("/tmp/demo")
        );
    }

    #[test]
    fn resolve_path_handles_missing_leaf() {
        let temp = std::env::temp_dir();
        let candidate = temp.join("mutator_nonexistent_dir_12345").join("a.py");
        let resolved = resolve_path(&candidate);
        assert!(resolved.is_absolute());
        assert!(resolved.ends_with("mutator_nonexistent_dir_12345/a.py"));
    }
}
