//! Keybinding rules: the defaults, shortcut parsing/encoding, compilation into resolved rules,
//! merging with the defaults (`packages/shared/src/keybindings.ts`), and the `KeybindingRule`
//! schema checks with the TS error texts (`packages/contracts/src/keybindings.ts`).

use serde_json::{json, Value};
use zc_contracts::{KeybindingCommand, KeybindingShortcut, ResolvedKeybindingRule};
use zc_core::defect::js_length;

use super::when::{encode_when_ast, parse_keybinding_when_expression};
use crate::js::js_trim;

/// `MAX_KEYBINDINGS_COUNT`.
pub const MAX_KEYBINDINGS_COUNT: usize = 256;
/// `MAX_KEYBINDING_VALUE_LENGTH`.
pub const MAX_KEYBINDING_VALUE_LENGTH: usize = 64;
/// `MAX_KEYBINDING_WHEN_LENGTH`.
pub const MAX_KEYBINDING_WHEN_LENGTH: usize = 256;
/// `MAX_SCRIPT_ID_LENGTH`.
pub const MAX_SCRIPT_ID_LENGTH: usize = 24;

/// A decoded `KeybindingRule` (`key` and `when` trimmed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingRule {
    pub key: String,
    pub command: String,
    pub when: Option<String>,
}

impl KeybindingRule {
    pub fn new(key: &str, command: &str, when: Option<&str>) -> Self {
        Self {
            key: key.to_owned(),
            command: command.to_owned(),
            when: when.map(str::to_owned),
        }
    }

    /// The encoded rule (`{key, command, when?}`).
    pub fn to_json(&self) -> Value {
        let mut value = json!({"key": self.key, "command": self.command});
        if let Some(when) = &self.when {
            value["when"] = Value::String(when.clone());
        }
        value
    }
}

/// `STATIC_KEYBINDING_COMMANDS`, in declaration order (the order of the TS error text).
pub const STATIC_KEYBINDING_COMMANDS: &[&str] = &[
    "sidebar.toggle",
    "navigation.back",
    "navigation.forward",
    "terminal.toggle",
    "terminal.split",
    "terminal.splitVertical",
    "terminal.new",
    "terminal.close",
    "rightPanel.toggle",
    "rightPanel.toggleMaximized",
    "rightPanel.close",
    "pullRequest.copyNumber",
    "diff.toggle",
    "preview.toggle",
    "preview.refresh",
    "preview.focusUrl",
    "preview.zoomIn",
    "preview.zoomOut",
    "preview.resetZoom",
    "commandPalette.toggle",
    "filePicker.toggle",
    "projectSearch.toggle",
    "usage.open",
    "theme.select",
    "appearance.cycle",
    "themeEditor.toggle",
    "composer.stash",
    "composer.host",
    "composer.effort",
    "composer.mode",
    "composer.workspace",
    "composer.previousWorktree",
    "composer.branch",
    "chat.new",
    "chat.newLocal",
    "editor.openFavorite",
    "usage.cost",
    "usage.tokens",
    "usage.limits",
    "usage.period.day",
    "usage.period.week",
    "usage.period.month",
    "usage.period.quarter",
    "modelPicker.toggle",
    "modelPicker.previousProvider",
    "modelPicker.nextProvider",
    "modelPicker.jump.1",
    "modelPicker.jump.2",
    "modelPicker.jump.3",
    "modelPicker.jump.4",
    "modelPicker.jump.5",
    "modelPicker.jump.6",
    "modelPicker.jump.7",
    "modelPicker.jump.8",
    "modelPicker.jump.9",
    "thread.stop",
    "thread.steerQueuedMessage",
    "thread.previous",
    "thread.next",
    "thread.copyReference",
    "thread.settle",
    "thread.pin",
    "thread.undo",
    "thread.jump.1",
    "thread.jump.2",
    "thread.jump.3",
    "thread.jump.4",
    "thread.jump.5",
    "thread.jump.6",
    "thread.jump.7",
    "thread.jump.8",
    "thread.jump.9",
];

