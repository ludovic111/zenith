//! `provider/Drivers/ClaudeSkills.ts`: filesystem discovery of Claude Code skills for the `$`
//! picker and for skill dispatch. Skills live in `<config dir>/skills` (user, wins on name
//! collisions) and `<cwd>/.claude/skills` (project), one directory per skill with a `SKILL.md`
//! whose YAML frontmatter carries metadata; `skillOverrides` in the settings files switch them
//! off. Every behavior here was verified against the CLI by the TS authors (see the comments in
//! the TS module); this port keeps them.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::home::{resolve_against, Env};
use zc_core::paths::{expand_home_path, home_dir, resolve_path};

/// `ServerProviderSkill`, as the Claude driver reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeSkill {
    pub name: String,
    pub path: String,
    pub enabled: bool,
    /// `user` or `project`.
    pub scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_invocation_only: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_invocable: Option<bool>,
}

enum Frontmatter {
    Missing,
    Malformed,
    Parsed {
        description: Option<String>,
        user_invocation_only: bool,
        user_invocable_false: bool,
    },
}

fn frontmatter_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)\A---\r?\n(.*?)\r?\n---(?:\r?\n|\z)").expect("valid regex"))
}

/// `parseFrontmatterBoolean`: the YAML 1.1 spellings Claude Code accepts.
fn parse_frontmatter_boolean(value: Option<&serde_yaml::Value>) -> Option<bool> {
    match value? {
        serde_yaml::Value::Bool(b) => Some(*b),
        serde_yaml::Value::Number(n) => match n.as_f64() {
            Some(1.0) => Some(true),
            Some(0.0) => Some(false),
            _ => None,
        },
        serde_yaml::Value::String(s) => match s.trim().to_lowercase().as_str() {
            "true" | "yes" | "on" | "y" => Some(true),
            "false" | "no" | "off" | "n" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn parse_skill_frontmatter(contents: &str) -> Frontmatter {
    let Some(captures) = frontmatter_regex().captures(contents) else {
        return Frontmatter::Missing;
    };
    let body = captures.get(1).map_or("", |m| m.as_str());
    let parsed: serde_yaml::Value = match serde_yaml::from_str(body) {
        Ok(value) => value,
        Err(_) => return Frontmatter::Malformed,
    };
    let empty = serde_yaml::Mapping::new();
    let record = match &parsed {
        serde_yaml::Value::Mapping(map) => map,
        serde_yaml::Value::Sequence(_) => &empty,
        serde_yaml::Value::Tagged(tagged) => match &tagged.value {
            serde_yaml::Value::Mapping(map) => map,
            _ => return Frontmatter::Malformed,
        },
        _ => return Frontmatter::Malformed,
    };
    let get = |key: &str| record.get(serde_yaml::Value::String(key.to_string()));
    let description = match get("description") {
        Some(serde_yaml::Value::String(s)) => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
        _ => None,
    };
    Frontmatter::Parsed {
        description,
        user_invocation_only: parse_frontmatter_boolean(get("disable-model-invocation")) == Some(true),
        user_invocable_false: parse_frontmatter_boolean(get("user-invocable")) == Some(false),
    }
}

/// The host platform as Node names it (`darwin`, `win32`, `linux`).
pub fn host_platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(windows) {
        "win32"
    } else {
        "linux"
    }
}

fn join(base: &str, parts: &[&str], windows: bool) -> String {
    let separator = if windows { '\\' } else { '/' };
    let mut out = base.trim_end_matches(['/', '\\']).to_string();
    for part in parts {
        out.push(separator);
        out.push_str(part);
    }
    out
}

/// `skillOverrideSettingsPaths`: user, project, project-local, the repository root's local file
/// (from a nested workspace), then the administrator's managed policy.
pub fn skill_override_settings_paths(config_dir: &str, cwd: Option<&str>, platform: &str, environment: &Env, repository_root: Option<&str>) -> Vec<String> {
    let windows = platform == "win32";
    let managed = match platform {
        "darwin" => Some("/Library/Application Support/ClaudeCode/managed-settings.json".to_string()),
        "win32" => environment
            .get("PROGRAMDATA")
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
            .map(|program_data| join(program_data, &["ClaudeCode", "managed-settings.json"], true)),
        _ => Some("/etc/claude-code/managed-settings.json".to_string()),
    };
    let root = repository_root.filter(|root| Some(*root) != cwd);
    let mut paths = vec![join(config_dir, &["settings.json"], windows)];
    if let Some(cwd) = cwd {
        paths.push(join(cwd, &[".claude", "settings.json"], windows));
        paths.push(join(cwd, &[".claude", "settings.local.json"], windows));
    }
    if let Some(root) = root {
        paths.push(join(root, &[".claude", "settings.local.json"], windows));
    }
    paths.extend(managed);
    paths
}

/// `findRepositoryRoot`: the nearest ancestor (inclusive) holding `.git`.
fn find_repository_root(cwd: &str) -> Option<String> {
    let mut current = resolve_path(Path::new(cwd));
    loop {
        if current.join(".git").exists() {
            return Some(current.to_string_lossy().into_owned());
        }
        let parent = current.parent()?.to_path_buf();
        if parent == current {
            return None;
        }
        current = parent;
    }
}

#[derive(Clone, Copy)]
struct SkillOverride {
    enabled: bool,
    user_invocation_only: bool,
}

/// One settings file's `skillOverrides`, or `None` when the file or any value is invalid.
fn decode_skill_overrides(contents: &str) -> Option<Vec<(String, SkillOverride)>> {
    let value: Value = zc_core::lenient_json::parse_lenient_json(contents).ok()?;
    let object = value.as_object()?;
    let overrides = match object.get("skillOverrides") {
        None => return Some(Vec::new()),
        Some(Value::Object(map)) => map,
        Some(_) => return None,
    };
    let mut parsed = Vec::new();
    for (name, value) in overrides {
        let override_ = match value.as_str()? {
            "off" => SkillOverride {
                enabled: false,
                user_invocation_only: false,
            },
            "user-invocable-only" => SkillOverride {
                enabled: true,
                user_invocation_only: true,
            },
            "on" | "name-only" => SkillOverride {
                enabled: true,
                user_invocation_only: false,
            },
            _ => return None,
        };
        parsed.push((name.clone(), override_));
    }
    Some(parsed)
}

fn read_skill_overrides(config_dir: &str, cwd: Option<&str>, environment: &Env) -> HashMap<String, SkillOverride> {
    let repository_root = cwd.and_then(find_repository_root);
    let mut overrides = HashMap::new();
    for settings_path in skill_override_settings_paths(config_dir, cwd, host_platform(), environment, repository_root.as_deref()) {
        let Ok(contents) = std::fs::read_to_string(&settings_path) else { continue };
        let Some(entries) = decode_skill_overrides(&contents) else {
            tracing::debug!(path = settings_path, "claude settings file is unreadable; ignoring skillOverrides");
            continue;
        };
        overrides.extend(entries);
    }
    overrides
}

/// `resolveClaudeConfigDirPath`: what the spawned CLI sees as its config dir. A relative
/// inherited `CLAUDE_CONFIG_DIR` resolves against the workspace (the CLI's own cwd).
pub fn resolve_claude_config_dir_path(home_path: &str, environment: &Env, cwd: Option<&str>) -> PathBuf {
    let home_path = home_path.trim();
    if !home_path.is_empty() {
        return resolve_path(&expand_home_path(home_path));
    }
    let from_env = environment.get("CLAUDE_CONFIG_DIR").map(|v| v.trim()).unwrap_or_default();
    if !from_env.is_empty() {
        return match cwd {
            Some(cwd) => resolve_against(Path::new(cwd), from_env),
            None => resolve_path(Path::new(from_env)),
        };
    }
    home_dir().join(".claude")
}

/// A stand-in for `String.prototype.localeCompare` (ICU root collation) good enough for skill
/// names: punctuation before digits before letters, case-insensitive first, lowercase first.
pub(crate) fn locale_compare(left: &str, right: &str) -> Ordering {
    fn weight(c: char) -> (u32, u32) {
        const PUNCTUATION: &str = "_-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";
        if let Some(index) = PUNCTUATION.find(c) {
            return (index as u32, 0);
        }
        if c.is_ascii_digit() {
            return (100 + c as u32, 0);
        }
        if c.is_alphabetic() {
            let lower = c.to_lowercase().next().unwrap_or(c);
            return (1000 + lower as u32, u32::from(c.is_uppercase()));
        }
        (500 + c as u32, 0)
    }
    let primary = |s: &str| s.chars().map(|c| weight(c).0).collect::<Vec<_>>();
    let tertiary = |s: &str| s.chars().map(|c| weight(c).1).collect::<Vec<_>>();
    primary(left)
        .cmp(&primary(right))
        .then_with(|| tertiary(left).cmp(&tertiary(right)))
        .then_with(|| left.cmp(right))
}

/// `discoverClaudeSkills(config, cwd, environment)`: best effort, sorted by name.
pub fn discover_claude_skills(home_path: &str, cwd: Option<&str>, environment: &Env) -> Vec<ClaudeSkill> {
    let config_dir = resolve_claude_config_dir_path(home_path, environment, cwd).to_string_lossy().into_owned();
    let overrides = read_skill_overrides(&config_dir, cwd, environment);
    let mut roots = vec![(PathBuf::from(&config_dir).join("skills"), "user")];
    if let Some(cwd) = cwd {
        roots.push((PathBuf::from(cwd).join(".claude").join("skills"), "project"));
    }
    let mut skills: Vec<ClaudeSkill> = Vec::new();
    for (directory, scope) in roots {
        let mut entries: Vec<String> = match std::fs::read_dir(&directory) {
            Ok(read) => read.filter_map(|entry| entry.ok()?.file_name().into_string().ok()).collect(),
            Err(_) => Vec::new(),
        };
        // JS `Array.prototype.sort()`: UTF-16 code unit order.
        entries.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
        for entry in entries {
            let skill_path = directory.join(&entry).join("SKILL.md");
            let Ok(contents) = std::fs::read_to_string(&skill_path) else { continue };
            let frontmatter = parse_skill_frontmatter(&contents);
            if matches!(frontmatter, Frontmatter::Malformed) {
                continue;
            }
            let name = entry.trim().to_string();
            if name.is_empty() || skills.iter().any(|skill| skill.name == name) {
                continue;
            }
            let override_ = overrides.get(&name).copied();
            let (description, frontmatter_user_only, user_invocable_false) = match frontmatter {
                Frontmatter::Parsed {
                    description,
                    user_invocation_only,
                    user_invocable_false,
                } => (description, user_invocation_only, user_invocable_false),
                _ => (None, false, false),
            };
            let user_invocation_only = frontmatter_user_only || override_.is_some_and(|o| o.user_invocation_only);
            skills.push(ClaudeSkill {
                name,
                path: skill_path.to_string_lossy().into_owned(),
                enabled: override_.is_none_or(|o| o.enabled),
                scope: scope.to_string(),
                description,
                user_invocation_only: user_invocation_only.then_some(true),
                user_invocable: user_invocable_false.then_some(false),
            });
        }
    }
    skills.sort_by(|a, b| locale_compare(&a.name, &b.name));
    skills
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(skills_dir: &Path, directory: &str, contents: &str) {
        let dir = skills_dir.join(directory);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), contents).unwrap();
    }

    fn s(path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }

    fn names_enabled(skills: &[ClaudeSkill]) -> Vec<(String, bool)> {
        skills.iter().map(|skill| (skill.name.clone(), skill.enabled)).collect()
    }

    fn pairs(items: &[(&str, bool)]) -> Vec<(String, bool)> {
        items.iter().map(|(n, e)| (n.to_string(), *e)).collect()
    }

    #[test]
    fn discovers_user_and_project_skills_with_frontmatter_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("claude-home");
        let workspace = temp.path().join("workspace");
        write_skill(
            &config_dir.join("skills"),
            "codex-review",
            "---\nname: codex-review\ndescription: Ask Codex for a review.\n---\n\n# Body",
        );
        write_skill(
            &workspace.join(".claude/skills"),
            "deploy",
            "---\nname: deploy\ndescription: Deploy the app.\n---\n\n# Deploy",
        );
        let skills = discover_claude_skills(&s(&config_dir), Some(&s(&workspace)), &Env::new());
        assert_eq!(
            skills,
            vec![
                ClaudeSkill {
                    name: "codex-review".into(),
                    path: s(&config_dir.join("skills/codex-review/SKILL.md")),
                    enabled: true,
                    scope: "user".into(),
                    description: Some("Ask Codex for a review.".into()),
                    user_invocation_only: None,
                    user_invocable: None,
                },
                ClaudeSkill {
                    name: "deploy".into(),
                    path: s(&workspace.join(".claude/skills/deploy/SKILL.md")),
                    enabled: true,
                    scope: "project".into(),
                    description: Some("Deploy the app.".into()),
                    user_invocation_only: None,
                    user_invocable: None,
                },
            ]
        );
        assert_eq!(
            serde_json::to_value(&skills[0]).unwrap(),
            serde_json::json!({"name": "codex-review", "path": s(&config_dir.join("skills/codex-review/SKILL.md")), "enabled": true, "scope": "user", "description": "Ask Codex for a review."})
        );
    }

    #[test]
    fn ignores_agents_skills_and_prefers_user_skills_on_collisions() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("claude-home");
        let workspace = temp.path().join("workspace");
        write_skill(&workspace.join(".agents/skills"), "review", "---\ndescription: Codex only.\n---\n");
        write_skill(&config_dir.join("skills"), "deploy", "---\ndescription: User deploy.\n---\n");
        write_skill(&workspace.join(".claude/skills"), "deploy", "---\ndescription: Project deploy.\n---\n");
        let skills = discover_claude_skills(&s(&config_dir), Some(&s(&workspace)), &Env::new());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].scope, "user");
        assert_eq!(skills[0].description.as_deref(), Some("User deploy."));
    }

    #[test]
    fn ignores_agents_skills_which_claude_code_does_not_load() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("claude-home");
        let workspace = temp.path().join("workspace");
        // `/review` here is answered with `Unknown command` by the CLI.
        write_skill(
            &workspace.join(".agents/skills"),
            "review",
            "---\nname: review\ndescription: Review the changes.\n---",
        );
        assert_eq!(
            discover_claude_skills(&s(&config_dir), Some(&s(&workspace)), &Env::new()),
            Vec::<ClaudeSkill>::new()
        );
    }

    #[test]
    fn prefers_user_skills_on_name_collisions_even_with_a_stray_agents_copy() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("claude-home");
        let workspace = temp.path().join("workspace");
        write_skill(&config_dir.join("skills"), "deploy", "---\nname: deploy\ndescription: User deploy.\n---");
        write_skill(
            &workspace.join(".agents/skills"),
            "deploy",
            "---\nname: deploy\ndescription: Agents deploy.\n---",
        );
        write_skill(
            &workspace.join(".claude/skills"),
            "deploy",
            "---\nname: deploy\ndescription: Claude deploy.\n---",
        );
        assert_eq!(
            discover_claude_skills(&s(&config_dir), Some(&s(&workspace)), &Env::new()),
            vec![ClaudeSkill {
                name: "deploy".into(),
                path: s(&config_dir.join("skills/deploy/SKILL.md")),
                enabled: true,
                scope: "user".into(),
                description: Some("User deploy.".into()),
                user_invocation_only: None,
                user_invocable: None,
            }]
        );
    }

    #[test]
    fn falls_back_to_the_directory_name_and_skips_malformed_frontmatter() {
        let temp = tempfile::tempdir().unwrap();
        let skills_dir = temp.path().join("claude-home/skills");
        write_skill(&skills_dir, "no-frontmatter", "# Just a heading\n");
        write_skill(&skills_dir, "broken-yaml", "---\nname: [unclosed\n---\n");
        std::fs::write(skills_dir.join("README.md"), "not a skill").unwrap();
        let skills = discover_claude_skills(&s(&temp.path().join("claude-home")), None, &Env::new());
        assert_eq!(skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), vec!["no-frontmatter"]);
        assert_eq!(skills[0].description, None);
    }

    #[test]
    fn honors_claude_config_dir_from_the_environment_when_home_path_is_unset() {
        let temp = tempfile::tempdir().unwrap();
        let env_dir = temp.path().join("env-config");
        write_skill(
            &env_dir.join("skills"),
            "env-skill",
            "---\nname: env-skill\ndescription: From env config dir.\n---",
        );
        let env = Env::from([("CLAUDE_CONFIG_DIR".to_string(), s(&env_dir))]);
        assert_eq!(
            discover_claude_skills("", None, &env).iter().map(|s| s.name.clone()).collect::<Vec<_>>(),
            vec!["env-skill"]
        );
        let explicit = temp.path().join("explicit-home");
        write_skill(&explicit.join("skills"), "explicit-skill", "---\nname: explicit-skill\n---");
        assert_eq!(
            discover_claude_skills(&s(&explicit), None, &env)
                .iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>(),
            vec!["explicit-skill"]
        );
    }

    #[test]
    fn resolves_a_relative_claude_config_dir_against_the_workspace_cwd() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        write_skill(&workspace.join("relative-config/skills"), "relative-skill", "---\nname: relative-skill\n---");
        let env = Env::from([("CLAUDE_CONFIG_DIR".to_string(), "relative-config".to_string())]);
        let skills = discover_claude_skills("", Some(&s(&workspace)), &env);
        assert_eq!(skills.iter().map(|s| s.name.clone()).collect::<Vec<_>>(), vec!["relative-skill"]);
        assert_eq!(skills[0].scope, "user");
    }

    #[test]
    fn marks_skills_that_only_the_user_can_invoke() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        write_skill(
            &workspace.join(".claude/skills"),
            "re-release-version",
            "---\nname: re-release-version\ndescription: Move the current tag forward.\ndisable-model-invocation: true\n---\n\n# Body",
        );
        write_skill(
            &workspace.join(".claude/skills"),
            "release-version",
            "---\nname: release-version\ndescription: Cut a release.\n---\n\n# Body",
        );
        let skills = discover_claude_skills(&s(&temp.path().join("claude-home")), Some(&s(&workspace)), &Env::new());
        assert_eq!(skills.iter().find(|s| s.name == "re-release-version").unwrap().user_invocation_only, Some(true));
        assert_eq!(skills.iter().find(|s| s.name == "release-version").unwrap().user_invocation_only, None);
    }

    #[test]
    fn disables_skills_switched_off_by_skill_overrides() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("claude-home");
        let workspace = temp.path().join("workspace");
        for name in ["kept", "off-by-user", "off-by-project"] {
            write_skill(&config_dir.join("skills"), name, &format!("---\nname: {name}\n---\n\n# Body"));
        }
        std::fs::write(
            config_dir.join("settings.json"),
            r#"{ "skillOverrides": { "off-by-user": "off", "kept": "on" } }"#,
        )
        .unwrap();
        std::fs::create_dir_all(workspace.join(".claude")).unwrap();
        std::fs::write(workspace.join(".claude/settings.json"), r#"{ "skillOverrides": { "off-by-project": "off" } }"#).unwrap();
        let skills = discover_claude_skills(&s(&config_dir), Some(&s(&workspace)), &Env::new());
        assert_eq!(
            names_enabled(&skills),
            pairs(&[("kept", true), ("off-by-project", false), ("off-by-user", false)])
        );
    }

    #[test]
    fn ignores_unreadable_settings_and_drops_a_file_with_one_invalid_value() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("claude-home");
        write_skill(&config_dir.join("skills"), "kept", "---\nname: kept\n---\n\n# Body");
        std::fs::write(config_dir.join("settings.json"), "{ not json").unwrap();
        assert_eq!(
            names_enabled(&discover_claude_skills(&s(&config_dir), None, &Env::new())),
            pairs(&[("kept", true)])
        );

        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("claude-home");
        for name in ["unknown-mode", "boolean-false", "sibling-off"] {
            write_skill(&config_dir.join("skills"), name, &format!("---\nname: {name}\n---\n\n# Body"));
        }
        std::fs::write(
            config_dir.join("settings.json"),
            r#"{ "skillOverrides": { "unknown-mode": "some-future-mode", "boolean-false": false, "sibling-off": "off" } }"#,
        )
        .unwrap();
        assert_eq!(
            names_enabled(&discover_claude_skills(&s(&config_dir), None, &Env::new())),
            pairs(&[("boolean-false", true), ("sibling-off", true), ("unknown-mode", true)])
        );
    }

    #[test]
    fn treats_a_user_invocable_only_override_like_disable_model_invocation() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("claude-home");
        write_skill(&config_dir.join("skills"), "ask-someone", "---\nname: ask-someone\n---\n\n# Body");
        std::fs::write(
            config_dir.join("settings.json"),
            r#"{ "skillOverrides": { "ask-someone": "user-invocable-only" } }"#,
        )
        .unwrap();
        let skills = discover_claude_skills(&s(&config_dir), None, &Env::new());
        assert_eq!((skills[0].enabled, skills[0].user_invocation_only), (true, Some(true)));
    }

    #[test]
    fn reads_repository_root_settings_from_a_nested_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("claude-home");
        let repo = temp.path().join("repo");
        let workspace = repo.join("packages/app");
        for name in ["root-off", "root-off-cwd-on", "cwd-off-root-on"] {
            write_skill(&config_dir.join("skills"), name, &format!("---\nname: {name}\n---\n\n# Body"));
        }
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join(".claude")).unwrap();
        std::fs::create_dir_all(workspace.join(".claude")).unwrap();
        std::fs::write(repo.join(".claude/settings.json"), r#"{ "skillOverrides": { "cwd-off-root-on": "off" } }"#).unwrap();
        std::fs::write(
            repo.join(".claude/settings.local.json"),
            r#"{ "skillOverrides": { "root-off": "off", "root-off-cwd-on": "off", "cwd-off-root-on": "on" } }"#,
        )
        .unwrap();
        std::fs::write(
            workspace.join(".claude/settings.local.json"),
            r#"{ "skillOverrides": { "root-off-cwd-on": "on", "cwd-off-root-on": "off" } }"#,
        )
        .unwrap();
        let skills = discover_claude_skills(&s(&config_dir), Some(&s(&workspace)), &Env::new());
        assert_eq!(
            names_enabled(&skills),
            pairs(&[("cwd-off-root-on", true), ("root-off", false), ("root-off-cwd-on", false)])
        );
    }

    #[test]
    fn ignores_ancestor_settings_outside_a_repository() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("claude-home");
        let parent = temp.path().join("not-a-repo");
        let workspace = parent.join("workspace");
        write_skill(&config_dir.join("skills"), "kept", "---\nname: kept\n---\n\n# Body");
        std::fs::create_dir_all(parent.join(".claude")).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(parent.join(".claude/settings.local.json"), r#"{ "skillOverrides": { "kept": "off" } }"#).unwrap();
        assert_eq!(
            names_enabled(&discover_claude_skills(&s(&config_dir), Some(&s(&workspace)), &Env::new())),
            pairs(&[("kept", true)])
        );
    }

    #[test]
    fn lets_the_administrators_managed_policy_outrank_every_other_settings_file() {
        for (platform, expected) in [
            ("darwin", "/Library/Application Support/ClaudeCode/managed-settings.json"),
            ("linux", "/etc/claude-code/managed-settings.json"),
        ] {
            assert_eq!(
                skill_override_settings_paths("/home/.claude", Some("/workspace"), platform, &Env::new(), None),
                vec![
                    "/home/.claude/settings.json",
                    "/workspace/.claude/settings.json",
                    "/workspace/.claude/settings.local.json",
                    expected
                ]
            );
        }
        let env = Env::from([("PROGRAMDATA".to_string(), r"C:\ProgramData".to_string())]);
        assert_eq!(
            skill_override_settings_paths(r"C:\Users\me\.claude", None, "win32", &env, None).last().unwrap(),
            r"C:\ProgramData\ClaudeCode\managed-settings.json"
        );
        assert_eq!(
            skill_override_settings_paths(r"C:\Users\me\.claude", None, "win32", &Env::new(), None),
            vec![r"C:\Users\me\.claude\settings.json"]
        );
        assert_eq!(
            skill_override_settings_paths("/home/.claude", Some("/repo/packages/app"), "linux", &Env::new(), Some("/repo")),
            vec![
                "/home/.claude/settings.json",
                "/repo/packages/app/.claude/settings.json",
                "/repo/packages/app/.claude/settings.local.json",
                "/repo/.claude/settings.local.json",
                "/etc/claude-code/managed-settings.json"
            ]
        );
        assert_eq!(
            skill_override_settings_paths("/home/.claude", Some("/repo"), "linux", &Env::new(), Some("/repo")),
            vec![
                "/home/.claude/settings.json",
                "/repo/.claude/settings.json",
                "/repo/.claude/settings.local.json",
                "/etc/claude-code/managed-settings.json"
            ]
        );
    }

    #[test]
    fn identifies_and_switches_skills_by_directory_name() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("claude-home");
        write_skill(&config_dir.join("skills"), "probe-alias", "---\nname: probe-alias-frontmatter\n---\n\n# Body");
        std::fs::write(
            config_dir.join("settings.json"),
            r#"{ "skillOverrides": { "probe-alias-frontmatter": "off" } }"#,
        )
        .unwrap();
        assert_eq!(
            names_enabled(&discover_claude_skills(&s(&config_dir), None, &Env::new())),
            pairs(&[("probe-alias", true)])
        );
        std::fs::write(config_dir.join("settings.json"), r#"{ "skillOverrides": { "probe-alias": "off" } }"#).unwrap();
        assert_eq!(
            names_enabled(&discover_claude_skills(&s(&config_dir), None, &Env::new())),
            pairs(&[("probe-alias", false)])
        );
    }

    #[test]
    fn records_agent_only_skills_and_accepts_yaml_11_booleans() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("claude-home");
        let skills_dir = config_dir.join("skills");
        write_skill(&skills_dir, "agent-only", "---\nname: agent-only\nuser-invocable: false\n---\n\n# Body");
        write_skill(&skills_dir, "user-only-yes", "---\ndisable-model-invocation: yes\n---\n\n# Body");
        write_skill(&skills_dir, "agent-only-no", "---\nuser-invocable: no\n---\n\n# Body");
        write_skill(&skills_dir, "plain-off", "---\ndisable-model-invocation: off\n---\n\n# Body");
        let skills = discover_claude_skills(&s(&config_dir), None, &Env::new());
        let summary: Vec<(String, bool, bool)> = skills
            .iter()
            .map(|s| (s.name.clone(), s.user_invocation_only == Some(true), s.user_invocable == Some(false)))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("agent-only".to_string(), false, true),
                ("agent-only-no".to_string(), false, true),
                ("plain-off".to_string(), false, false),
                ("user-only-yes".to_string(), true, false)
            ]
        );
    }

    #[test]
    fn returns_an_empty_list_when_no_skill_roots_exist() {
        let temp = tempfile::tempdir().unwrap();
        assert!(discover_claude_skills(
            &s(&temp.path().join("missing-home")),
            Some(&s(&temp.path().join("missing-workspace"))),
            &Env::new()
        )
        .is_empty());
    }
}
