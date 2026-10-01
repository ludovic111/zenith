//! The settings GitManager writes with: the acting project's effective settings
//! (`projectSettingsFor`), the writer model (`sourceControlWriterModelSelection`, falling back
//! to `textGenerationModelSelection`), and the writing style turned into a text generation
//! policy (`resolveStylePolicy`).

use std::path::{Path, MAIN_SEPARATOR};
use std::sync::Arc;

use serde_json::Value;
use zc_ports::text_generation::TextGenerationPolicy;
use zc_ports::{ProjectionReads, ProviderStatusReads, SettingsService};
use zc_textgen::policy::{conventional_commits_policy, custom_policy, repository_conventions_policy, CustomPolicyOverrides};
use zc_vcs::git_exec::ExecuteGitInput;
use zc_vcs::GitVcsDriver;

/// `PROJECT_SCOPED_SERVER_SETTING_KEYS`.
const PROJECT_SCOPED_KEYS: &[&str] = &[
    "worktreeCleanup",
    "defaultModelSelection",
    "defaultRuntimeMode",
    "defaultThreadEnvMode",
    "newWorktreesStartFromOrigin",
    "worktreeSubmodules",
    "defaultAutoPull",
    "defaultProjectScripts",
    "enableAgentBrowserAccess",
    "enableAgentDeviceAccess",
    "textGenerationModelSelection",
    "sourceControlWriterModelSelection",
    "sourceControlWritingStyle",
    "pullRequestMergeMethod",
    "sidebarAutoSettleOnMerge",
    "sidebarAutoSettleAfterDays",
    "continueThreadsAfterServerUpdate",
    "responseStreamingMode",
];

/// `hasProjectSettingsOverrides(settings)`.
pub fn has_project_settings_overrides(settings: &Value) -> bool {
    settings["projectSettingsOverrides"]
        .as_object()
        .into_iter()
        .flatten()
        .any(|(_, entry)| entry.as_object().is_some_and(|entry| !entry.is_empty()))
}

/// `resolveProjectSettings(settings, projectId).settings`.
pub fn resolve_project_settings(settings: &Value, project_id: Option<&str>) -> Value {
    let Some(overrides) = project_id
        .and_then(|id| settings["projectSettingsOverrides"].get(id))
        .and_then(Value::as_object)
    else {
        return settings.clone();
    };
    let mut effective = settings.clone();
    for key in PROJECT_SCOPED_KEYS {
        let Some(value) = overrides.get(*key) else { continue };
        if (*key == "textGenerationModelSelection" || *key == "defaultModelSelection")
            && !value.is_null()
            && !zc_settings::settings::logic::is_model_selection_provider_enabled(settings, value)
        {
            continue;
        }
        effective[*key] = value.clone();
    }
    effective
}

/// `SourceControlTextGenerationSettings`: the writer model and the writing style.
#[derive(Debug, Clone, PartialEq)]
pub struct WriterSettings {
    pub model_selection: Value,
    pub style: Value,
}

impl WriterSettings {
    pub fn style_mode(&self) -> &str {
        self.style["mode"].as_str().unwrap_or("repo_conventions")
    }

    pub fn custom_instructions(&self) -> &str {
        self.style["customInstructions"].as_str().unwrap_or("")
    }

    pub fn follow_change_request_templates(&self) -> bool {
        self.style["followChangeRequestTemplates"].as_bool().unwrap_or(true)
    }
}

/// The services the settings are read from.
#[derive(Clone)]
pub struct SettingsSources {
    pub settings: Arc<dyn SettingsService>,
    pub provider_status: Arc<dyn ProviderStatusReads>,
    /// Optional: git actions also run without orchestration (CLI, tests).
    pub projections: Option<Arc<dyn ProjectionReads>>,
}

impl SettingsSources {
    /// The wire JSON of the environment settings.
    pub async fn environment_settings(&self) -> Result<Value, String> {
        let settings = self.settings.get_settings().await.map_err(|error| format!("{error:?}"))?;
        serde_json::to_value(settings).map_err(|error| error.to_string())
    }

    /// `projectSettingsFor({cwd, threadId})`: the environment settings with the acting
    /// project's overrides (the thread's project, else the project rooted at `cwd`).
    pub async fn project_settings_for(&self, cwd: &str, thread_id: Option<&str>) -> Result<Value, String> {
        let settings = self.environment_settings().await?;
        let Some(projections) = self.projections.as_ref().filter(|_| has_project_settings_overrides(&settings)) else {
            return Ok(settings);
        };
        let project_id = match thread_id {
            Some(thread_id) => projections
                .get_thread_shell_by_id(&zc_contracts::ThreadId::new(thread_id))
                .await
                .ok()
                .flatten()
                .map(|thread| thread.project_id.to_string()),
            None => projections
                .get_active_project_by_workspace_root(cwd)
                .await
                .ok()
                .flatten()
                .map(|project| project.id.to_string()),
        };
        Ok(resolve_project_settings(&settings, project_id.as_deref()))
    }

    /// The provider statuses as wire JSON.
    pub async fn providers(&self) -> Vec<Value> {
        self.provider_status.get_providers().await.into_iter().map(|provider| provider.0).collect()
    }