/// `script.<id>.run` with `<id>` matching `^[a-z0-9][a-z0-9-]*$`, at most 24 characters.
pub fn is_script_run_command(command: &str) -> bool {
    let Some(id) = command.strip_prefix("script.").and_then(|rest| rest.strip_suffix(".run")) else {
        return false;
    };
    let mut chars = id.chars();
    !id.is_empty()
        && id.len() <= MAX_SCRIPT_ID_LENGTH
        && chars.next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// `KeybindingCommand`.
pub fn is_keybinding_command(command: &str) -> bool {
    STATIC_KEYBINDING_COMMANDS.contains(&command) || is_script_run_command(command)
}

/// `DEFAULT_KEYBINDINGS` (56 rules).
pub fn default_keybindings() -> Vec<KeybindingRule> {
    let mut rules: Vec<KeybindingRule> = [
        ("mod+b", "sidebar.toggle", None),
        ("mod+[", "navigation.back", Some("!terminalFocus")),
        ("mod+]", "navigation.forward", Some("!terminalFocus")),
        ("mod+j", "terminal.toggle", None),
        ("mod+alt+b", "rightPanel.toggle", None),
        ("mod+d", "terminal.split", Some("terminalFocus")),
        ("mod+shift+d", "terminal.splitVertical", Some("terminalFocus")),
        ("mod+n", "terminal.new", Some("terminalFocus")),
        ("mod+w", "terminal.close", Some("terminalFocus")),
        ("mod+w", "rightPanel.close", Some("!terminalFocus")),
        ("mod+d", "diff.toggle", Some("!terminalFocus")),
        ("mod+shift+j", "preview.toggle", None),
        ("mod+r", "preview.refresh", Some("previewFocus")),
        ("mod+l", "preview.focusUrl", Some("previewFocus")),
        ("mod+=", "preview.zoomIn", Some("previewFocus")),
        ("mod++", "preview.zoomIn", Some("previewFocus")),
        ("mod+-", "preview.zoomOut", Some("previewFocus")),
        ("mod+0", "preview.resetZoom", Some("previewFocus")),
        ("mod+k", "commandPalette.toggle", Some("!terminalFocus")),
        ("mod+p", "filePicker.toggle", Some("!terminalFocus")),
        ("mod+shift+f", "projectSearch.toggle", Some("!terminalFocus")),
        ("mod+u", "usage.open", Some("!terminalFocus")),
        ("mod+alt+a", "theme.select", Some("!terminalFocus")),
        ("mod+alt+shift+a", "appearance.cycle", Some("!terminalFocus")),
        ("mod+alt+shift+t", "themeEditor.toggle", None),
        ("mod+s", "composer.stash", Some("!terminalFocus")),
        ("mod+shift+enter", "thread.steerQueuedMessage", Some("!terminalFocus")),
        ("mod+n", "chat.new", Some("!terminalFocus")),
        ("mod+shift+o", "chat.new", Some("!terminalFocus")),
        ("mod+shift+n", "chat.newLocal", Some("!terminalFocus")),
        ("mod+shift+m", "modelPicker.toggle", Some("!terminalFocus")),
        ("mod+shift+h", "composer.host", Some("!terminalFocus")),
        ("mod+shift+e", "composer.effort", Some("!terminalFocus")),
        ("mod+shift+a", "composer.mode", Some("!terminalFocus")),
        ("mod+shift+x", "composer.workspace", Some("!terminalFocus")),
        ("mod+shift+g", "composer.branch", Some("!terminalFocus")),
        ("mod+shift+l", "composer.previousWorktree", Some("!terminalFocus")),
        ("mod+shift+k", "pullRequest.copyNumber", Some("!terminalFocus")),
        ("mod+shift+arrowup", "modelPicker.previousProvider", Some("modelPickerOpen")),
        ("mod+shift+arrowdown", "modelPicker.nextProvider", Some("modelPickerOpen")),
        ("mod+o", "editor.openFavorite", None),
        ("mod+shift+[", "thread.previous", None),
        ("mod+shift+]", "thread.next", None),
        ("mod+shift+c", "thread.copyReference", Some("!terminalFocus")),
        ("mod+shift+s", "thread.settle", Some("!terminalFocus")),
        ("mod+shift+p", "thread.pin", Some("!terminalFocus")),
        ("mod+z", "thread.undo", Some("!terminalFocus && !editableFocus")),
    ]
    .into_iter()
    .map(|(key, command, when)| KeybindingRule::new(key, command, when))
    .collect();
    for index in 1..=9 {
        rules.push(KeybindingRule::new(&format!("mod+{index}"), &format!("thread.jump.{index}"), Some("isDesktop")));
    }
    for index in 1..=9 {
        rules.push(KeybindingRule::new(
            &format!("mod+{index}"),
            &format!("modelPicker.jump.{index}"),
            Some("modelPickerOpen && isDesktop"),
        ));
    }
    for (key, command) in [
        ("c", "usage.cost"),
        ("t", "usage.tokens"),
        ("l", "usage.limits"),
        ("mod+shift+1", "usage.period.day"),
        ("mod+shift+2", "usage.period.week"),
        ("mod+shift+3", "usage.period.month"),
        ("mod+shift+4", "usage.period.quarter"),
    ] {
        rules.push(KeybindingRule::new(key, command, Some("usagePageOpen")));
    }
    rules
}

fn normalize_key_token(token: &str) -> String {
    match token {
        "space" => " ".to_owned(),
        "esc" => "escape".to_owned(),
        other => other.to_owned(),
    }
}

/// `parseKeybindingShortcut`: `mod+shift+k` → shortcut; `None` when malformed (no key, two
/// keys, empty tokens). A trailing `+` is the plus key.
pub fn parse_keybinding_shortcut(value: &str) -> Option<KeybindingShortcut> {
    let lowered = value.to_lowercase();
    let mut tokens: Vec<String> = lowered.split('+').map(|token| js_trim(token).to_owned()).collect();
    let mut trailing_empty = 0;
    while tokens.last().is_some_and(String::is_empty) {
        trailing_empty += 1;
        tokens.pop();
    }
    if trailing_empty > 0 {
        tokens.push("+".to_owned());
    }
    if tokens.iter().any(String::is_empty) || tokens.is_empty() {
        return None;
    }
    let mut shortcut = KeybindingShortcut {
        key: String::new(),
        meta_key: false,
        ctrl_key: false,
        shift_key: false,
        alt_key: false,
        mod_key: false,
    };
    let mut key: Option<String> = None;
    for token in &tokens {
        match token.as_str() {
            "cmd" | "meta" => shortcut.meta_key = true,
            "ctrl" | "control" => shortcut.ctrl_key = true,
            "shift" => shortcut.shift_key = true,
            "alt" | "option" => shortcut.alt_key = true,
            "mod" => shortcut.mod_key = true,
            other => {
                if key.is_some() {
                    return None;
                }
                key = Some(normalize_key_token(other));
            }
        }
    }
    shortcut.key = key?;
    Some(shortcut)
}

/// `encodeShortcut`: the canonical key string (`None` when it cannot be represented).
pub fn encode_shortcut(shortcut: &KeybindingShortcut) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    if shortcut.mod_key {
        parts.push("mod");
    }
    if shortcut.meta_key {
        parts.push("meta");
    }
    if shortcut.ctrl_key {
        parts.push("ctrl");
    }
    if shortcut.alt_key {
        parts.push("alt");
    }
    if shortcut.shift_key {
        parts.push("shift");
    }
    if shortcut.key.is_empty() {
        return None;
    }
    if shortcut.key != "+" && shortcut.key.contains('+') {
        return None;
    }
    let key = if shortcut.key == " " { "space" } else { &shortcut.key };
    parts.push(key);
    Some(parts.join("+"))
}

