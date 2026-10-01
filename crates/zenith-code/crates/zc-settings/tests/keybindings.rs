//! Port of `apps/server/src/keybindings.test.ts` and `packages/contracts/src/keybindings.test.ts`
//! (the `when` parser cases are unit tests of `keybindings::when`).

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};
use zc_contracts::{ResolvedKeybindingRule, ResolvedKeybindingsConfig, ServerConfigIssue, ServerUpsertKeybindingInput};
use zc_settings::keybindings::rules::{decode_keybinding_rule, encode_resolved_rule};
use zc_settings::keybindings::{
    compile_resolved_keybinding_rule, compile_resolved_keybindings_config, default_keybindings, keybindings_config_json, parse_keybinding_shortcut,
    KeybindingRule, KeybindingsService,
};

struct Fixture {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("userdata");
        std::fs::create_dir_all(&state_dir).unwrap();
        Self {
            path: state_dir.join("keybindings.json"),
            _dir: dir,
        }
    }

    fn service(&self) -> KeybindingsService {
        KeybindingsService::new(&self.path)
    }

    fn write_rules(&self, rules: &[(&str, &str)]) {
        let rules: Vec<KeybindingRule> = rules.iter().map(|(key, command)| KeybindingRule::new(key, command, None)).collect();
        std::fs::write(&self.path, keybindings_config_json(&rules).unwrap()).unwrap();
    }

    fn write(&self, contents: &str) {
        std::fs::write(&self.path, contents).unwrap();
    }

    fn read(&self) -> String {
        std::fs::read_to_string(&self.path).unwrap()
    }

    /// `[{key, command}]` of the file.
    fn persisted_view(&self) -> Vec<(String, String)> {
        let value: Value = serde_json::from_str(&self.read()).unwrap();
        value
            .as_array()
            .unwrap()
            .iter()
            .map(|rule| (rule["key"].as_str().unwrap().to_owned(), rule["command"].as_str().unwrap().to_owned()))
            .collect()
    }
}

fn upsert(key: &str, command: &str, replace: Option<(&str, &str)>) -> ServerUpsertKeybindingInput {
    let mut input = json!({"key": key, "command": command});
    if let Some((key, command)) = replace {
        input["replace"] = json!({"key": key, "command": command});
    }
    serde_json::from_value(input).unwrap()
}

