//! `git/GitManager.ts`: status with the branch's change request, stacked actions (commit,
//! push, create PR, with an optional feature branch and progress events), pull request
//! resolution and thread preparation (local checkout or a dedicated worktree), and the saved
//! branch lookup the settlement reactor uses.
//!
//! It implements zc-vcs's [`GitManagerBackend`], so `GitWorkflowService` (and through it the
//! `zc_ports::GitWorkflow` port, the RPCs and the reactors) reach it.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};
use zc_core::defect::Defect;
use zc_ports::contracts::{GitActionProgressEvent, ModelSelection, VcsStatusPullRequest};
use zc_ports::git::{GitBranchPullRequest, GitRunStackedActionOptions};
use zc_ports::text_generation::{CommitMessageGenerationInput, PrContentGenerationInput, TextGeneration};
use zc_ports::TaggedError;
use zc_sourcecontrol::provider::{CheckoutChangeRequestInput, CreateChangeRequestInput, DefaultBranchInput, GetChangeRequestInput, RepositoryCloneUrlsInput};
use zc_sourcecontrol::SourceControlProvider;
use zc_vcs::contracts::{VcsListRefsInput, VcsRef, VcsStatusLocalResult, VcsStatusRemoteResult, VcsStatusResult};
use zc_vcs::driver_core::{GitCommitOptions, GitCommitProgress, GitPushStatus, OutputStream};
use zc_vcs::shared_git::is_ssh_remote_url;
use zc_vcs::status::{canonicalize_existing_path, GitStatusService, RemoteStatusOptions};
use zc_vcs::workflow::GitManagerBackend;
use zc_vcs::{GitManagerError, GitManagerServiceError, GitVcsDriver};

use crate::helpers::*;
use crate::pr_lookup::PullRequestLookup;
use crate::providers::{kind_str, provider_error, SourceControlProviders};
use crate::settings::{resolve_style_policy, SettingsSources, WriterSettings};
use crate::types::*;

/// `COMMIT_TIMEOUT_MS`: hooks can be slow.
pub const COMMIT_TIMEOUT_MS: u64 = 10 * 60_000;

/// `ProjectSetupScriptRunner.runForThread` as preparing a PR worktree uses it.
#[async_trait]
pub trait PullRequestSetupScripts: Send + Sync {
    async fn run_for_thread(&self, thread_id: &str, project_cwd: &str, worktree_path: &str) -> Result<(), String>;
}

/// Where GitManager takes random ids from (`crypto.randomUUIDv4`).
pub type UuidSource = Arc<dyn Fn() -> String + Send + Sync>;

/// What GitManager is built from.
#[derive(Clone)]
pub struct GitManagerDeps {
    pub git: GitVcsDriver,
    pub providers: Arc<dyn SourceControlProviders>,
    pub text_generation: Arc<dyn TextGeneration>,
    pub settings: SettingsSources,
    pub setup_scripts: Option<Arc<dyn PullRequestSetupScripts>>,
    /// Where PR bodies are written for `--body-file` (`$TMPDIR`, `$TEMP`, `$TMP` or `/tmp`).
    pub temp_dir: PathBuf,
    pub uuids: UuidSource,
}

impl GitManagerDeps {
    /// `process.env.TMPDIR ?? process.env.TEMP ?? process.env.TMP ?? "/tmp"`.
    pub fn default_temp_dir() -> PathBuf {
        ["TMPDIR", "TEMP", "TMP"]
            .iter()
            .find_map(std::env::var_os)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"))
    }
}

/// `CommitAndBranchSuggestion`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Suggestion {
    subject: String,
    body: String,
    branch: Option<String>,
    commit_message: String,
}

/// `createProgressEmitter`: the reporter plus the action's context.
#[derive(Clone)]
struct Progress {
    action_id: String,
    cwd: String,
    action: GitStackedAction,
    reporter: Option<zc_ports::git::GitActionProgressReporter>,
}

impl Progress {
    fn emit(&self, payload: ProgressPayload) {
        if let Some(reporter) = &self.reporter {
            reporter(GitActionProgressEvent(payload.to_event(&self.action_id, &self.cwd, self.action)));
        }
    }
}

fn manager_error(operation: &str, cwd: &str, detail: impl Into<String>) -> GitManagerServiceError {
    GitManagerError::new(operation, cwd, detail).into()
}

fn text_generation_error(error: TaggedError) -> GitManagerServiceError {
    GitManagerServiceError::Other(error)
}

/// GitManager.
#[derive(Clone)]
pub struct GitManager {
    deps: GitManagerDeps,
    lookup: PullRequestLookup,
    status: GitStatusService,
}

impl GitManager {
    pub fn new(deps: GitManagerDeps) -> Self {
        let lookup = PullRequestLookup::new(deps.git.clone(), deps.providers.clone());
        let status = GitStatusService::new(deps.git.clone(), Arc::new(lookup.clone()));
        Self { deps, lookup, status }
    }

    pub fn lookup(&self) -> &PullRequestLookup {
        &self.lookup
    }

    pub fn status_service(&self) -> &GitStatusService {
        &self.status
    }

    fn git(&self) -> &GitVcsDriver {
        &self.deps.git
    }

    async fn provider(&self, cwd: &str) -> Result<Arc<dyn SourceControlProvider>, GitManagerServiceError> {
        self.deps.providers.resolve(cwd).await.map_err(provider_error)
    }

    /// The change request words of the checkout's forge (`unknown` when it cannot be told).
    async fn terminology(&self, cwd: &str) -> ChangeRequestTerminology {
        match self.deps.providers.resolve(cwd).await {
            Ok(provider) => change_request_terminology(kind_str(&*provider)),
            Err(_) => change_request_terminology("unknown"),
        }
    }

    // ------------------------------------------------------------------------------------------
    // Status
    // ------------------------------------------------------------------------------------------

    pub async fn status(&self, cwd: &str) -> Result<VcsStatusResult, GitManagerServiceError> {
        self.status.status(cwd).await
    }