/// The encode side of `ResolvedKeybindingFromConfig`: a resolved rule back to a config rule.
pub fn encode_resolved_rule(rule: &ResolvedKeybindingRule) -> Option<KeybindingRule> {
    let key = encode_shortcut(&rule.shortcut)?;
    let command = serde_json::to_value(&rule.command).ok()?.as_str()?.to_owned();
    Some(KeybindingRule {
        key,
        command,
        when: rule.when_ast.as_ref().map(encode_when_ast),
    })
}

fn command_of(command: &str) -> Option<KeybindingCommand> {
    serde_json::from_value(Value::String(command.to_owned())).ok()
}

/// `compileResolvedKeybindingRule`: `None` when the shortcut or the `when` does not parse.
pub fn compile_resolved_keybinding_rule(rule: &KeybindingRule) -> Option<ResolvedKeybindingRule> {
    let shortcut = parse_keybinding_shortcut(&rule.key)?;
    let command = command_of(&rule.command)?;
    let when_ast = match &rule.when {
        Some(when) => Some(parse_keybinding_when_expression(when)?),
        None => None,
    };
    Some(ResolvedKeybindingRule { command, shortcut, when_ast })
}

/// `compileResolvedKeybindingsConfig`: valid rules, the last 256.
pub fn compile_resolved_keybindings_config(config: &[KeybindingRule]) -> Vec<ResolvedKeybindingRule> {
    let compiled: Vec<_> = config.iter().filter_map(compile_resolved_keybinding_rule).collect();
    let skip = compiled.len().saturating_sub(MAX_KEYBINDINGS_COUNT);
    compiled.into_iter().skip(skip).collect()
}

