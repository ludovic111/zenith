//! The slice of `t3.json` (`contracts/t3ProjectFile.ts`, `shared/t3ProjectFile.ts`) the git
//! driver needs: `worktreeSubmodules`, read from a freshly created worktree.
//!
//! `parseT3ProjectFile` decodes the *whole* file and treats any invalid field as "no file",
//! so this validates every field of the schema, not only the one it returns. zc-project
//! (WP-25) owns the full typed file; this module can then call it instead.

use serde_json::{Map, Value};

use zc_ports::contracts::WorktreeSubmodules;

/// `T3_PROJECT_FILE_NAME`.
pub const T3_PROJECT_FILE_NAME: &str = "t3.json";
const PATH_MAX_LENGTH: usize = 512;
const MAX_SCRIPTS: usize = 50;

/// What the driver reads from a valid `t3.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct T3ProjectFileSlice {
    pub worktree_submodules: Option<WorktreeSubmodules>,
}

fn trimmed_non_empty(value: &Value, max: Option<usize>) -> bool {
    let Some(text) = value.as_str() else {
        return false;
    };
    let length = |s: &str| s.encode_utf16().count();
    if text.is_empty() || max.is_some_and(|max| length(text) > max) {
        return false;
    }
    !text.trim().is_empty()
}

fn literal(value: &Value, allowed: &[&str]) -> bool {
    value.as_str().is_some_and(|text| allowed.contains(&text))
}

fn valid_script(value: &Value) -> bool {
    let Some(script) = value.as_object() else {
        return false;
    };
    let required = |key: &str| script.get(key).is_some_and(|v| trimmed_non_empty(v, None));
    let optional = |key: &str, check: &dyn Fn(&Value) -> bool| script.get(key).is_none_or(check);
    required("name")
        && required("command")
        && optional("icon", &|v| literal(v, &["play", "test", "lint", "configure", "build", "debug"]))
        && optional("runOnWorktreeCreate", &Value::is_boolean)
        && optional("async", &Value::is_boolean)
        && optional("previewUrl", &|v| trimmed_non_empty(v, None))
        && optional("autoOpenPreview", &Value::is_boolean)
}

fn valid_file(file: &Map<String, Value>) -> bool {
    let optional = |key: &str, check: &dyn Fn(&Value) -> bool| file.get(key).is_none_or(check);
    optional("$schema", &Value::is_string)
        && optional("iconPath", &|v| trimmed_non_empty(v, Some(PATH_MAX_LENGTH)))
        && optional("defaultThreadEnvMode", &|v| literal(v, &["local", "worktree"]))
        && optional("worktreeSubmodules", &|v| literal(v, &["recursive", "top-level", "none"]))
        && optional("scripts", &|v| {
            v.as_array()
                .is_some_and(|scripts| scripts.len() <= MAX_SCRIPTS && scripts.iter().all(valid_script))
        })
}

/// `parseT3ProjectFile(contents)`: `None` for an invalid or malformed file.
pub fn parse_t3_project_file(contents: &str) -> Option<T3ProjectFileSlice> {
    let value = zc_core::lenient_json::parse_lenient_json(contents).ok()?;
    let file = value.as_object()?;
    if !valid_file(file) {
        return None;
    }
    let worktree_submodules = file.get("worktreeSubmodules").and_then(|v| serde_json::from_value(v.clone()).ok());
    Some(T3ProjectFileSlice { worktree_submodules })
}

/// Where a resolved setting came from (`ProjectSettingSource`, the two values the driver uses).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingSource {
    Environment,
    T3Json,
}

/// `resolveProjectFileBackedSetting("worktreeSubmodules", setting, projectFile)`: the setting,
/// else the file, else the built-in `"recursive"`.
pub fn resolve_worktree_submodules(setting: Option<WorktreeSubmodules>, project_file: Option<T3ProjectFileSlice>) -> (WorktreeSubmodules, SettingSource) {
    if let Some(setting) = setting {
        return (setting, SettingSource::Environment);
    }
    match project_file.and_then(|file| file.worktree_submodules) {
        Some(value) => (value, SettingSource::T3Json),
        None => (WorktreeSubmodules::Recursive, SettingSource::Environment),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_worktree_submodules_from_a_valid_file() {
        let file = parse_t3_project_file(
            r#"{
                // comments are fine
                "worktreeSubmodules": "top-level",
                "scripts": [{"name": "Dev", "command": "npm run dev", "icon": "play"}],
            }"#,
        )
        .unwrap();
        assert_eq!(file.worktree_submodules, Some(WorktreeSubmodules::TopLevel));
        assert_eq!(
            resolve_worktree_submodules(None, Some(file)),
            (WorktreeSubmodules::TopLevel, SettingSource::T3Json)
        );
        assert_eq!(
            resolve_worktree_submodules(Some(WorktreeSubmodules::None), Some(file)),
            (WorktreeSubmodules::None, SettingSource::Environment)
        );
    }

    #[test]
    fn rejects_files_with_any_invalid_field() {
        assert!(parse_t3_project_file(r#"{"worktreeSubmodules": "all"}"#).is_none());
        assert!(parse_t3_project_file(r#"{"worktreeSubmodules": "none", "scripts": [{"name": " ", "command": "x"}]}"#).is_none());
        assert!(parse_t3_project_file("[]").is_none());
        assert!(parse_t3_project_file("{").is_none());
        assert_eq!(
            resolve_worktree_submodules(None, None),
            (WorktreeSubmodules::Recursive, SettingSource::Environment)
        );
    }
}