    pub async fn local_status(&self, cwd: &str) -> Result<VcsStatusLocalResult, GitManagerServiceError> {
        self.status.local_status(cwd).await
    }

    pub async fn remote_status(&self, cwd: &str, options: RemoteStatusOptions) -> Result<Option<VcsStatusRemoteResult>, GitManagerServiceError> {
        self.status.remote_status(cwd, options).await
    }

    /// `invalidateStatus`: both result caches and the PR-lookup epoch (explicit freshness).
    pub async fn invalidate_status(&self, cwd: &str) {
        self.status.invalidate_status(cwd).await
    }

    /// `branchPullRequest({cwd, branch}, {refresh})`.
    pub async fn branch_pull_request(&self, cwd: &str, branch: &str, refresh: bool) -> Result<Option<GitBranchPullRequest>, GitManagerServiceError> {
        Ok(self.lookup.branch_pull_request(cwd, branch, refresh).await?.map(|found| GitBranchPullRequest {
            pull_request: VcsStatusPullRequest(serde_json::to_value(&found.pull_request).unwrap_or(Value::Null)),
            repository_key: found.repository_key,
            updated_at: found.updated_at,
            closed_at: Some(found.closed_at),
            merged_at: Some(found.merged_at),
        }))
    }

    // ------------------------------------------------------------------------------------------
    // Commit message and branch suggestions
    // ------------------------------------------------------------------------------------------

    /// `resolveCommitAndBranchSuggestion`: stages the changes (all, or `file_paths`), then uses
    /// the custom message or generates one. `None` when nothing is staged.
    async fn resolve_suggestion(
        &self,
        cwd: &str,
        branch: Option<&str>,
        commit_message: Option<&str>,
        include_branch: bool,
        file_paths: Option<&[String]>,
        settings: &WriterSettings,
    ) -> Result<Option<Suggestion>, GitManagerServiceError> {
        let Some(context) = self.git().prepare_commit_context(cwd, file_paths).await? else {
            return Ok(None);
        };
        if let Some((subject, body)) = parse_custom_commit_message(commit_message.unwrap_or("")) {
            return Ok(Some(Suggestion {
                branch: include_branch.then(|| zc_textgen::utils::sanitize_feature_branch_name(&subject)),
                commit_message: format_commit_message(&subject, &body),
                subject,
                body,
            }));
        }
        let policy = resolve_style_policy(self.git(), &self.deps.settings, cwd, settings).await;
        let generated = self
            .deps
            .text_generation
            .generate_commit_message(CommitMessageGenerationInput {
                cwd: cwd.to_owned(),
                branch: branch.map(str::to_owned),
                staged_summary: limit_context(&context.staged_summary, 8_000),
                staged_patch: limit_context(&context.staged_patch, 50_000),
                include_branch,
                policy: Some(policy),
                model_selection: ModelSelection(settings.model_selection.clone()),
            })
            .await
            .map_err(text_generation_error)?;
        let (subject, body) = sanitize_commit_message(&generated.subject, &generated.body);
        Ok(Some(Suggestion {
            branch: generated.branch,
            commit_message: format_commit_message(&subject, &body),
            subject,
            body,
        }))
    }

    // ------------------------------------------------------------------------------------------
    // Stacked action steps
    // ------------------------------------------------------------------------------------------

    /// `runFeatureBranchStep`: a fresh `feature/…` branch named after the commit, checked out.
    async fn run_feature_branch_step(
        &self,
        settings: &WriterSettings,
        cwd: &str,
        branch: Option<&str>,
        commit_message: Option<&str>,
        file_paths: Option<&[String]>,
    ) -> Result<(BranchStep, Suggestion), GitManagerServiceError> {
        let Some(suggestion) = self.resolve_suggestion(cwd, branch, commit_message, true, file_paths, settings).await? else {
            return Err(manager_error(
                "runFeatureBranchStep",
                cwd,
                "Cannot create a feature branch because there are no changes to commit.",
            ));
        };
        let preferred = suggestion
            .branch
            .clone()
            .unwrap_or_else(|| zc_textgen::utils::sanitize_feature_branch_name(&suggestion.subject));
        let existing = self.git().list_local_branch_names(cwd).await?;
        let resolved = resolve_auto_feature_branch_name(&existing, Some(&preferred));
        self.git()
            .create_ref(&serde_json::from_value(json!({ "cwd": cwd, "refName": resolved })).expect("valid create ref input"))
            .await?;
        self.git()
            .switch_ref(&serde_json::from_value(json!({ "cwd": cwd, "refName": resolved })).expect("valid switch ref input"))
            .await?;
        Ok((
            BranchStep {
                status: "created".into(),
                name: Some(resolved),
            },
            suggestion,
        ))
    }

