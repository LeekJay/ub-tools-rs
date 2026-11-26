use std::path::{Component, Path, PathBuf};

/// Convert a path to POSIX forward-slash representation.
pub(crate) fn to_posix(path: &Path) -> String {
    let raw = path.to_string_lossy();
    if cfg!(windows) {
        raw.replace('\\', "/")
    } else {
        raw.into_owned()
    }
}

/// Strip the Windows `\\?\` verbatim prefix that `fs::canonicalize` produces.
pub(crate) fn strip_verbatim(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{unc}"));
        }
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    path
}

/// Normalize dot components lexically without touching the filesystem.
pub(crate) fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                } else if !out.has_root() {
                    out.push(comp);
                }
            }
            _ => out.push(comp),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

/// Resolve a path like Python's `Path.resolve()`, without `\\?\` on Windows.
pub(crate) fn resolve_path(path: &Path) -> PathBuf {
    if let Ok(canon) = std::fs::canonicalize(path) {
        return strip_verbatim(canon);
    }
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    strip_verbatim(lexical_normalize(&abs))
}
