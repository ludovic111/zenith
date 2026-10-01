//! Home-directory helpers (`pathExpansion.ts`, `os-jank.ts` `expandHomePath`).

use std::path::{Path, PathBuf};

/// The current user's home directory, like Node's `os.homedir()`: `$HOME` when it is set and
/// non-empty, otherwise the passwd entry for the effective uid.
pub fn home_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(home);
    }
    passwd_home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

/// The home directory from the passwd database (Node's `os.userInfo().homedir`), ignoring `$HOME`.
#[cfg(unix)]
pub fn passwd_home_dir() -> Option<PathBuf> {
    passwd_field(|entry| entry.pw_dir)
}

#[cfg(not(unix))]
pub fn passwd_home_dir() -> Option<PathBuf> {
    None
}

/// The user's login shell from the passwd database (Node's `os.userInfo().shell`).
#[cfg(unix)]
pub fn passwd_shell() -> Option<String> {
    passwd_field(|entry| entry.pw_shell).and_then(|path| path.to_str().map(str::to_owned))
}

#[cfg(not(unix))]
pub fn passwd_shell() -> Option<String> {
    None
}

#[cfg(unix)]
fn passwd_field(field: impl Fn(&libc::passwd) -> *mut libc::c_char) -> Option<PathBuf> {
    use std::ffi::CStr;
    use std::os::unix::ffi::OsStrExt;

    let mut buffer = vec![0 as libc::c_char; 16 * 1024];
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: getpwuid_r writes into `entry` and `buffer`, both owned and sized here.
    let status = unsafe { libc::getpwuid_r(libc::geteuid(), &mut entry, buffer.as_mut_ptr(), buffer.len(), &mut result) };
    if status != 0 || result.is_null() {
        return None;
    }
    let pointer = field(&entry);
    if pointer.is_null() {
        return None;
    }
    // SAFETY: the pointer refers into `buffer`, which outlives this read.
    let bytes = unsafe { CStr::from_ptr(pointer) }.to_bytes();
    if bytes.is_empty() {
        return None;
    }
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
}

/// Expand a leading `~` (or `~/…`, `~\…`) to the home directory. Anything else, including an
/// empty string and `~user`, is returned unchanged.
pub fn expand_home_path(value: &str) -> PathBuf {
    expand_home_path_with(value, &home_dir())
}

/// [`expand_home_path`] against an explicit home directory (for tests and embedders).
pub fn expand_home_path_with(value: &str, home: &Path) -> PathBuf {
    if value == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = value.strip_prefix("~/").or_else(|| value.strip_prefix("~\\")) {
        return home.join(rest);
    }
    PathBuf::from(value)
}

/// `path.resolve(value)` for a single argument: absolute paths are normalized, relative ones are
/// joined onto the current directory and normalized. `.` and `..` segments are folded lexically,
/// without touching the file system, exactly like Node.
pub fn resolve_path(value: &Path) -> PathBuf {
    let joined = if value.is_absolute() {
        value.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")).join(value)
    };
    normalize_lexically(&joined)
}

/// Fold `.` and `..` segments without resolving symlinks (Node's `path.normalize` for absolute
/// paths). `..` above the root stays at the root.
pub fn normalize_lexically(path: &Path) -> PathBuf {
    use std::path::Component;
    let absolute = path.is_absolute();
    let mut out = PathBuf::new();
    let mut parts: Vec<&std::ffi::OsStr> = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => out.push(component),
            Component::CurDir => {}
            Component::ParentDir => {
                if parts.last().is_some_and(|last| *last != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push(component.as_os_str());
                }
            }
            Component::Normal(segment) => parts.push(segment),
        }
    }
    for part in parts {
        out.push(part);
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_only_a_leading_tilde() {
        let home = Path::new("/home/ada");
        assert_eq!(expand_home_path_with("~", home), PathBuf::from("/home/ada"));
        assert_eq!(expand_home_path_with("~/x/y", home), PathBuf::from("/home/ada/x/y"));
        assert_eq!(expand_home_path_with("~\\x", home), PathBuf::from("/home/ada/x"));
        assert_eq!(expand_home_path_with("~bob/x", home), PathBuf::from("~bob/x"));
        assert_eq!(expand_home_path_with("/abs/~/x", home), PathBuf::from("/abs/~/x"));
        assert_eq!(expand_home_path_with("", home), PathBuf::from(""));
    }

    #[test]
    fn normalizes_like_node_path_resolve() {
        assert_eq!(normalize_lexically(Path::new("/a/./b/../c/")), PathBuf::from("/a/c"));
        assert_eq!(normalize_lexically(Path::new("/../..")), PathBuf::from("/"));
        assert!(resolve_path(Path::new("rel")).is_absolute());
    }
}