    /// `runCommitStep`.
    #[allow(clippy::too_many_arguments)]
    async fn run_commit_step(
        &self,
        settings: &WriterSettings,
        cwd: &str,
        branch: Option<&str>,
        commit_message: Option<&str>,
        pre_resolved: Option<Suggestion>,
        file_paths: Option<&[String]>,
        progress: &Progress,
    ) -> Result<CommitStep, GitManagerServiceError> {
        let suggestion = match pre_resolved {
            Some(suggestion) => Some(suggestion),
            None => {
                if commit_message.map(zc_textgen::js::trim).unwrap_or("").is_empty() {
                    progress.emit(ProgressPayload::PhaseStarted {
                        phase: GitActionProgressPhase::Commit,
                        label: "Generating commit message...".into(),
                    });
                }
                self.resolve_suggestion(cwd, branch, commit_message, false, file_paths, settings).await?
            }
        };
        let Some(suggestion) = suggestion else {
            return Ok(CommitStep::with_status("skipped_no_changes"));
        };
        progress.emit(ProgressPayload::PhaseStarted {
            phase: GitActionProgressPhase::Commit,
            label: "Committing...".into(),
        });

        let current_hook: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let commit_progress = progress.reporter.as_ref().map(|_| {
            let (on_line, on_started, on_finished) = (progress.clone(), progress.clone(), progress.clone());
            let (line_hook, started_hook, finished_hook) = (current_hook.clone(), current_hook.clone(), current_hook.clone());
            GitCommitProgress {
                on_output_line: Some(Arc::new(move |stream: OutputStream, text: &str| {
                    let Some(text) = sanitize_progress_text(text) else { return };
                    let hook_name = line_hook.lock().unwrap_or_else(|p| p.into_inner()).clone();
                    on_line.emit(ProgressPayload::HookOutput {
                        hook_name,
                        stream: match stream {
                            OutputStream::Stdout => "stdout",
                            OutputStream::Stderr => "stderr",
                        },
                        text,
                    });
                })),
                on_hook_started: Some(Arc::new(move |hook_name: &str| {
                    *started_hook.lock().unwrap_or_else(|p| p.into_inner()) = Some(hook_name.to_owned());
                    on_started.emit(ProgressPayload::HookStarted {
                        hook_name: hook_name.to_owned(),
                    });
                })),
                on_hook_finished: Some(Arc::new(move |finished: zc_vcs::git_exec::HookFinished| {
                    {
                        let mut current = finished_hook.lock().unwrap_or_else(|p| p.into_inner());
                        if current.as_deref() == Some(finished.hook_name.as_str()) {
                            *current = None;
                        }
                    }
                    on_finished.emit(ProgressPayload::HookFinished {
                        hook_name: finished.hook_name,
                        exit_code: finished.exit_code,
                        duration_ms: finished.duration_ms,
                    });
                })),
            }
        });
        let commit_sha = self
            .git()
            .commit(
                cwd,
                &suggestion.subject,
                &suggestion.body,
                GitCommitOptions {
                    timeout_ms: Some(COMMIT_TIMEOUT_MS),
                    progress: commit_progress,
                },
            )
            .await?;
        let unfinished = current_hook.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(hook_name) = unfinished {
            progress.emit(ProgressPayload::HookFinished {
                hook_name,
                exit_code: Some(0),
                duration_ms: None,
            });
        }
        Ok(CommitStep {
            status: "created".into(),
            commit_sha: Some(commit_sha),
            subject: Some(suggestion.subject),
        })
    }

    /// `resolveBaseBranch(cwd, branch, upstreamRef, headContext)`.
    async fn resolve_base_branch(
        &self,
        cwd: &str,
        branch: &str,
        upstream_ref: Option<&str>,
        head: &BranchHeadContext,
    ) -> Result<String, GitManagerServiceError> {
        if let Some(configured) = self
            .git()
            .read_config_value(cwd, &format!("branch.{branch}.gh-merge-base"))
            .await?
            .filter(|value| !value.is_empty())
        {
            return Ok(configured);
        }
        if let Some(upstream) = upstream_ref.filter(|upstream| !upstream.is_empty()) {
            if !head.is_cross_repository {
                let upstream_branch = zc_vcs::remote_refs::extract_branch_name_from_remote_ref(upstream, head.remote_name.as_deref(), &[]);
                if !upstream_branch.is_empty() && upstream_branch != branch {
                    return Ok(upstream_branch);
                }
            }
        }
        if let Ok(provider) = self.deps.providers.resolve(cwd).await {
            if let Ok(Some(default)) = provider
                .get_default_branch(DefaultBranchInput {
                    cwd: cwd.to_owned(),
                    context: None,
                })
                .await
            {
                if !default.is_empty() {
                    return Ok(default);
                }
            }
        }
        // The provider lookup can fail for unrelated reasons: ask the remote before guessing.
        if let Ok(remote) = self.git().resolve_primary_remote_name(cwd).await {
            if let Ok(Some(default)) = self.git().resolve_default_branch_name(cwd, &remote).await {
                if !default.is_empty() {
                    return Ok(default);
                }
            }
        }
        Ok("main".into())
    }

    /// `resolveBaseRangeRef(cwd, baseBranch)`: the remote-tracking commit of the base.
    async fn resolve_base_range_ref(&self, cwd: &str, base_branch: &str) -> String {
        let Ok(remote) = self.git().resolve_primary_remote_name(cwd).await else {
            return base_branch.to_owned();
        };
        match self.git().resolve_remote_tracking_commit(cwd, base_branch, &remote).await {
            Ok(resolved) => resolved.commit_sha,
            Err(_) => base_branch.to_owned(),
        }
    }