fn commands(rules: &[ResolvedKeybindingRule]) -> Vec<String> {
    rules
        .iter()
        .map(|rule| serde_json::to_value(&rule.command).unwrap().as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn parses_shortcuts_including_plus_key() {
    let shortcut = |key: &str| json!({"key": key, "metaKey": false, "ctrlKey": false, "shiftKey": false, "altKey": false, "modKey": true});
    assert_eq!(serde_json::to_value(parse_keybinding_shortcut("mod+j").unwrap()).unwrap(), shortcut("j"));
    assert_eq!(serde_json::to_value(parse_keybinding_shortcut("mod++").unwrap()).unwrap(), shortcut("+"));
}

#[test]
fn compiles_valid_rule_with_parsed_when_ast() {
    let compiled = compile_resolved_keybinding_rule(&KeybindingRule::new("mod+d", "terminal.split", Some("terminalOpen && !terminalFocus"))).unwrap();
    assert_eq!(
        serde_json::to_value(compiled).unwrap(),
        json!({
            "command": "terminal.split",
            "shortcut": {"key": "d", "metaKey": false, "ctrlKey": false, "shiftKey": false, "altKey": false, "modKey": true},
            "whenAst": {"type": "and", "left": {"type": "identifier", "name": "terminalOpen"}, "right": {"type": "not", "node": {"type": "identifier", "name": "terminalFocus"}}}
        })
    );
}

#[test]
fn encodes_resolved_plus_key_shortcuts() {
    let resolved: ResolvedKeybindingRule = serde_json::from_value(json!({
        "command": "terminal.toggle",
        "shortcut": {"key": "+", "metaKey": false, "ctrlKey": false, "shiftKey": false, "altKey": false, "modKey": true}
    }))
    .unwrap();
    let encoded = encode_resolved_rule(&resolved).unwrap();
    assert_eq!(encoded.key, "mod++");
    assert_eq!(encoded.command, "terminal.toggle");
}

#[test]
fn rejects_invalid_rules() {
    assert!(compile_resolved_keybinding_rule(&KeybindingRule::new("mod+shift+d+o", "terminal.new", None)).is_none());
    assert!(compile_resolved_keybinding_rule(&KeybindingRule::new("mod+d", "terminal.split", Some("terminalFocus && ("))).is_none());
    let deep = format!("{}terminalFocus", "!".repeat(300));
    assert!(compile_resolved_keybinding_rule(&KeybindingRule::new("mod+d", "terminal.split", Some(&deep))).is_none());
}

#[test]
fn formats_invalid_resolved_keybinding_rules_with_the_custom_message() {
    let detail = zc_settings::keybindings::rules::resolve_keybinding_rule(&KeybindingRule::new("mod+shift+d+o", "terminal.new", None)).unwrap_err();
    assert!(detail.contains("Invalid keybinding rule"));
    assert!(!detail.contains("Invalid data"));
}

#[tokio::test]
async fn bootstraps_default_keybindings_when_config_file_is_missing() {
    let fixture = Fixture::new();
    assert!(!fixture.path.exists());
    fixture.service().sync_default_keybindings_on_startup().await.unwrap();
    let persisted: Value = serde_json::from_str(&fixture.read()).unwrap();
    let defaults: Vec<Value> = default_keybindings().iter().map(KeybindingRule::to_json).collect();
    assert_eq!(persisted, Value::Array(defaults));
}

#[tokio::test]
async fn uses_defaults_in_runtime_when_config_is_malformed_without_overriding_file() {
    let fixture = Fixture::new();
    fixture.write("{ not-json");
    let state = fixture.service().load_config_state().await.unwrap();
    assert_eq!(state.keybindings, compile_resolved_keybindings_config(&default_keybindings()));
    assert_eq!(
        serde_json::to_value(&state.issues).unwrap(),
        json!([{"kind": "keybindings.malformed-config", "message": "expected JSON array (SchemaError: Expected a valid JSON string)"}])
    );
    assert_eq!(fixture.read(), "{ not-json");
}

#[tokio::test]
async fn ignores_invalid_entries_in_runtime_and_reports_them_as_issues() {
    let fixture = Fixture::new();
    fixture
        .write(r#"[{"key":"mod+j","command":"terminal.toggle"},{"key":"mod+shift+d+o","command":"terminal.new"},{"key":"mod+x","command":"invalid.command"}]"#);
    let state = fixture.service().load_config_state().await.unwrap();
    let commands = commands(&state.keybindings);
    assert!(commands.contains(&"terminal.toggle".to_owned()));
    assert!(!commands.contains(&"invalid.command".to_owned()));
    let indexes: Vec<i64> = state
        .issues
        .iter()
        .map(|issue| match issue {
            ServerConfigIssue::KeybindingsInvalidEntry(entry) => entry.index.get() as i64,
            ServerConfigIssue::KeybindingsMalformedConfig(_) => -1,
        })
        .collect();
    assert_eq!(indexes, [1, 2]);
    let messages: Vec<String> = state
        .issues
        .iter()
        .map(|issue| serde_json::to_value(issue).unwrap()["message"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(messages[0], "SchemaError: Invalid keybinding rule");
    assert!(messages[1].starts_with("SchemaError: Expected \"sidebar.toggle\""));
}

#[tokio::test]
async fn upserts_missing_default_keybindings_on_startup_without_overriding_existing_command_rules() {
    let fixture = Fixture::new();
    fixture.write_rules(&[("mod+shift+t", "terminal.toggle"), ("mod+shift+r", "script.run-tests.run")]);
    fixture.service().sync_default_keybindings_on_startup().await.unwrap();
    let persisted = fixture.persisted_view();
    let toggles: Vec<&(String, String)> = persisted.iter().filter(|(_, command)| command == "terminal.toggle").collect();
    assert_eq!(toggles, [&("mod+shift+t".to_owned(), "terminal.toggle".to_owned())]);
    let persisted_commands: HashSet<&str> = persisted.iter().map(|(_, command)| command.as_str()).collect();
    for default in default_keybindings() {
        assert!(persisted_commands.contains(default.command.as_str()), "expected {}", default.command);
    }
    assert!(persisted_commands.contains("script.run-tests.run"));
}

#[tokio::test]
async fn skips_conflicting_default_keybindings_on_startup() {
    let fixture = Fixture::new();
    fixture.write_rules(&[("mod+j", "script.custom-action.run")]);
    fixture.service().sync_default_keybindings_on_startup().await.unwrap();
    let persisted = fixture.persisted_view();
    assert!(!persisted.iter().any(|(_, command)| command == "terminal.toggle"));
    assert!(persisted.iter().any(|(_, command)| command == "script.custom-action.run"));
}

#[tokio::test]
async fn upserts_custom_keybindings_to_configured_path() {
    let fixture = Fixture::new();
    fixture.write_rules(&[("mod+j", "terminal.toggle")]);
    let resolved = fixture
        .service()
        .upsert_keybinding_rule(&upsert("mod+shift+r", "script.run-tests.run", None))
        .await
        .unwrap();
    assert_eq!(
        fixture.persisted_view(),
        [
            ("mod+j".into(), "terminal.toggle".into()),
            ("mod+shift+r".into(), "script.run-tests.run".into())
        ]
    );
    assert!(commands(&resolved).contains(&"script.run-tests.run".to_owned()));
}

#[tokio::test]
async fn appends_additional_custom_keybindings_for_the_same_command() {
    let fixture = Fixture::new();
    fixture.write_rules(&[("mod+r", "script.run-tests.run")]);
    fixture
        .service()
        .upsert_keybinding_rule(&upsert("mod+shift+r", "script.run-tests.run", None))
        .await
        .unwrap();
    assert_eq!(
        fixture.persisted_view(),
        [
            ("mod+r".into(), "script.run-tests.run".into()),
            ("mod+shift+r".into(), "script.run-tests.run".into())
        ]
    );
}

#[tokio::test]
async fn replaces_only_the_targeted_custom_keybinding() {
    let fixture = Fixture::new();
    fixture.write_rules(&[("mod+r", "script.run-tests.run"), ("mod+shift+r", "script.run-tests.run")]);
    fixture
        .service()
        .upsert_keybinding_rule(&upsert("mod+alt+r", "script.run-tests.run", Some(("mod+r", "script.run-tests.run"))))
        .await
        .unwrap();
    assert_eq!(
        fixture.persisted_view(),
        [
            ("mod+shift+r".into(), "script.run-tests.run".into()),
            ("mod+alt+r".into(), "script.run-tests.run".into())
        ]
    );
}

#[tokio::test]
async fn replacing_with_a_rule_that_already_exists_elsewhere_does_not_duplicate_it() {
    let fixture = Fixture::new();
    fixture.write_rules(&[("mod+r", "script.run-tests.run"), ("mod+alt+r", "script.run-tests.run")]);
    fixture
        .service()
        .upsert_keybinding_rule(&upsert("mod+alt+r", "script.run-tests.run", Some(("mod+r", "script.run-tests.run"))))
        .await
        .unwrap();
    assert_eq!(fixture.persisted_view(), [("mod+alt+r".into(), "script.run-tests.run".into())]);
}

#[tokio::test]
async fn removes_only_the_targeted_custom_keybinding() {
    let fixture = Fixture::new();
    fixture.write_rules(&[("mod+r", "script.run-tests.run"), ("mod+shift+r", "script.run-tests.run")]);
    fixture
        .service()
        .remove_keybinding_rule(&serde_json::from_value(json!({"key": "mod+r", "command": "script.run-tests.run"})).unwrap())
        .await
        .unwrap();
    assert_eq!(fixture.persisted_view(), [("mod+shift+r".into(), "script.run-tests.run".into())]);
}

#[tokio::test]
async fn refuses_to_overwrite_malformed_keybindings_config() {
    let fixture = Fixture::new();
    fixture.write("{ not-json");
    let error = fixture
        .service()
        .upsert_keybinding_rule(&upsert("mod+shift+r", "script.run-tests.run", None))
        .await
        .unwrap_err();
    assert_eq!(error.detail, "expected JSON array");
    assert_eq!(fixture.read(), "{ not-json");
}

#[tokio::test]
async fn reports_non_array_config_parse_errors_without_duplicate_prefix() {
    let fixture = Fixture::new();
    fixture.write(r#"{"key":"mod+j","command":"terminal.toggle"}"#);
    let service = fixture.service();
    for _ in 0..2 {
        let error = service
            .upsert_keybinding_rule(&upsert("mod+shift+r", "script.run-tests.run", None))
            .await
            .unwrap_err();
        assert_eq!(error.detail, "expected JSON array");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn fails_when_config_directory_is_not_writable() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    fixture.write_rules(&[("mod+j", "terminal.toggle")]);
    let directory = fixture.path.parent().unwrap().to_path_buf();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o500)).unwrap();
    let result = fixture
        .service()
        .upsert_keybinding_rule(&upsert("mod+shift+r", "script.run-tests.run", None))
        .await;
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(result.unwrap_err().detail, "failed to write keybindings config");
    assert_eq!(fixture.persisted_view(), [("mod+j".into(), "terminal.toggle".into())]);
}

#[tokio::test]
async fn caches_loaded_resolved_config_across_repeated_reads() {
    let fixture = Fixture::new();
    fixture.write_rules(&[("mod+j", "terminal.toggle")]);
    let service = fixture.service();
    let first = service.load_config_state().await.unwrap().keybindings;
    std::fs::remove_file(&fixture.path).unwrap();
    let second = service.load_config_state().await.unwrap().keybindings;
    assert_eq!(first, second);
    assert!(commands(&second).contains(&"terminal.toggle".to_owned()));
}

#[tokio::test]
async fn updates_cached_resolved_config_after_upsert() {
    let fixture = Fixture::new();
    fixture.write_rules(&[("mod+j", "terminal.toggle")]);
    let service = fixture.service();
    service.load_config_state().await.unwrap();
    service
        .upsert_keybinding_rule(&upsert("mod+shift+r", "script.run-tests.run", None))
        .await
        .unwrap();
    let loaded = commands(&service.load_config_state().await.unwrap().keybindings);
    assert!(loaded.contains(&"script.run-tests.run".to_owned()));
    assert!(loaded.contains(&"terminal.toggle".to_owned()));
}

#[tokio::test]
async fn serializes_concurrent_upserts_to_avoid_lost_updates() {
    let fixture = Fixture::new();
    fixture.write_rules(&[]);
    let service = fixture.service();
    let commands: Vec<String> = (0..20).map(|index| format!("script.concurrent-{index}.run")).collect();
    let tasks: Vec<_> = commands
        .iter()
        .enumerate()
        .map(|(index, command)| {
            let service = service.clone();
            let input = upsert(&format!("mod+{}", (b'a' + index as u8) as char), command, None);
            tokio::spawn(async move { service.upsert_keybinding_rule(&input).await.unwrap() })
        })
        .collect();
    for task in tasks {
        task.await.unwrap();
    }
    let persisted: HashSet<String> = fixture.persisted_view().into_iter().map(|(_, command)| command).collect();
    for command in &commands {
        assert!(persisted.contains(command), "expected persisted command {command}");
    }
}

#[tokio::test]
async fn caps_upserts_at_the_newest_256_rules() {
    let fixture = Fixture::new();
    let rules: Vec<(String, String)> = (0..256).map(|i| (format!("mod+{i}"), format!("script.s{i}.run"))).collect();
    let refs: Vec<(&str, &str)> = rules.iter().map(|(k, c)| (k.as_str(), c.as_str())).collect();
    fixture.write_rules(&refs);
    fixture
        .service()
        .upsert_keybinding_rule(&upsert("mod+shift+x", "chat.new", None))
        .await
        .unwrap();
    let persisted = fixture.persisted_view();
    assert_eq!(persisted.len(), 256);
    assert_eq!(persisted.first().unwrap().1, "script.s1.run");
    assert_eq!(persisted.last().unwrap().1, "chat.new");
}

#[tokio::test]
async fn start_backfills_and_watches_the_file() {
    let fixture = Fixture::new();
    let service = fixture.service();
    service.start().await.unwrap();
    service.ready().await.unwrap();
    assert_eq!(fixture.persisted_view().len(), default_keybindings().len());
    let mut changes = service.subscribe_changes();
    tokio::time::sleep(Duration::from_millis(200)).await;
    fixture.write_rules(&[("mod+shift+b", "sidebar.toggle")]);
    // The startup write itself may still be reported first; wait for the edit.
    let edited = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(state) = changes.recv().await {
            let sidebar = state
                .keybindings
                .iter()
                .find(|rule| serde_json::to_value(&rule.command).unwrap() == json!("sidebar.toggle"))
                .cloned();
            if sidebar.is_some_and(|rule| rule.shortcut.shift_key) {
                return true;
            }
        }
        false
    })
    .await
    .expect("the edit within 10 s");
    assert!(edited);
}

// ---------------------------------------------------------------------------------------------
// packages/contracts/src/keybindings.test.ts
// ---------------------------------------------------------------------------------------------

#[test]
fn parses_keybinding_rules() {
    for (key, command, when) in [
        ("mod+j", "terminal.toggle", None),
        ("mod+b", "sidebar.toggle", None),
        ("mod+alt+b", "rightPanel.toggle", None),
        ("mod+shift+m", "rightPanel.toggleMaximized", None),
        ("mod+w", "terminal.close", None),
        ("mod+d", "diff.toggle", None),
        ("mod+k", "commandPalette.toggle", None),
        ("mod+p", "filePicker.toggle", None),
        ("mod+shift+f", "projectSearch.toggle", None),
        ("mod+u", "usage.open", None),
        ("mod+alt+shift+t", "themeEditor.toggle", None),
        ("mod+shift+n", "chat.newLocal", None),
        ("mod+shift+m", "modelPicker.toggle", None),
        ("mod+1", "modelPicker.jump.1", None),
        ("mod+shift+[", "thread.previous", None),
        ("mod+shift+s", "thread.settle", Some("!terminalFocus")),
        ("mod+shift+c", "thread.copyReference", Some("!terminalFocus")),
        ("mod+shift+k", "pullRequest.copyNumber", Some("!terminalFocus")),
        ("mod+escape", "thread.stop", None),
    ] {
        let mut input = json!({"key": key, "command": command});
        if let Some(when) = when {
            input["when"] = json!(when);
        }
        assert_eq!(decode_keybinding_rule(&input).unwrap().command, command);
    }
}

#[test]
fn rejects_invalid_command_values_and_accepts_script_commands() {
    assert!(decode_keybinding_rule(&json!({"key": "mod+j", "command": "script.Test.run"})).is_err());
    assert_eq!(
        decode_keybinding_rule(&json!({"key": "mod+r", "command": "script.setup.run"})).unwrap().command,
        "script.setup.run"
    );
}

const SHORTCUT: &str = r#"{"key": "p", "metaKey": false, "ctrlKey": false, "shiftKey": false, "altKey": false, "modKey": true}"#;

fn resolved_config(value: Value) -> Vec<String> {
    let config: ResolvedKeybindingsConfig = serde_json::from_value(value).unwrap();
    commands(&config.0)
}

#[test]
fn resolved_config_drops_unknown_when_nodes_and_malformed_entries() {
    let shortcut: Value = serde_json::from_str(SHORTCUT).unwrap();
    // Unknown commands: the generated `KeybindingCommand` reads any string as a script command
    // (zc-contracts does not check the template pattern), so the server-side check is
    // `is_keybinding_command`, which rejects them.
    assert!(!zc_settings::keybindings::rules::is_keybinding_command("someFuture.toggle"));
    assert_eq!(
        resolved_config(json!([
            {"command": "terminal.toggle", "shortcut": shortcut, "whenAst": {"type": "xor", "left": 1, "right": 2}},
            {"command": "terminal.split", "shortcut": shortcut}
        ])),
        ["terminal.split"]
    );
    assert_eq!(
        resolved_config(json!(["garbage", {"command": "terminal.toggle", "shortcut": shortcut}, null])),
        ["terminal.toggle"]
    );
}

#[test]
fn resolved_rules_encode_to_the_plain_wire_shape_and_drop_unknown_fields() {
    let shortcut: Value = serde_json::from_str(SHORTCUT).unwrap();
    let rule: ResolvedKeybindingRule = serde_json::from_value(json!({"command": "terminal.toggle", "shortcut": shortcut, "key": "mod+j"})).unwrap();
    assert_eq!(
        serde_json::to_value(&rule).unwrap(),
        json!({"command": "terminal.toggle", "shortcut": shortcut})
    );
}
