//! `T3ProjectFileLoader.ts`: the checked-in `t3.json` at a workspace root, decoded like
//! `T3ProjectFileFromJson` (lenient JSONC, then the `T3ProjectFile` schema: trimmed non-empty
//! strings, at most 512 characters for `iconPath`, at most 50 scripts).
//!
//! Loading never fails: a missing file is `None`, and an unreadable or invalid one is logged
//! and treated as absent.

use std::path::Path;

use serde_json::Value;
use zc_contracts::T3ProjectFile;
use zc_core::defect::js_length;

/// `T3_PROJECT_FILE_NAME`.
pub const T3_PROJECT_FILE_NAME: &str = "t3.json";
const PATH_MAX_LENGTH: usize = 512;
const MAX_SCRIPTS: usize = 50;

/// The `trimmedNonEmpty` codec: the raw string is non-empty (and short enough), and so is its
/// trimmed value, which is what decoding yields.
fn trimmed_non_empty(field: &str, value: &str, max: Option<usize>) -> Result<String, String> {
    let check = |text: &str| -> Result<(), String> {
        if text.is_empty() {
            return Err(format!("{field}: Expected a value with a length of at least 1"));
        }
        if let Some(max) = max {
            if js_length(text) > max {
                return Err(format!("{field}: Expected a value with a length of at most {max}"));
            }
        }
        Ok(())
    };
    check(value)?;
    let trimmed = value.trim();
    check(trimmed)?;
    Ok(trimmed.to_owned())
}

/// `optionalKey` fields refuse an explicit `null`.
fn no_null_keys(object: &serde_json::Map<String, Value>, keys: &[&str], at: &str) -> Result<(), String> {
    match keys.iter().find(|key| object.get(**key).is_some_and(Value::is_null)) {
        Some(key) => Err(format!("{at}{key}: Expected a value, got null")),
        None => Ok(()),
    }
}

/// `Schema.decode(T3ProjectFileFromJson)`.
pub fn decode_t3_project_file(raw: &str) -> Result<T3ProjectFile, String> {
    let value = zc_core::parse_lenient_json(raw).map_err(|error| error.to_string())?;
    let Value::Object(object) = &value else {
        return Err("Expected an object".into());
    };
    no_null_keys(object, &["$schema", "iconPath", "defaultThreadEnvMode", "worktreeSubmodules", "scripts"], "")?;
    if let Some(Value::Array(scripts)) = object.get("scripts") {
        if scripts.len() > MAX_SCRIPTS {
            return Err(format!("scripts: Expected an array of at most {MAX_SCRIPTS} items"));
        }
        for (index, script) in scripts.iter().enumerate() {
            if let Value::Object(script) = script {
                no_null_keys(
                    script,
                    &["icon", "runOnWorktreeCreate", "async", "previewUrl", "autoOpenPreview"],
                    &format!("scripts[{index}]."),
                )?;
            }
        }
    }
    let mut file: T3ProjectFile = serde_json::from_value(value).map_err(|error| error.to_string())?;
    if let Some(icon_path) = &file.icon_path {
        file.icon_path = Some(trimmed_non_empty("iconPath", icon_path, Some(PATH_MAX_LENGTH))?);
    }
    if let Some(scripts) = &mut file.scripts {
        for (index, script) in scripts.iter_mut().enumerate() {
            script.name = trimmed_non_empty(&format!("scripts[{index}].name"), &script.name, None)?;
            script.command = trimmed_non_empty(&format!("scripts[{index}].command"), &script.command, None)?;
            if let Some(preview_url) = &script.preview_url {
                script.preview_url = Some(trimmed_non_empty(&format!("scripts[{index}].previewUrl"), preview_url, None)?);
            }
        }
    }
    Ok(file)
}

/// `T3ProjectFileLoader.load` (blocking).
pub fn load_t3_project_file(workspace_root: &str) -> Option<T3ProjectFile> {
    let file_path = Path::new(workspace_root).join(T3_PROJECT_FILE_NAME);
    let raw = match std::fs::read(&file_path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::warn!(operation = "read", workspace_root, file_path = %file_path.display(), %error, "Failed to read t3.json");
            return None;
        }
    };
    match decode_t3_project_file(&raw) {
        Ok(file) => Some(file),
        Err(error) => {
            tracing::warn!(operation = "decode", workspace_root, file_path = %file_path.display(), %error, "Failed to decode t3.json");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    //! `T3ProjectFileLoader.test.ts`.
    use super::*;

    fn load_with(contents: Option<&str>) -> Option<T3ProjectFile> {
        let dir = tempfile::tempdir().unwrap();
        if let Some(contents) = contents {
            std::fs::write(dir.path().join("t3.json"), contents).unwrap();
        }
        load_t3_project_file(&dir.path().to_string_lossy())
    }

    #[test]
    fn loads_and_decodes_a_valid_t3_json() {
        let loaded = load_with(Some(
            r#"{
            // JSONC is tolerated
            "iconPath": "assets/logo.svg",
            "scripts": [{ "name": "Dev", "command": "pnpm dev" }],
          }"#,
        ))
        .expect("decoded");
        assert_eq!(loaded.icon_path.as_deref(), Some("assets/logo.svg"));
        let scripts = loaded.scripts.unwrap();
        assert_eq!(scripts.len(), 1);
        assert_eq!((scripts[0].name.as_str(), scripts[0].command.as_str()), ("Dev", "pnpm dev"));
        assert_eq!(
            serde_json::to_value(&scripts[0]).unwrap(),
            serde_json::json!({"name": "Dev", "command": "pnpm dev"})
        );
    }

    #[test]
    fn returns_none_when_missing() {
        assert!(load_with(None).is_none());
    }

    #[test]
    fn returns_none_for_malformed_json() {
        assert!(load_with(Some("{ not json")).is_none());
    }

    #[test]
    fn returns_none_for_schema_invalid_files() {
        assert!(load_with(Some(r#"{ "scripts": [{ "name": "Dev" }] }"#)).is_none());
        assert!(load_with(Some(r#"{ "iconPath": "   " }"#)).is_none());
        assert!(load_with(Some(r#"{ "iconPath": null }"#)).is_none());
        assert!(load_with(Some("[]")).is_none());
    }

    #[test]
    fn trims_strings() {
        let file = decode_t3_project_file(r#"{"iconPath": "  brand/mark.svg "}"#).unwrap();
        assert_eq!(file.icon_path.as_deref(), Some("brand/mark.svg"));
    }
}