    /// `runPrStep`.
    async fn run_pr_step(
        &self,
        settings: &WriterSettings,
        cwd: &str,
        fallback_branch: Option<&str>,
        progress: &Progress,
    ) -> Result<PrStep, GitManagerServiceError> {
        let provider = self.provider(cwd).await?;
        let kind = kind_str(&*provider);
        let terms = change_request_terminology(kind);
        let details = self.git().status_details(cwd).await?;
        let Some(branch) = details.branch.clone().or_else(|| fallback_branch.map(str::to_owned)) else {
            return Err(manager_error("runPrStep", cwd, "Cannot create a pull request from detached HEAD."));
        };
        if !details.has_upstream {
            return Err(manager_error(
                "runPrStep",
                cwd,
                "Current branch has not been pushed. Push before creating a PR.",
            ));
        }
        let head = self
            .lookup
            .resolve_branch_head_context(cwd, &branch, details.upstream_ref.as_deref(), None)
            .await;
        if let Some(existing) = self.lookup.find_open_pr(cwd, &head).await? {
            return Ok(PrStep {
                status: "opened_existing".into(),
                url: Some(existing.url),
                number: Some(existing.number),
                base_branch: Some(existing.base_ref_name),
                head_branch: Some(existing.head_ref_name),
                title: Some(existing.title),
            });
        }

        let base_branch = self.resolve_base_branch(cwd, &branch, details.upstream_ref.as_deref(), &head).await?;
        progress.emit(ProgressPayload::PhaseStarted {
            phase: GitActionProgressPhase::Pr,
            label: format!("Generating {} content...", terms.short_label),
        });
        let base_range_ref = self.resolve_base_range_ref(cwd, &base_branch).await;
        let range = self.git().read_range_context(cwd, &base_range_ref).await?;
        let policy = resolve_style_policy(self.git(), &self.deps.settings, cwd, settings).await;
        let template = if settings.follow_change_request_templates() && kind == "github" {
            zc_sourcecontrol::pr_template::detect_pr_template(cwd, &base_range_ref, self.git()).await
        } else {
            None
        };
        let generated = self
            .deps
            .text_generation
            .generate_pr_content(PrContentGenerationInput {
                cwd: cwd.to_owned(),
                base_branch: base_branch.clone(),
                head_branch: head.head_branch.clone(),
                commit_summary: limit_context(&range.commit_summary, 20_000),
                diff_summary: limit_context(&range.diff_summary, 20_000),
                diff_patch: limit_context(&range.diff_patch, 60_000),
                change_request_template: template,
                policy: Some(policy),
                model_selection: ModelSelection(settings.model_selection.clone()),
            })
            .await
            .map_err(text_generation_error)?;

        let body_file = self
            .deps
            .temp_dir
            .join(format!("t3code-pr-body-{}-{}.md", std::process::id(), (self.deps.uuids)()));
        tokio::fs::write(&body_file, &generated.body).await.map_err(|error| {
            GitManagerError::new("runPrStep", cwd, "Failed to write pull request body temp file.").with_cause(zc_vcs::errors::platform_error_defect(
                "writeFileString",
                &body_file.to_string_lossy(),
                &error,
            ))
        })?;
        progress.emit(ProgressPayload::PhaseStarted {
            phase: GitActionProgressPhase::Pr,
            label: format!("Creating {}...", terms.singular),
        });
        let created = provider
            .create_change_request(CreateChangeRequestInput {
                cwd: cwd.to_owned(),
                context: None,
                source: None,
                target: None,
                base_ref_name: base_branch.clone(),
                head_selector: head.preferred_head_selector.clone(),
                title: generated.title.clone(),
                body_file: body_file.to_string_lossy().into_owned(),
            })
            .await;
        let _ = tokio::fs::remove_file(&body_file).await;
        created.map_err(provider_error)?;

        match self.lookup.find_open_pr(cwd, &head).await? {
            None => Ok(PrStep {
                status: "created".into(),
                url: None,
                number: None,
                base_branch: Some(base_branch),
                head_branch: Some(head.head_branch),
                title: Some(generated.title),
            }),
            Some(created) => Ok(PrStep {
                status: "created".into(),
                url: Some(created.url),
                number: Some(created.number),
                base_branch: Some(created.base_ref_name),
                head_branch: Some(created.head_ref_name),
                title: Some(created.title),
            }),
        }
    }

    /// `buildCompletionToast(cwd, result)`.
    async fn build_completion_toast(
        &self,
        cwd: &str,
        action: GitStackedAction,
        branch: &BranchStep,
        commit: &CommitStep,
        push: &PushStep,
        pr: &PrStep,
    ) -> Result<Toast, GitManagerServiceError> {
        let terms = self.terminology(cwd).await;
        let (title, description) = summarize(commit, push, pr, terms);
        let mut current_is_default = false;
        let mut final_branch: Option<(String, Option<String>, bool)> = None;
        if action != GitStackedAction::Commit {
            let status = self.git().status_details(cwd).await?;
            if let Some(name) = status.branch.clone() {
                final_branch = Some((name, status.upstream_ref.clone(), status.has_upstream));
                current_is_default = status.is_default_branch;
            }
        }
        let explicit_pr_url = if pr.has_pull_request() {
            pr.url.clone().filter(|url| !url.is_empty())
        } else {
            None
        };
        let should_lookup_existing = matches!(action, GitStackedAction::CommitPush | GitStackedAction::Push)
            && push.status == "pushed"
            && branch.status != "created"
            && !current_is_default
            && explicit_pr_url.is_none()
            && final_branch.as_ref().is_some_and(|(_, _, has_upstream)| *has_upstream);
        let mut open_pr_url: Option<String> = None;
        if should_lookup_existing {
            if let Some((name, upstream, _)) = &final_branch {
                let head = self.lookup.resolve_branch_head_context(cwd, name, upstream.as_deref(), None).await;
                open_pr_url = self.lookup.find_open_pr(cwd, &head).await.ok().flatten().map(|found| found.url);
            }
        }
        let open_pr_url = open_pr_url.or(explicit_pr_url);

        let cta = if action == GitStackedAction::Commit && commit.status == "created" {
            ToastCta::RunAction {
                label: "Push".into(),
                action: ToastRunAction { kind: GitStackedAction::Push },
            }
        } else if matches!(
            action,
            GitStackedAction::Push | GitStackedAction::CreatePr | GitStackedAction::CommitPush | GitStackedAction::CommitPushPr
        ) && open_pr_url.as_deref().is_some_and(|url| !url.is_empty())
            && (!current_is_default || pr.has_pull_request())
        {
            ToastCta::OpenPr {
                label: format!("View {}", terms.short_label),
                url: open_pr_url.unwrap_or_default(),
            }
        } else if matches!(action, GitStackedAction::Push | GitStackedAction::CommitPush) && push.status == "pushed" && !current_is_default {
            ToastCta::RunAction {
                label: format!("Create {}", terms.short_label),
                action: ToastRunAction {
                    kind: GitStackedAction::CreatePr,
                },
            }
        } else {
            ToastCta::None
        };
        Ok(Toast { title, description, cta })
    }

    // ------------------------------------------------------------------------------------------
    // runStackedAction
    // ------------------------------------------------------------------------------------------

    /// `runStackedAction(input, options)`: the steps, in order, with progress events; the
    /// status caches are invalidated whatever happens, and a failure emits `action_failed`
    /// naming the phase it happened in.
    pub async fn run_stacked_action(
        &self,
        input: GitRunStackedActionInput,
        options: GitRunStackedActionOptions,
    ) -> Result<GitRunStackedActionResult, GitManagerServiceError> {
        let progress = Progress {
            action_id: options.action_id.clone().unwrap_or_else(|| (self.deps.uuids)()),
            cwd: input.cwd.clone(),
            action: input.action,
            reporter: options.progress_reporter.clone(),
        };
        let phase: Mutex<Option<GitActionProgressPhase>> = Mutex::new(None);
        let result = self.run_action(&input, &progress, &phase).await;
        self.invalidate_status(&input.cwd).await;
        if let Err(error) = &result {
            progress.emit(ProgressPayload::ActionFailed {
                phase: *phase.lock().unwrap_or_else(|p| p.into_inner()),
                message: error.message(),
            });
        }
        result
    }