/// `DEFAULT_RESOLVED_KEYBINDINGS`.
pub fn default_resolved_keybindings() -> Vec<ResolvedKeybindingRule> {
    compile_resolved_keybindings_config(&default_keybindings())
}

/// `mergeWithDefaultKeybindings`: defaults whose command the user did not bind, then the
/// user's rules; the last 256.
pub fn merge_with_default_keybindings(custom: Vec<ResolvedKeybindingRule>) -> Vec<ResolvedKeybindingRule> {
    if custom.is_empty() {
        return default_resolved_keybindings();
    }
    let overridden: Vec<&KeybindingCommand> = custom.iter().map(|rule| &rule.command).collect();
    let mut merged: Vec<ResolvedKeybindingRule> = default_resolved_keybindings()
        .into_iter()
        .filter(|rule| !overridden.contains(&&rule.command))
        .collect();
    merged.extend(custom);
    let skip = merged.len().saturating_sub(MAX_KEYBINDINGS_COUNT);
    merged.into_iter().skip(skip).collect()
}

/// `isSameKeybindingRule`.
pub fn is_same_keybinding_rule(left: &KeybindingRule, right: &KeybindingRule) -> bool {
    left.command == right.command && left.key == right.key && left.when == right.when
}

fn shortcut_context(rule: &KeybindingRule) -> Option<String> {
    let parsed = parse_keybinding_shortcut(&rule.key)?;
    let encoded = encode_shortcut(&parsed)?;
    Some(format!("{encoded}\u{0}{}", rule.when.as_deref().unwrap_or("")))
}

