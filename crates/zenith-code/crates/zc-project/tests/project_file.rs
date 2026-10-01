//! Port of `project/T3ProjectFileLoader.test.ts`.

use zc_project::T3ProjectFileLoader;

fn write(dir: &std::path::Path, contents: &str) {
    std::fs::write(dir.join("t3.json"), contents).unwrap();
}

#[tokio::test]
async fn loads_and_decodes_a_valid_t3_json() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        r#"{
            // JSONC is tolerated
            "iconPath": "assets/logo.svg",
            "scripts": [{ "name": "  Dev ", "command": "pnpm dev" }],
          }"#,
    );
    let loaded = T3ProjectFileLoader.load(dir.path()).await.unwrap();
    assert_eq!(loaded.icon_path.as_deref(), Some("assets/logo.svg"));
    let scripts = loaded.scripts.unwrap();
    assert_eq!(scripts.len(), 1);
    assert_eq!((scripts[0].name.as_str(), scripts[0].command.as_str()), ("Dev", "pnpm dev"));
    assert_eq!(
        serde_json::to_value(&scripts[0]).unwrap(),
        serde_json::json!({"name": "Dev", "command": "pnpm dev"})
    );
}

#[tokio::test]
async fn returns_none_when_t3_json_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    assert!(T3ProjectFileLoader.load(dir.path()).await.is_none());
}

#[tokio::test]
async fn returns_none_for_malformed_json_without_failing() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "{ not json");
    assert!(T3ProjectFileLoader.load(dir.path()).await.is_none());
}

#[tokio::test]
async fn returns_none_for_schema_invalid_files_without_failing() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), r#"{ "scripts": [{ "name": "Dev" }] }"#);
    assert!(T3ProjectFileLoader.load(dir.path()).await.is_none());
    write(dir.path(), r#"{ "worktreeSubmodules": "sometimes" }"#);
    assert!(T3ProjectFileLoader.load(dir.path()).await.is_none());
    write(dir.path(), r#"{ "iconPath": "   " }"#);
    assert!(T3ProjectFileLoader.load(dir.path()).await.is_none());
}

#[tokio::test]
async fn keeps_every_documented_field() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        r#"{"$schema": "https://t3.codes/schema/t3.json", "defaultThreadEnvMode": "worktree", "worktreeSubmodules": "top-level",
            "scripts": [{"name": "Setup", "command": "npm ci", "icon": "configure", "runOnWorktreeCreate": true, "async": false,
                         "previewUrl": " http://localhost:3000 ", "autoOpenPreview": true}]}"#,
    );
    let loaded = T3ProjectFileLoader.load(dir.path()).await.unwrap();
    assert_eq!(
        serde_json::to_value(&loaded).unwrap(),
        serde_json::json!({
            "$schema": "https://t3.codes/schema/t3.json", "defaultThreadEnvMode": "worktree", "worktreeSubmodules": "top-level",
            "scripts": [{"name": "Setup", "command": "npm ci", "icon": "configure", "runOnWorktreeCreate": true, "async": false,
                         "previewUrl": "http://localhost:3000", "autoOpenPreview": true}]
        })
    );
}