    async fn run_action(
        &self,
        input: &GitRunStackedActionInput,
        progress: &Progress,
        phase: &Mutex<Option<GitActionProgressPhase>>,
    ) -> Result<GitRunStackedActionResult, GitManagerServiceError> {
        let set_phase = |next: GitActionProgressPhase| *phase.lock().unwrap_or_else(|p| p.into_inner()) = Some(next);
        let cwd = input.cwd.as_str();
        let action = input.action;
        let initial = self.git().status_details(cwd).await?;
        let wants_commit = action.is_commit();
        let wants_push = matches!(action, GitStackedAction::Push | GitStackedAction::CommitPush | GitStackedAction::CommitPushPr)
            || (action == GitStackedAction::CreatePr && (!initial.has_upstream || initial.ahead_count > 0));
        let wants_pr = matches!(action, GitStackedAction::CreatePr | GitStackedAction::CommitPushPr);
        let feature_branch = input.feature_branch == Some(true);

        if feature_branch && !wants_commit {
            return Err(manager_error(
                "runStackedAction",
                cwd,
                "Feature-branch checkout is only supported for commit actions.",
            ));
        }
        if action == GitStackedAction::CreatePr && initial.has_working_tree_changes {
            return Err(manager_error("runStackedAction", cwd, "Commit local changes before creating a PR."));
        }

        let mut phases = Vec::new();
        if feature_branch {
            phases.push(GitActionProgressPhase::Branch);
        }
        if wants_commit {
            phases.push(GitActionProgressPhase::Commit);
        }
        if wants_push {
            phases.push(GitActionProgressPhase::Push);
        }
        if wants_pr {
            phases.push(GitActionProgressPhase::Pr);
        }
        progress.emit(ProgressPayload::ActionStarted { phases });

        if !feature_branch && wants_push && initial.branch.is_none() {
            return Err(manager_error("runStackedAction", cwd, "Cannot push from detached HEAD."));
        }
        if !feature_branch && wants_pr && initial.branch.is_none() {
            return Err(manager_error("runStackedAction", cwd, "Cannot create a pull request from detached HEAD."));
        }

        let settings = self
            .deps
            .settings
            .writer_settings(cwd, input.thread_id.as_deref())
            .await
            .map_err(|cause| GitManagerError::new("runStackedAction", cwd, "Failed to get server settings.").with_cause(Defect::error("Error", cause)))?;

        let mut commit_message = input.commit_message.clone();
        let mut pre_resolved = None;
        let branch_step = if feature_branch {
            set_phase(GitActionProgressPhase::Branch);
            progress.emit(ProgressPayload::PhaseStarted {
                phase: GitActionProgressPhase::Branch,
                label: "Preparing feature branch...".into(),
            });
            let (step, suggestion) = self
                .run_feature_branch_step(
                    &settings,
                    cwd,
                    initial.branch.as_deref(),
                    input.commit_message.as_deref(),
                    input.file_paths.as_deref(),
                )
                .await?;
            commit_message = Some(suggestion.commit_message.clone());
            pre_resolved = Some(suggestion);
            step
        } else {
            BranchStep::skipped()
        };

        let current_branch = branch_step.name.clone().or_else(|| initial.branch.clone());
        let pr_terms = if wants_pr { Some(self.terminology(cwd).await) } else { None };

        let commit = if wants_commit {
            set_phase(GitActionProgressPhase::Commit);
            self.run_commit_step(
                &settings,
                cwd,
                current_branch.as_deref(),
                commit_message.as_deref(),
                pre_resolved,
                input.file_paths.as_deref(),
                progress,
            )
            .await?
        } else {
            CommitStep::with_status("skipped_not_requested")
        };

        let push = if wants_push {
            progress.emit(ProgressPayload::PhaseStarted {
                phase: GitActionProgressPhase::Push,
                label: "Pushing...".into(),
            });
            set_phase(GitActionProgressPhase::Push);
            let pushed = self.git().push_current_branch(cwd, current_branch.as_deref(), None).await?;
            PushStep {
                status: match pushed.status {
                    GitPushStatus::Pushed => "pushed".into(),
                    GitPushStatus::SkippedUpToDate => "skipped_up_to_date".into(),
                },
                branch: Some(pushed.branch),
                upstream_branch: pushed.upstream_branch,
                set_upstream: pushed.set_upstream,
            }
        } else {
            PushStep::skipped()
        };

        let pr = if wants_pr {
            progress.emit(ProgressPayload::PhaseStarted {
                phase: GitActionProgressPhase::Pr,
                label: format!("Preparing {}...", pr_terms.map_or("PR", |terms| terms.short_label)),
            });
            set_phase(GitActionProgressPhase::Pr);
            self.run_pr_step(&settings, cwd, current_branch.as_deref(), progress).await?
        } else {
            PrStep::skipped()
        };

        let toast = self.build_completion_toast(cwd, action, &branch_step, &commit, &push, &pr).await?;
        let result = GitRunStackedActionResult {
            action,
            branch: branch_step,
            commit,
            push,
            pr,
            toast,
        };
        progress.emit(ProgressPayload::ActionFinished {
            result: Box::new(result.clone()),
        });
        Ok(result)
    }

    // ------------------------------------------------------------------------------------------
    // Pull requests
    // ------------------------------------------------------------------------------------------

    async fn get_change_request(&self, cwd: &str, reference: &str) -> Result<zc_contracts::ChangeRequest, GitManagerServiceError> {
        self.provider(cwd)
            .await?
            .get_change_request(GetChangeRequestInput {
                cwd: cwd.to_owned(),
                context: None,
                reference: reference.to_owned(),
            })
            .await
            .map_err(provider_error)
    }

