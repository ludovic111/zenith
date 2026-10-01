//! The host platform as Node names it (`process.platform`), which the TS threads through
//! `HostProcessPlatform` and reports in errors (`"darwin"`, `"linux"`, `"win32"`).

/// `NodeJS.Platform`, reduced to what the workspace and launcher code distinguishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodePlatform {
    Darwin,
    Linux,
    Win32,
    /// Any other Unix (`freebsd`, …): treated like Linux without the Linux-only probes.
    Other(&'static str),
}

impl NodePlatform {
    /// The platform this process runs on.
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Darwin
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else if cfg!(windows) {
            Self::Win32
        } else if cfg!(target_os = "freebsd") {
            Self::Other("freebsd")
        } else if cfg!(target_os = "openbsd") {
            Self::Other("openbsd")
        } else {
            Self::Other("unknown")
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Darwin => "darwin",
            Self::Linux => "linux",
            Self::Win32 => "win32",
            Self::Other(name) => name,
        }
    }
}

/// `isWindowsDrivePath(value) || isUncPath(value)` (shared `path.ts`).
pub fn is_windows_absolute_path(value: &str) -> bool {
    if value.starts_with("\\\\") {
        return true;
    }
    let bytes = value.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && (bytes.len() == 2 || bytes[2] == b'/' || bytes[2] == b'\\')
}

/// `isExplicitRelativePath` (shared `path.ts`).
pub fn is_explicit_relative_path(value: &str) -> bool {
    value == "." || value == ".." || value.starts_with("./") || value.starts_with("../") || value.starts_with(".\\") || value.starts_with("..\\")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_shapes() {
        assert!(is_windows_absolute_path("C:\\Users"));
        assert!(is_windows_absolute_path("c:/x"));
        assert!(is_windows_absolute_path("D:"));
        assert!(is_windows_absolute_path("\\\\server\\share"));
        assert!(!is_windows_absolute_path("/home"));
        assert!(!is_windows_absolute_path("C:x"));
        assert!(is_explicit_relative_path("./src"));
        assert!(is_explicit_relative_path(".."));
        assert!(!is_explicit_relative_path(".config"));
        assert!(!is_explicit_relative_path("src"));
    }
}