    /// The writer settings of `runStackedAction`: `sourceControlWriterModelSelection` when set
    /// (and its provider usable), else `textGenerationModelSelection`.
    pub async fn writer_settings(&self, cwd: &str, thread_id: Option<&str>) -> Result<WriterSettings, String> {
        let settings = self.project_settings_for(cwd, thread_id).await?;
        let model_selection = if settings["sourceControlWriterModelSelection"].is_null() {
            settings["textGenerationModelSelection"].clone()
        } else {
            let providers = self.providers().await;
            zc_settings::settings::logic::resolve_source_control_writer_model_selection(&settings, Some(&providers))
        };
        Ok(WriterSettings {
            model_selection,
            style: settings["sourceControlWritingStyle"].clone(),
        })
    }
}

/// `readRepositoryInstructions(cwd, fileName)`: a regular file inside the checkout of at most
/// 20,000 bytes, trimmed; `""` otherwise.
pub async fn read_repository_instructions(cwd: &str, file_name: &str) -> String {
    async fn read(cwd: &str, file_name: &str) -> Option<String> {
        let root = tokio::fs::canonicalize(cwd).await.ok()?;
        let path = tokio::fs::canonicalize(Path::new(&root).join(file_name)).await.ok()?;
        let prefix = format!("{}{MAIN_SEPARATOR}", root.to_string_lossy());
        if !path.to_string_lossy().starts_with(&prefix) {
            return None;
        }
        let metadata = tokio::fs::metadata(&path).await.ok()?;
        if !metadata.is_file() || metadata.len() > 20_000 {
            return None;
        }
        let contents = tokio::fs::read_to_string(&path).await.ok()?;
        Some(zc_textgen::js::trim(&contents).to_owned())
    }
    read(cwd, file_name).await.unwrap_or_default()
}

/// `readRecentCommitSubjects(cwd)`: the last 20 non-merge subjects.
pub async fn read_recent_commit_subjects(git: &GitVcsDriver, cwd: &str) -> Vec<String> {
    match git
        .execute(ExecuteGitInput::new(
            "GitManager.readRecentCommitSubjects",
            cwd,
            ["log", "-n", "20", "--no-merges", "--pretty=format:%s"],
        ))
        .await
    {
        Ok(result) => result
            .stdout
            .split('\n')
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// `resolveStylePolicy(cwd, settings)`: the policy of the writing style. Repository conventions
/// add the recent commit subjects, `AGENTS.md`, and `CLAUDE.md` when Claude writes.
pub async fn resolve_style_policy(git: &GitVcsDriver, sources: &SettingsSources, cwd: &str, settings: &WriterSettings) -> TextGenerationPolicy {
    match settings.style_mode() {
        "conventional_commits" => conventional_commits_policy(),
        "custom" => {
            let instructions = settings.custom_instructions();
            custom_policy(if instructions.is_empty() {
                CustomPolicyOverrides::default()
            } else {
                CustomPolicyOverrides {
                    commit_instructions: Some(instructions.to_owned()),
                    change_request_instructions: Some(instructions.to_owned()),
                    ..CustomPolicyOverrides::default()
                }
            })
        }
        _ => {
            let subjects = read_recent_commit_subjects(git, cwd).await;
            let agents = read_repository_instructions(cwd, "AGENTS.md").await;
            let instance_id = settings.model_selection["instanceId"].as_str().unwrap_or("");
            let is_claude_writer = instance_id == "claudeAgent"
                || sources
                    .providers()
                    .await
                    .iter()
                    .any(|provider| provider["instanceId"].as_str() == Some(instance_id) && provider["driver"].as_str() == Some("claudeAgent"));
            let claude = if is_claude_writer {
                read_repository_instructions(cwd, "CLAUDE.md").await
            } else {
                String::new()
            };
            let mut examples = Vec::new();
            if !subjects.is_empty() {
                let mut lines = vec!["Recent commit subjects from this repository:".to_owned()];
                lines.extend(subjects);
                examples.push(lines.join("\n"));
            }
            if !agents.is_empty() {
                examples.push(format!("Local AGENTS.md:\n{agents}"));
            }
            if !claude.is_empty() {
                examples.push(format!("Local CLAUDE.md:\n{claude}"));
            }
            let base = repository_conventions_policy();
            if examples.is_empty() {
                return base;
            }
            let examples = examples.join("\n\n");
            TextGenerationPolicy {
                commit_instructions: Some(format!("{}\n\n{examples}", base.commit_instructions.as_deref().unwrap_or(""))),
                change_request_instructions: Some(format!("{}\n\n{examples}", base.change_request_instructions.as_deref().unwrap_or(""))),
                ..base
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn project_overrides_apply_to_writer_keys() {
        let settings = json!({
            "sourceControlWritingStyle": {"mode": "repo_conventions"},
            "projectSettingsOverrides": {"p1": {"sourceControlWritingStyle": {"mode": "custom", "customInstructions": "x"}}},
            "providerInstances": {},
            "providers": {},
        });
        assert!(has_project_settings_overrides(&settings));
        assert_eq!(
            resolve_project_settings(&settings, Some("p1"))["sourceControlWritingStyle"]["mode"],
            json!("custom")
        );
        assert_eq!(
            resolve_project_settings(&settings, Some("p2"))["sourceControlWritingStyle"]["mode"],
            json!("repo_conventions")
        );
        assert!(!has_project_settings_overrides(&json!({"projectSettingsOverrides": {"p": {}}})));
    }
}