    /// `resolvePullRequest({cwd, reference})`.
    pub async fn resolve_pull_request(&self, input: &GitPullRequestRefInput) -> Result<Value, GitManagerServiceError> {
        let summary = self.get_change_request(&input.cwd, &normalize_pull_request_reference(&input.reference)).await?;
        Ok(json!({ "pullRequest": ResolvedPullRequest::from_change_request(&summary).to_wire() }))
    }

    /// The preferred remote name and URL of a pull request's head repository.
    async fn head_remote(&self, cwd: &str, pr: &ResolvedPullRequest, repository: &str) -> Result<String, GitManagerServiceError> {
        let clone_urls = self
            .provider(cwd)
            .await?
            .get_repository_clone_urls(RepositoryCloneUrlsInput {
                cwd: cwd.to_owned(),
                context: None,
                repository: repository.to_owned(),
            })
            .await
            .map_err(provider_error)?;
        let origin_url = self.git().read_config_value(cwd, "remote.origin.url").await?;
        let remote_url = if origin_url.as_deref().is_some_and(is_ssh_remote_url) {
            clone_urls.ssh_url.clone()
        } else {
            clone_urls.url.clone()
        };
        let preferred = [
            pr.head_repository_owner_login.as_deref().map(zc_textgen::js::trim).unwrap_or(""),
            zc_textgen::js::trim(repository.split('/').next().unwrap_or("")),
        ]
        .into_iter()
        .find(|name| !name.is_empty())
        .unwrap_or("fork")
        .to_owned();
        Ok(self.git().ensure_remote(cwd, &preferred, &remote_url).await?)
    }

    /// `configurePullRequestHeadUpstream`: track the head branch (best effort, only logged).
    async fn configure_head_upstream(&self, cwd: &str, pr: &ResolvedPullRequest, local_branch: &str) {
        let result: Result<(), GitManagerServiceError> = async {
            let repository = pr.head_repository().unwrap_or_default();
            if repository.is_empty() && pr.is_cross_repository != Some(true) {
                let remote = self.git().resolve_primary_remote_name(cwd).await?;
                self.git().fetch_remote_tracking_branch(cwd, &remote, &pr.head_branch).await?;
                self.git().set_branch_upstream(cwd, local_branch, &remote, &pr.head_branch).await?;
                return Ok(());
            }
            if repository.is_empty() {
                return Ok(());
            }
            let remote = self.head_remote(cwd, pr, &repository).await?;
            self.git().fetch_remote_tracking_branch(cwd, &remote, &pr.head_branch).await?;
            self.git().set_branch_upstream(cwd, local_branch, &remote, &pr.head_branch).await?;
            Ok(())
        }
        .await;
        if let Err(error) = result {
            tracing::warn!(cwd, local_branch, head_branch = pr.head_branch, cause = %error, "GitManager.configurePullRequestHeadUpstream failed");
        }
    }

    /// `materializePullRequestHeadBranch`: the head as `local_branch`, from the head repository,
    /// else from the pull request ref.
    async fn materialize_head_branch(&self, cwd: &str, pr: &ResolvedPullRequest, local_branch: &str) -> Result<(), GitManagerServiceError> {
        let primary: Result<(), GitManagerServiceError> = async {
            let repository = pr.head_repository().unwrap_or_default();
            if repository.is_empty() {
                self.git().fetch_pull_request_branch(cwd, pr.number.max(0) as u64, local_branch).await?;
                return Ok(());
            }
            let remote = self.head_remote(cwd, pr, &repository).await?;
            self.git().fetch_remote_branch(cwd, &remote, &pr.head_branch, local_branch).await?;
            self.git().set_branch_upstream(cwd, local_branch, &remote, &pr.head_branch).await?;
            Ok(())
        }
        .await;
        let Err(primary) = primary else {
            return Ok(());
        };
        match self.git().fetch_pull_request_branch(cwd, pr.number.max(0) as u64, local_branch).await {
            Ok(()) => Ok(()),
            Err(fallback) => {
                let primary_json = serde_json::to_value(&primary).unwrap_or(Value::Null);
                let fallback_json = serde_json::to_value(&fallback).unwrap_or(Value::Null);
                let message = format!("Repository-head and pull-request-ref fetches both failed for pull request #{}.", pr.number);
                let cause = json!({
                    "name": "AggregateError",
                    "message": message,
                    "errors": [primary_json.clone(), fallback_json],
                    "cause": primary_json,
                });
                let mut fields = serde_json::Map::new();
                fields.insert("cwd".into(), json!(cwd));
                fields.insert("pullRequestNumber".into(), json!(pr.number));
                fields.insert("headRepository".into(), json!(pr.head_repository()));
                fields.insert("headBranch".into(), json!(pr.head_branch));
                fields.insert("localBranch".into(), json!(local_branch));
                fields.insert("cause".into(), cause);
                Err(GitManagerServiceError::Other(TaggedError {
                    tag: "GitPullRequestMaterializationError".into(),
                    fields,
                    message: format!(
                        "Failed to materialize pull request #{} branch {} as {}.",
                        pr.number, pr.head_branch, local_branch
                    ),
                }))
            }
        }
    }

    /// `preparePullRequestThread(input)`.
    pub async fn prepare_pull_request_thread(&self, input: &GitPreparePullRequestThreadInput) -> Result<Value, GitManagerServiceError> {
        let result = self.prepare_inner(input).await;
        self.invalidate_status(&input.cwd).await;
        result
    }

    async fn maybe_run_setup_script(&self, input: &GitPreparePullRequestThreadInput, worktree_path: &str) {
        let (Some(thread_id), Some(scripts)) = (input.thread_id.as_deref(), self.deps.setup_scripts.as_ref()) else {
            return;
        };
        if let Err(cause) = scripts.run_for_thread(thread_id, &input.cwd, worktree_path).await {
            tracing::warn!(thread_id, worktree_path, cause, "GitManager.preparePullRequestThread setup script failed");
        }
    }

