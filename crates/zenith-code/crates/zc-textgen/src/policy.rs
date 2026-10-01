//! `TextGenerationPolicy.ts` and `TextGenerationPresets.ts`. The policy types themselves are
//! the port's ([`zc_ports::text_generation::TextGenerationPolicy`]).

pub use zc_ports::text_generation::{TextGenerationPolicy, TextGenerationPolicyKind};

/// `conventionalCommitsTextGenerationPolicy`.
pub fn conventional_commits_policy() -> TextGenerationPolicy {
    TextGenerationPolicy {
        kind: TextGenerationPolicyKind::ConventionalCommits,
        commit_instructions: Some(
            "Use Conventional Commits when generating commit subjects. Prefer the narrowest accurate type and include a scope only when it is obvious from the diff."
                .into(),
        ),
        change_request_instructions: Some(
            "Keep the change request title concise. Do not force Conventional Commit syntax into the title unless the repository already uses it.".into(),
        ),
        branch_instructions: None,
        thread_title_instructions: None,
        infer_repository_conventions: false,
    }
}

/// `repositoryConventionsTextGenerationPolicy`.
pub fn repository_conventions_policy() -> TextGenerationPolicy {
    TextGenerationPolicy {
        kind: TextGenerationPolicyKind::RepoConventions,
        commit_instructions: Some("Follow the repository's established commit message style when examples are available.".into()),
        change_request_instructions: Some("Follow the repository's established change request title and body style when examples are available.".into()),
        branch_instructions: None,
        thread_title_instructions: None,
        infer_repository_conventions: true,
    }
}

/// The overridable fields of `customTextGenerationPolicy(overrides)`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CustomPolicyOverrides {
    pub commit_instructions: Option<String>,
    pub change_request_instructions: Option<String>,
    pub branch_instructions: Option<String>,
    pub thread_title_instructions: Option<String>,
    pub infer_repository_conventions: Option<bool>,
}

/// `customTextGenerationPolicy(overrides)`: kind `custom`, conventions not inferred unless asked.
pub fn custom_policy(overrides: CustomPolicyOverrides) -> TextGenerationPolicy {
    TextGenerationPolicy {
        kind: TextGenerationPolicyKind::Custom,
        commit_instructions: overrides.commit_instructions,
        change_request_instructions: overrides.change_request_instructions,
        branch_instructions: overrides.branch_instructions,
        thread_title_instructions: overrides.thread_title_instructions,
        infer_repository_conventions: overrides.infer_repository_conventions.unwrap_or(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_serialize_like_the_ts_constants() {
        assert_eq!(
            serde_json::to_value(repository_conventions_policy()).unwrap(),
            serde_json::json!({
                "kind": "repo_conventions",
                "commitInstructions": "Follow the repository's established commit message style when examples are available.",
                "changeRequestInstructions": "Follow the repository's established change request title and body style when examples are available.",
                "inferRepositoryConventions": true
            })
        );
        let custom = custom_policy(CustomPolicyOverrides {
            branch_instructions: Some("Prefix with the ticket.".into()),
            ..CustomPolicyOverrides::default()
        });
        assert_eq!(custom.kind, TextGenerationPolicyKind::Custom);
        assert!(!custom.infer_repository_conventions);
    }
}