/// `hasSameShortcutContext`: same canonical shortcut and same `when` text.
pub fn has_same_shortcut_context(left: &KeybindingRule, right: &KeybindingRule) -> bool {
    match (shortcut_context(left), shortcut_context(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

// ---------------------------------------------------------------------------------------------
// Decoding with the TS messages (`Cause.pretty` of the Schema error)
// ---------------------------------------------------------------------------------------------

fn command_union_text(with_string: bool) -> String {
    let mut members: Vec<String> = STATIC_KEYBINDING_COMMANDS.iter().map(|c| format!("\"{c}\"")).collect();
    if with_string {
        members.push("string".to_owned());
    }
    members.join(" | ")
}

/// `Schema.decodeUnknown(KeybindingRule)`: the rule (trimmed), or the `Cause.pretty` text.
pub fn decode_keybinding_rule(entry: &Value) -> Result<KeybindingRule, String> {
    let Value::Object(map) = entry else {
        return Err("SchemaError: Expected object".to_owned());
    };
    let key = trimmed_string_field(map.get("key"), "key", MAX_KEYBINDING_VALUE_LENGTH, false)?.expect("required");
    let command = match map.get("command") {
        None => return Err("SchemaError: Missing key\n  at [\"command\"]".to_owned()),
        Some(Value::String(command)) if is_keybinding_command(command) => command.clone(),
        Some(Value::String(_)) => {
            return Err(format!(
                "SchemaError: Expected {}\n  at [\"command\"]\nExpected a string matching template literal parts\n  at [\"command\"]",
                command_union_text(false)
            ))
        }
        Some(_) => return Err(format!("SchemaError: Expected {}\n  at [\"command\"]", command_union_text(true))),
    };
    let when = trimmed_string_field(map.get("when"), "when", MAX_KEYBINDING_WHEN_LENGTH, true)?;
    Ok(KeybindingRule { key, command, when })
}

fn trimmed_string_field(value: Option<&Value>, name: &str, max: usize, optional: bool) -> Result<Option<String>, String> {
    let at = format!("\n  at [\"{name}\"]");
    match value {
        None if optional => Ok(None),
        None => Err(format!("SchemaError: Missing key{at}")),
        Some(Value::String(text)) => {
            let trimmed = js_trim(text);
            let length = js_length(trimmed);
            if length < 1 {
                Err(format!("SchemaError: Expected a value with a length of at least 1{at}"))
            } else if length > max {
                Err(format!("SchemaError: Expected a value with a length of at most {max}{at}"))
            } else {
                Ok(Some(trimmed.to_owned()))
            }
        }
        Some(_) if optional => Err(format!("SchemaError: Expected string | undefined{at}")),
        Some(_) => Err(format!("SchemaError: Expected string{at}")),
    }
}

/// `ResolvedKeybindingFromConfig` decode: the rule must compile.
pub fn resolve_keybinding_rule(rule: &KeybindingRule) -> Result<ResolvedKeybindingRule, String> {
    compile_resolved_keybinding_rule(rule).ok_or_else(|| "SchemaError: Invalid keybinding rule".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shortcut(key: &str, modifiers: &[&str]) -> KeybindingShortcut {
        KeybindingShortcut {
            key: key.to_owned(),
            meta_key: modifiers.contains(&"meta"),
            ctrl_key: modifiers.contains(&"ctrl"),
            shift_key: modifiers.contains(&"shift"),
            alt_key: modifiers.contains(&"alt"),
            mod_key: modifiers.contains(&"mod"),
        }
    }

    // `parses shortcuts including plus key` (keybindings.test.ts).
    #[test]
    fn parses_shortcuts_including_plus_key() {
        assert_eq!(parse_keybinding_shortcut("mod+j"), Some(shortcut("j", &["mod"])));
        assert_eq!(parse_keybinding_shortcut("mod++"), Some(shortcut("+", &["mod"])));
        assert_eq!(parse_keybinding_shortcut("Cmd+Shift+K"), Some(shortcut("k", &["meta", "shift"])));
        assert_eq!(parse_keybinding_shortcut("ctrl+alt+space"), Some(shortcut(" ", &["ctrl", "alt"])));
        assert_eq!(parse_keybinding_shortcut("control+option+esc"), Some(shortcut("escape", &["ctrl", "alt"])));
        assert_eq!(parse_keybinding_shortcut("mod+shift+d+d"), None);
        assert_eq!(parse_keybinding_shortcut("mod+"), Some(shortcut("+", &["mod"])));
        assert_eq!(parse_keybinding_shortcut("mod"), None);
        assert_eq!(parse_keybinding_shortcut("mod++shift"), None);
        // Like JS, `"".split("+")` is one empty token: the plus key.
        assert_eq!(parse_keybinding_shortcut(""), Some(shortcut("+", &[])));
    }

    // `encodes resolved plus-key shortcuts`.
    #[test]
    fn encodes_resolved_plus_key_shortcuts() {
        let resolved = compile_resolved_keybinding_rule(&KeybindingRule::new("mod++", "preview.zoomIn", None)).unwrap();
        assert_eq!(encode_resolved_rule(&resolved).unwrap(), KeybindingRule::new("mod++", "preview.zoomIn", None));
        assert_eq!(encode_shortcut(&shortcut(" ", &["shift"])).as_deref(), Some("shift+space"));
        assert_eq!(encode_shortcut(&shortcut("a+b", &[])), None);
        assert_eq!(
            encode_shortcut(&shortcut("k", &["meta", "ctrl", "alt", "shift", "mod"])).as_deref(),
            Some("mod+meta+ctrl+alt+shift+k")
        );
    }

    // `compiles valid rule with parsed when AST` + `rejects invalid rules`.
    #[test]
    fn compiles_and_rejects_rules() {
        let compiled = compile_resolved_keybinding_rule(&KeybindingRule::new("mod+d", "terminal.split", Some("terminalOpen && !terminalFocus"))).unwrap();
        assert_eq!(
            serde_json::to_value(&compiled).unwrap(),
            json!({
                "command": "terminal.split",
                "shortcut": {"key": "d", "metaKey": false, "ctrlKey": false, "shiftKey": false, "altKey": false, "modKey": true},
                "whenAst": {"type": "and", "left": {"type": "identifier", "name": "terminalOpen"}, "right": {"type": "not", "node": {"type": "identifier", "name": "terminalFocus"}}}
            })
        );
        assert!(compile_resolved_keybinding_rule(&KeybindingRule::new("mod+shift+d+d", "terminal.split", None)).is_none());
        assert!(compile_resolved_keybinding_rule(&KeybindingRule::new("mod+d", "terminal.split", Some("a &&"))).is_none());
        assert_eq!(
            resolve_keybinding_rule(&KeybindingRule::new("mod+shift+d+d", "terminal.split", None)).unwrap_err(),
            "SchemaError: Invalid keybinding rule"
        );
    }

    #[test]
    fn decodes_rules_with_the_schema_messages() {
        assert_eq!(
            decode_keybinding_rule(&json!({"key": "  mod+x ", "command": "chat.new", "when": " a "})),
            Ok(KeybindingRule::new("mod+x", "chat.new", Some("a")))
        );
        assert_eq!(decode_keybinding_rule(&json!("x")).unwrap_err(), "SchemaError: Expected object");
        assert_eq!(
            decode_keybinding_rule(&json!({"command": "chat.new"})).unwrap_err(),
            "SchemaError: Missing key\n  at [\"key\"]"
        );
        assert_eq!(
            decode_keybinding_rule(&json!({"key": 1, "command": "chat.new"})).unwrap_err(),
            "SchemaError: Expected string\n  at [\"key\"]"
        );
        assert_eq!(
            decode_keybinding_rule(&json!({"key": "x".repeat(65), "command": "chat.new"})).unwrap_err(),
            "SchemaError: Expected a value with a length of at most 64\n  at [\"key\"]"
        );
        assert_eq!(
            decode_keybinding_rule(&json!({"key": "mod+x", "command": "chat.new", "when": null})).unwrap_err(),
            "SchemaError: Expected string | undefined\n  at [\"when\"]"
        );
        assert!(decode_keybinding_rule(&json!({"key": "mod+x", "command": "nope"}))
            .unwrap_err()
            .ends_with("\"thread.jump.9\"\n  at [\"command\"]\nExpected a string matching template literal parts\n  at [\"command\"]"));
        assert!(decode_keybinding_rule(&json!({"key": "mod+x", "command": 5}))
            .unwrap_err()
            .ends_with("\"thread.jump.9\" | string\n  at [\"command\"]"));
    }

    // `contracts/keybindings.test.ts`: commands.
    #[test]
    fn accepts_static_and_script_commands() {
        for command in [
            "terminal.toggle",
            "rightPanel.toggleMaximized",
            "thread.stop",
            "modelPicker.jump.1",
            "script.setup.run",
            "script.a-1.run",
        ] {
            assert!(is_keybinding_command(command), "{command}");
        }
        for command in [
            "script.Test.run",
            "script..run",
            "script.-a.run",
            "someFuture.toggle",
            &format!("script.{}.run", "a".repeat(25)),
        ] {
            assert!(!is_keybinding_command(command), "{command}");
        }
    }

    #[test]
    fn defaults_compile_and_merge() {
        let defaults = default_keybindings();
        assert_eq!(defaults.len(), 72);
        assert_eq!(default_resolved_keybindings().len(), 72);
        let custom = compile_resolved_keybindings_config(&[KeybindingRule::new("mod+shift+b", "sidebar.toggle", None)]);
        let merged = merge_with_default_keybindings(custom);
        assert_eq!(merged.len(), 72);
        assert_eq!(serde_json::to_value(&merged.last().unwrap().command).unwrap(), json!("sidebar.toggle"));
        assert_eq!(merged.last().unwrap().shortcut.key, "b");
    }

    #[test]
    fn shortcut_contexts_compare_canonical_shortcuts() {
        assert!(has_same_shortcut_context(
            &KeybindingRule::new("Shift+Mod+K", "a", Some("x")),
            &KeybindingRule::new("mod+shift+k", "b", Some("x"))
        ));
        assert!(!has_same_shortcut_context(
            &KeybindingRule::new("mod+k", "a", Some("x")),
            &KeybindingRule::new("mod+k", "b", None)
        ));
    }
}