    async fn prepare_inner(&self, input: &GitPreparePullRequestThreadInput) -> Result<Value, GitManagerServiceError> {
        let cwd = input.cwd.as_str();
        let reference = normalize_pull_request_reference(&input.reference);
        let root = canonicalize_existing_path(cwd).await;
        let summary = self.get_change_request(cwd, &reference).await?;
        let pr = ResolvedPullRequest::from_change_request(&summary);
        let answer = |branch: &str, worktree_path: Option<&str>, on_head: bool| {
            json!({
                "pullRequest": pr.to_wire(),
                "branch": branch,
                "worktreePath": worktree_path,
                "isOnPullRequestHead": on_head,
            })
        };

        if input.mode == PreparePullRequestThreadMode::Local {
            self.provider(cwd)
                .await?
                .checkout_change_request(CheckoutChangeRequestInput {
                    cwd: cwd.to_owned(),
                    context: None,
                    reference: reference.clone(),
                    force: true,
                })
                .await
                .map_err(provider_error)?;
            let details = self.git().status_details(cwd).await?;
            let branch = details.branch.clone().unwrap_or_else(|| pr.head_branch.clone());
            self.configure_head_upstream(cwd, &pr, &branch).await;
            return Ok(answer(&branch, None, true));
        }

        let local_branch = resolve_pull_request_worktree_local_branch_name(pr.number, &pr.head_branch, pr.is_cross_repository);

        if let Some(found) = self.find_local_head_branch(cwd, &root, &pr, &local_branch).await? {
            if let Some(answer) = self.reuse_or_reject(input, &root, &pr, &local_branch, &found).await? {
                return Ok(answer);
            }
        }

        self.materialize_head_branch(cwd, &pr, &local_branch).await?;

        if let Some(found) = self.find_local_head_branch(cwd, &root, &pr, &local_branch).await? {
            if let Some(answer) = self.reuse_or_reject(input, &root, &pr, &local_branch, &found).await? {
                return Ok(answer);
            }
        }

        // Best effort: a settings read failure falls back to the checkout's t3.json.
        let submodules = self
            .deps
            .settings
            .project_settings_for(cwd, input.thread_id.as_deref())
            .await
            .ok()
            .and_then(|settings| serde_json::from_value(settings["worktreeSubmodules"].clone()).ok());
        let worktree = self
            .git()
            .create_worktree(
                &serde_json::from_value(json!({ "cwd": cwd, "refName": local_branch, "path": null })).expect("valid worktree input"),
                &zc_ports::git::CreateWorktreeOptions {
                    submodules,
                    ..Default::default()
                },
            )
            .await?;
        self.ensure_existing_worktree_upstream(&worktree.worktree.path, &pr).await?;
        self.maybe_run_setup_script(input, &worktree.worktree.path).await;
        Ok(answer(&worktree.worktree.ref_name, Some(&worktree.worktree.path), true))
    }

    /// The reuse paths of `preparePullRequestThread`: a branch checked out in another worktree
    /// is reused; one checked out in the main repo is an error. `None`: not checked out.
    async fn reuse_or_reject(
        &self,
        input: &GitPreparePullRequestThreadInput,
        root: &str,
        pr: &ResolvedPullRequest,
        local_branch: &str,
        found: &VcsRef,
    ) -> Result<Option<Value>, GitManagerServiceError> {
        let Some(worktree_path) = found.worktree_path.as_deref() else {
            return Ok(None);
        };
        let canonical = canonicalize_existing_path(worktree_path).await;
        if canonical == root {
            return Err(manager_error(
                "preparePullRequestThread",
                &input.cwd,
                "This PR branch is already checked out in the main repo. Use Local, or switch the main repo off that branch before creating a worktree thread.",
            ));
        }
        Ok(Some(self.reuse_existing_worktree(input, pr, local_branch, worktree_path, &found.name).await?))
    }

    /// `ensureExistingWorktreeUpstream(worktreePath)`.
    async fn ensure_existing_worktree_upstream(&self, worktree_path: &str, pr: &ResolvedPullRequest) -> Result<(), GitManagerServiceError> {
        let details = self.git().status_details(worktree_path).await?;
        let branch = details.branch.unwrap_or_else(|| pr.head_branch.clone());
        self.configure_head_upstream(worktree_path, pr, &branch).await;
        Ok(())
    }

    /// `reuseExistingWorktree(worktreePath, checkedOutBranch)`: advance the checkout from inside
    /// the worktree (git refuses to move a branch checked out elsewhere); a checkout that cannot
    /// move is still handed back, flagged as not on the head.
    async fn reuse_existing_worktree(
        &self,
        input: &GitPreparePullRequestThreadInput,
        pr: &ResolvedPullRequest,
        local_branch: &str,
        worktree_path: &str,
        checked_out_branch: &str,
    ) -> Result<Value, GitManagerServiceError> {
        let answer = |on_head: bool| {
            json!({
                "pullRequest": pr.to_wire(),
                "branch": local_branch,
                "worktreePath": worktree_path,
                "isOnPullRequestHead": on_head,
            })
        };
        if checked_out_branch != local_branch {
            // A branch that only shares the head's bare name (a fork PR from "main") is somebody
            // else's work: it keeps its tracking config and nothing else.
            self.ensure_existing_worktree_upstream(worktree_path, pr).await?;
            return Ok(answer(false));
        }
        // Before the upstream refresh force-updates the remote-tracking ref.
        let upstream_before = self.git().resolve_commit(worktree_path, "@{upstream}").await.ok();
        self.ensure_existing_worktree_upstream(worktree_path, pr).await?;

        let refreshed: Result<zc_vcs::driver_core::GitRefreshCheckedOutBranchResult, GitManagerServiceError> = async {
            let target = match self.git().fetch_pull_request_head_commit(worktree_path, pr.number.max(0) as u64).await {
                Ok(target) => target,
                Err(_) => {
                    let details = self.git().status_details(worktree_path).await?;
                    let upstream = details
                        .upstream_ref
                        .filter(|upstream| upstream.ends_with(&format!("/{}", pr.head_branch)))
                        .ok_or_else(|| {
                            manager_error(
                                "preparePullRequestThread",
                                worktree_path,
                                "The pull request head could not be resolved for this checkout.",
                            )
                        })?;
                    self.git().resolve_commit(worktree_path, &upstream).await?
                }
            };
            Ok(self
                .git()
                .refresh_checked_out_branch(worktree_path, &target, upstream_before.as_deref())
                .await?)
        }
        .await;
        let (moved, on_target) = match refreshed {
            Ok(refreshed) => (refreshed.moved, refreshed.on_target),
            Err(error) => {
                tracing::warn!(worktree_path, local_branch, cause = %error, "GitManager.preparePullRequestThread reused worktree refresh failed");
                (false, false)
            }
        };
        // Only when the code changed: another thread may be running in this worktree.
        if moved {
            self.maybe_run_setup_script(input, worktree_path).await;
        }
        Ok(answer(on_target))
    }

    /// `findLocalHeadBranch(cwd)`: the local PR branch, or (for a fork PR) a branch of the head's
    /// bare name checked out in another worktree.
    async fn find_local_head_branch(
        &self,
        cwd: &str,
        root: &str,
        pr: &ResolvedPullRequest,
        local_branch: &str,
    ) -> Result<Option<VcsRef>, GitManagerServiceError> {
        let input: VcsListRefsInput = serde_json::from_value(json!({ "cwd": cwd, "refresh": true })).expect("valid list refs input");
        let refs = self.git().list_refs(&input).await?.refs;
        if let Some(found) = refs.iter().find(|r| !r.is_remote() && r.name == local_branch) {
            return Ok(Some(found.clone()));
        }
        if local_branch == pr.head_branch {
            return Ok(None);
        }
        for candidate in &refs {
            let Some(path) = candidate.worktree_path.as_deref() else { continue };
            if candidate.is_remote() || candidate.name != pr.head_branch {
                continue;
            }
            if canonicalize_existing_path(path).await != root {
                return Ok(Some(candidate.clone()));
            }
        }
        Ok(None)
    }
}

/// `summarizeGitActionResult(result, terms)`: the toast title and description.
fn summarize(commit: &CommitStep, push: &PushStep, pr: &PrStep, terms: ChangeRequestTerminology) -> (String, Option<String>) {
    if pr.has_pull_request() {
        let number = pr.number.map(|n| format!(" #{n}")).unwrap_or_default();
        let verb = if pr.status == "created" { "Created" } else { "Opened" };
        return (
            format!("{verb} {}{number}", terms.short_label),
            truncate_text(pr.title.as_deref(), TOAST_DESCRIPTION_MAX),
        );
    }
    if push.status == "pushed" {
        let sha = shorten_sha(commit.commit_sha.as_deref());
        let branch = push.upstream_branch.clone().or_else(|| push.branch.clone()).filter(|b| !b.is_empty());
        let sha_part = sha.map(|sha| format!(" {sha}")).unwrap_or_default();
        let branch_part = branch.map(|branch| format!(" to {branch}")).unwrap_or_default();
        return (
            format!("Pushed{sha_part}{branch_part}"),
            truncate_text(commit.subject.as_deref(), TOAST_DESCRIPTION_MAX),
        );
    }
    if commit.status == "created" {
        let title = match shorten_sha(commit.commit_sha.as_deref()) {
            Some(sha) => format!("Committed {sha}"),
            None => "Committed changes".into(),
        };
        return (title, truncate_text(commit.subject.as_deref(), TOAST_DESCRIPTION_MAX));
    }
    ("Done".into(), None)
}

fn decode<T: serde::de::DeserializeOwned>(operation: &str, value: Value) -> Result<T, GitManagerServiceError> {
    let cwd = value.get("cwd").and_then(Value::as_str).unwrap_or_default().to_owned();
    serde_json::from_value(value).map_err(|error| {
        GitManagerError::new(operation, cwd, "Invalid input.")
            .with_cause(Defect::error("Error", error.to_string()))
            .into()
    })
}

#[async_trait]
impl GitManagerBackend for GitManager {
    async fn status(&self, cwd: &str) -> Result<VcsStatusResult, GitManagerServiceError> {
        GitManager::status(self, cwd).await
    }

    async fn local_status(&self, cwd: &str) -> Result<VcsStatusLocalResult, GitManagerServiceError> {
        GitManager::local_status(self, cwd).await
    }

    async fn remote_status(&self, cwd: &str, options: RemoteStatusOptions) -> Result<Option<VcsStatusRemoteResult>, GitManagerServiceError> {
        GitManager::remote_status(self, cwd, options).await
    }

    async fn invalidate_local_status(&self, cwd: &str) {
        self.status.invalidate_local_status(cwd).await
    }

    async fn invalidate_remote_status(&self, cwd: &str) {
        self.status.invalidate_remote_status(cwd).await
    }

    async fn invalidate_status(&self, cwd: &str) {
        GitManager::invalidate_status(self, cwd).await
    }

    async fn run_stacked_action(&self, input: Value, options: GitRunStackedActionOptions) -> Result<Value, GitManagerServiceError> {
        let input: GitRunStackedActionInput = decode("GitManager.runStackedAction", input)?;
        let result = GitManager::run_stacked_action(self, input, options).await?;
        Ok(serde_json::to_value(result).unwrap_or(Value::Null))
    }

    async fn resolve_pull_request(&self, input: Value) -> Result<Value, GitManagerServiceError> {
        let input: GitPullRequestRefInput = decode("GitManager.resolvePullRequest", input)?;
        GitManager::resolve_pull_request(self, &input).await
    }

    async fn prepare_pull_request_thread(&self, input: Value) -> Result<Value, GitManagerServiceError> {
        let input: GitPreparePullRequestThreadInput = decode("GitManager.preparePullRequestThread", input)?;
        GitManager::prepare_pull_request_thread(self, &input).await
    }

    async fn branch_pull_request(&self, cwd: &str, branch: &str, refresh: bool) -> Result<Option<GitBranchPullRequest>, GitManagerServiceError> {
        GitManager::branch_pull_request(self, cwd, branch, refresh).await
    }
}
