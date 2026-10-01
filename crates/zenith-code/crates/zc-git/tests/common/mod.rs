//! Shared fixtures of the GitManager tests (`GitManager.test.ts`):
//!
//! - real git repositories in temp directories, with an isolated git config, and local bare
//!   repositories as remotes;
//! - [`FakeGh`]: the TS tests' `createGitHubCliWithFakeGh`, at the process level: every `gh`
//!   call goes to a scenario (pr list / view / create / checkout, repo view, the quota probe)
//!   and is recorded; everything else runs for real;
//! - [`FakeTextGeneration`]: canned commit messages and PR content, recording its inputs;
//! - in-memory settings and provider statuses.

#![allow(dead_code, clippy::result_large_err)]

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, Once};

use async_trait::async_trait;
use serde_json::{json, Value};
use zc_core::defect::Defect;
use zc_core::process::{ProcessInvocation, ProcessRunError, ProcessRunInput, ProcessRunOutput, ProcessRunner, SystemProcessRunner};
use zc_core::vcs_process::VcsProcess;
use zc_git::{FixedProvider, GitManager, GitManagerDeps, PullRequestSetupScripts, SettingsSources};
use zc_ports::contracts::{ServerProvider, ServerSettings, ServerSettingsError, ServerSettingsPatch, TextGenerationError};
use zc_ports::text_generation::*;
use zc_ports::{EventStream, ProviderStatusReads, SettingsService, TextGeneration};
use zc_sourcecontrol::github::{GitHubCli, GitHubSourceControlProvider};
use zc_sourcecontrol::SourceControlProvider;
use zc_vcs::GitVcsDriver;

static ISOLATE: Once = Once::new();

/// Point git at an empty global config and ignore the system config (once per test binary).
pub fn isolate_git() {
    ISOLATE.call_once(|| {
        let dir = std::env::temp_dir().join(format!("zc-git-test-home-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("gitconfig");
        std::fs::write(
            &config,
            "[protocol \"file\"]\n\tallow = always\n[advice]\n\tdefaultBranchName = false\n\tdetachedHead = false\n[commit]\n\tgpgsign = false\n[init]\n\tdefaultBranch = main\n[user]\n\tname = Test User\n\temail = test@example.com\n",
        )
        .unwrap();
        // SAFETY: runs once, before any test of this binary spawns a process.
        unsafe {
            std::env::set_var("GIT_CONFIG_GLOBAL", &config);
            std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
            std::env::remove_var("GIT_DIR");
            std::env::remove_var("GIT_WORK_TREE");
            std::env::remove_var("GH_HOST");
        }
    });
}

/// A temporary directory with its canonical path.
pub struct Tmp {
    _dir: tempfile::TempDir,
    pub path: PathBuf,
}

impl Tmp {
    pub fn new(prefix: &str) -> Self {
        isolate_git();
        let dir = tempfile::Builder::new().prefix(prefix).tempdir().unwrap();
        let path = std::fs::canonicalize(dir.path()).unwrap();
        Self { _dir: dir, path }
    }

    pub fn str(&self) -> &str {
        self.path.to_str().unwrap()
    }

    pub fn join(&self, rel: &str) -> String {
        self.path.join(rel).to_string_lossy().into_owned()
    }
}

/// Run git; panics on failure. Returns trimmed stdout.
pub fn git(cwd: impl AsRef<Path>, args: &[&str]) -> String {
    isolate_git();
    let output = Command::new("git").args(args).current_dir(cwd.as_ref()).output().unwrap();
    assert!(output.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Run git, returning success and output.
pub fn git_try(cwd: impl AsRef<Path>, args: &[&str]) -> (bool, String) {
    isolate_git();
    let output = Command::new("git").args(args).current_dir(cwd.as_ref()).output().unwrap();
    (output.status.success(), String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

pub fn write(cwd: impl AsRef<Path>, rel: &str, contents: &str) {
    let path = cwd.as_ref().join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// `initRepo`: `main` with one commit of `README.md`.
pub fn init_repo(cwd: impl AsRef<Path>) {
    let cwd = cwd.as_ref();
    git(cwd, &["init", "--initial-branch=main"]);
    git(cwd, &["config", "user.email", "test@example.com"]);
    git(cwd, &["config", "user.name", "Test User"]);
    write(cwd, "README.md", "hello\n");
    git(cwd, &["add", "README.md"]);
    git(cwd, &["commit", "-m", "Initial commit"]);
}

/// A repository with a commit, kept alive with its directory.
pub fn repo() -> Tmp {
    let dir = Tmp::new("t3code-git-manager-");
    init_repo(&dir.path);
    dir
}

/// `createBareRemote`.
pub fn bare_remote() -> Tmp {
    let dir = Tmp::new("t3code-git-remote-");
    git(&dir.path, &["init", "--bare"]);
    dir
}

/// `configureRemote(cwd, remoteName, remotePath, fetchNamespace)`.
pub fn configure_remote(cwd: impl AsRef<Path>, remote_name: &str, remote_path: &str, fetch_namespace: &str) {
    let cwd = cwd.as_ref();
    git(cwd, &["config", &format!("remote.{remote_name}.url"), remote_path]);
    git(
        cwd,
        &[
            "config",
            "--replace-all",
            &format!("remote.{remote_name}.fetch"),
            &format!("+refs/heads/*:refs/remotes/{fetch_namespace}/*"),
        ],
    );
}

/// `configureVisibleRemoteUrlWithLocalRewrite`: `remote.<name>.url` reads like a forge, pushes
/// and fetches go to the local bare repository.
pub fn configure_visible_remote(cwd: impl AsRef<Path>, remote_name: &str, visible_url: &str, local_path: &str) {
    let cwd = cwd.as_ref();
    git(cwd, &["config", &format!("remote.{remote_name}.url"), visible_url]);
    git(cwd, &["config", &format!("url.{local_path}.insteadOf"), visible_url]);
}

// ---------------------------------------------------------------------------------------------
// Fake gh
// ---------------------------------------------------------------------------------------------

/// How a scenario fails its gh calls.
#[derive(Debug, Clone)]
pub enum GhFailure {
    /// `gh` is not installed.
    Missing,
    /// `gh` exits with `code` and `stderr`.
    Exit { code: i32, stderr: String },
}

/// `FakeGhScenario`.
#[derive(Debug, Clone, Default)]
pub struct GhScenario {
    pub pr_list_sequence: Vec<String>,
    pub pr_list_by_head_selector: HashMap<String, String>,
    pub pr_list_sequence_by_head_selector: HashMap<String, Vec<String>>,
    pub created_pr_url: Option<String>,
    pub default_branch: Option<String>,
    /// `pullRequest` (flat fields like the TS scenario; turned into gh's shape).
    pub pull_request: Option<Value>,
    pub repository_clone_urls: HashMap<String, (String, String)>,
    pub fail_with: Option<GhFailure>,
    pub fail_after_calls: usize,
}

struct GhState {
    scenario: GhScenario,
    list_queue: VecDeque<String>,
    list_queue_by_head: HashMap<String, VecDeque<String>>,
    calls: Vec<String>,
}

/// `createGitHubCliWithFakeGh`, at the process level.
pub struct FakeGh {
    state: Mutex<GhState>,
}

fn ok(stdout: impl Into<String>) -> Result<ProcessRunOutput, ProcessRunError> {
    Ok(ProcessRunOutput {
        stdout: stdout.into(),
        code: Some(0),
        ..ProcessRunOutput::default()
    })
}

fn exit(code: i32, stderr: &str) -> Result<ProcessRunOutput, ProcessRunError> {
    Ok(ProcessRunOutput {
        stderr: stderr.into(),
        code: Some(code),
        ..ProcessRunOutput::default()
    })
}

fn missing(input: &ProcessRunInput) -> Result<ProcessRunOutput, ProcessRunError> {
    Err(ProcessRunError::Spawn {
        invocation: ProcessInvocation {
            command: input.command.clone(),
            argument_count: input.args.len(),
            cwd: input.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
            spawn_cwd: None,
        },
        resolved_command: Some(input.command.clone()),
        resolved_argument_count: Some(input.args.len()),
        shell: Some(false),
        cause: Defect::error("Error", "No such file or directory (os error 2)"),
    })
}

/// The flat fake PR fields (`headRepositoryNameWithOwner`, `headRepositoryOwnerLogin`) in gh's
/// nested JSON shape.
pub fn gh_shape(entry: &Value) -> Value {
    let Value::Object(map) = entry else { return entry.clone() };
    let mut out = map.clone();
    if let Some(name) = map.get("headRepositoryNameWithOwner").and_then(Value::as_str) {
        out.insert("headRepository".into(), json!({ "nameWithOwner": name }));
    }
    if let Some(login) = map.get("headRepositoryOwnerLogin").and_then(Value::as_str) {
        out.insert("headRepositoryOwner".into(), json!({ "login": login }));
    }
    Value::Object(out)
}

fn gh_list_shape(raw: &str) -> String {
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Array(entries)) => Value::Array(entries.iter().map(gh_shape).collect()).to_string(),
        _ => raw.to_owned(),
    }
}

impl FakeGh {
    pub fn new(scenario: GhScenario) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(GhState {
                list_queue: scenario.pr_list_sequence.iter().cloned().collect(),
                list_queue_by_head: scenario
                    .pr_list_sequence_by_head_selector
                    .iter()
                    .map(|(head, values)| (head.clone(), values.iter().cloned().collect()))
                    .collect(),
                scenario,
                calls: Vec::new(),
            }),
        })
    }

    /// Every gh call as `arg arg …` (the quota probe `api rate_limit` excluded).
    pub fn calls(&self) -> Vec<String> {
        self.state.lock().unwrap().calls.clone()
    }

    pub fn calls_starting_with(&self, prefix: &str) -> usize {
        self.calls().iter().filter(|call| call.starts_with(prefix)).count()
    }

    pub fn set_scenario(&self, update: impl FnOnce(&mut GhScenario)) {
        let mut state = self.state.lock().unwrap();
        update(&mut state.scenario);
        state.list_queue = state.scenario.pr_list_sequence.iter().cloned().collect();
    }

    pub fn push_list(&self, stdout: &str) {
        self.state.lock().unwrap().list_queue.push_back(stdout.to_owned());
    }

    fn respond(&self, input: &ProcessRunInput) -> Result<ProcessRunOutput, ProcessRunError> {
        let args = input.args.clone();
        if args.first().map(String::as_str) == Some("api") && args.get(1).map(String::as_str) == Some("rate_limit") {
            return ok(r#"{"data":{"rateLimit":{"cost":1,"limit":5000,"remaining":4999,"resetAt":"2099-01-01T00:00:00Z"}}}"#);
        }
        let mut state = self.state.lock().unwrap();
        state.calls.push(args.join(" "));
        if let Some(failure) = state.scenario.fail_with.clone() {
            if state.calls.len() > state.scenario.fail_after_calls {
                return match failure {
                    GhFailure::Missing => missing(input),
                    GhFailure::Exit { code, stderr } => exit(code, &stderr),
                };
            }
        }
        let (a0, a1) = (args.first().map(String::as_str), args.get(1).map(String::as_str));
        match (a0, a1) {
            (Some("pr"), Some("list")) => {
                let head = args.iter().position(|a| a == "--head").and_then(|i| args.get(i + 1)).cloned();
                let mapped_queue = head.as_ref().and_then(|h| state.list_queue_by_head.get_mut(h)).and_then(VecDeque::pop_front);
                let mapped = head.as_ref().and_then(|h| state.scenario.pr_list_by_head_selector.get(h)).cloned();
                let stdout = match (mapped_queue, mapped) {
                    (Some(value), _) | (None, Some(value)) => value,
                    (None, None) => state.list_queue.pop_front().unwrap_or_else(|| "[]".into()),
                };
                ok(format!("{}\n", gh_list_shape(&stdout)))
            }
            (Some("pr"), Some("create")) => ok(format!(
                "{}\n",
                state
                    .scenario
                    .created_pr_url
                    .clone()
                    .unwrap_or_else(|| "https://github.com/pingdotgg/codething-mvp/pull/101".into())
            )),
            (Some("pr"), Some("view")) => {
                let pr = state.scenario.pull_request.clone().unwrap_or_else(|| {
                    json!({
                        "number": 101,
                        "title": "Pull request",
                        "url": "https://github.com/pingdotgg/codething-mvp/pull/101",
                        "baseRefName": "main",
                        "headRefName": "feature/pull-request",
                        "state": "open",
                    })
                });
                ok(format!("{}\n", gh_shape(&pr)))
            }
            (Some("pr"), Some("checkout")) => {
                let head = state
                    .scenario
                    .pull_request
                    .as_ref()
                    .and_then(|pr| pr["headRefName"].as_str())
                    .map(str::to_owned);
                drop(state);
                if let (Some(head), Some(cwd)) = (head, input.cwd.as_ref()) {
                    let (exists, _) = git_try(cwd, &["show-ref", "--verify", "--quiet", &format!("refs/heads/{head}")]);
                    let (done, _) = if exists {
                        git_try(cwd, &["checkout", &head])
                    } else {
                        git_try(cwd, &["checkout", "-b", &head])
                    };
                    if !done {
                        return exit(1, "Failed to simulate gh checkout");
                    }
                }
                ok("")
            }
            (Some("repo"), Some("view")) => {
                let repository = args.get(2).cloned().filter(|r| !r.starts_with("--"));
                if let Some(repository) = repository {
                    if args.iter().any(|a| a == "nameWithOwner,url,sshUrl") {
                        return match state.scenario.repository_clone_urls.get(&repository) {
                            Some((url, ssh)) => ok(format!("{}\n", json!({"nameWithOwner": repository, "url": url, "sshUrl": ssh}))),
                            None => exit(1, &format!("Unexpected repository lookup: {repository}")),
                        };
                    }
                }
                ok(format!("{}\n", state.scenario.default_branch.clone().unwrap_or_else(|| "main".into())))
            }
            _ => exit(1, &format!("Unexpected gh command: {}", args.join(" "))),
        }
    }
}

/// Answers `gh` from a [`FakeGh`], runs everything else for real.
pub struct FakeGhRunner(pub Arc<FakeGh>);

#[async_trait]
impl ProcessRunner for FakeGhRunner {
    async fn run(&self, input: ProcessRunInput) -> Result<ProcessRunOutput, ProcessRunError> {
        if input.command == "gh" {
            return self.0.respond(&input);
        }
        SystemProcessRunner.run(input).await
    }
}

/// The GitHub provider over the fake gh.
pub fn github_provider(gh: &Arc<FakeGh>) -> Arc<dyn SourceControlProvider> {
    let process = VcsProcess::new(Arc::new(FakeGhRunner(gh.clone())));
    Arc::new(GitHubSourceControlProvider::new(GitHubCli::new(
        process,
        zc_sourcecontrol::util::system_clock(),
    )))
}

// ---------------------------------------------------------------------------------------------
// Fake text generation
// ---------------------------------------------------------------------------------------------

type CommitFn = Box<dyn Fn(&CommitMessageGenerationInput) -> Result<CommitMessageGenerationResult, TextGenerationError> + Send + Sync>;
type PrFn = Box<dyn Fn(&PrContentGenerationInput) -> Result<PrContentGenerationResult, TextGenerationError> + Send + Sync>;

/// `createTextGeneration(overrides)`.
pub struct FakeTextGeneration {
    pub commit: CommitFn,
    pub pr: PrFn,
    pub commit_inputs: Mutex<Vec<CommitMessageGenerationInput>>,
    pub pr_inputs: Mutex<Vec<PrContentGenerationInput>>,
}

impl Default for FakeTextGeneration {
    fn default() -> Self {
        Self {
            commit: Box::new(|input| {
                Ok(CommitMessageGenerationResult {
                    subject: "Implement stacked git actions".into(),
                    body: String::new(),
                    branch: input.include_branch.then(|| "feature/implement-stacked-git-actions".into()),
                })
            }),
            pr: Box::new(|_| {
                Ok(PrContentGenerationResult {
                    title: "Add stacked git actions".into(),
                    body: "## Summary\n- Add stacked git workflow\n\n## Testing\n- Not run".into(),
                })
            }),
            commit_inputs: Mutex::new(Vec::new()),
            pr_inputs: Mutex::new(Vec::new()),
        }
    }
}

/// `TextGenerationError({operation, detail: "fake text generation failed"})`.
pub fn fake_text_generation_error(operation: &str) -> TextGenerationError {
    zc_textgen::utils::text_generation_error(operation, "fake text generation failed")
}

#[async_trait]
impl TextGeneration for FakeTextGeneration {
    async fn generate_commit_message(&self, input: CommitMessageGenerationInput) -> Result<CommitMessageGenerationResult, TextGenerationError> {
        self.commit_inputs.lock().unwrap().push(input.clone());
        (self.commit)(&input)
    }

    async fn generate_pr_content(&self, input: PrContentGenerationInput) -> Result<PrContentGenerationResult, TextGenerationError> {
        self.pr_inputs.lock().unwrap().push(input.clone());
        (self.pr)(&input)
    }

    async fn generate_branch_name(&self, _input: BranchNameGenerationInput) -> Result<String, TextGenerationError> {
        Ok("update-workflow".into())
    }

    async fn generate_thread_title(&self, _input: ThreadTitleGenerationInput) -> Result<ThreadTitleGenerationResult, TextGenerationError> {
        Ok(ThreadTitleGenerationResult {
            title: "Update workflow".into(),
            needs_refinement: None,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Settings and provider statuses
// ---------------------------------------------------------------------------------------------

/// `ServerSettings.layerTest(overrides)`.
pub struct MemorySettings(pub Mutex<Value>);

impl MemorySettings {
    pub fn new(overrides: Value) -> Arc<Self> {
        Arc::new(Self(Mutex::new(zc_settings::settings::test_settings(&overrides))))
    }
}

#[async_trait]
impl SettingsService for MemorySettings {
    async fn get_settings(&self) -> Result<ServerSettings, ServerSettingsError> {
        Ok(serde_json::from_value(self.0.lock().unwrap().clone()).expect("valid test settings"))
    }

    async fn update_settings(&self, _patch: ServerSettingsPatch) -> Result<ServerSettings, ServerSettingsError> {
        self.get_settings().await
    }

    fn subscribe_changes(&self) -> EventStream<ServerSettings> {
        Box::pin(futures::stream::pending())
    }
}

/// `ProviderRegistry.getProviders` answering a fixed list.
pub struct FixedProviders(pub Vec<Value>);

#[async_trait]
impl ProviderStatusReads for FixedProviders {
    async fn get_providers(&self) -> Vec<ServerProvider> {
        self.0.iter().cloned().map(ServerProvider).collect()
    }
}

/// Records `runForThread` calls (`{threadId, projectCwd, worktreePath}`).
#[derive(Default)]
pub struct RecordingSetupScripts {
    pub calls: Mutex<Vec<(String, String, String)>>,
    pub fail: bool,
}

#[async_trait]
impl PullRequestSetupScripts for RecordingSetupScripts {
    async fn run_for_thread(&self, thread_id: &str, project_cwd: &str, worktree_path: &str) -> Result<(), String> {
        self.calls
            .lock()
            .unwrap()
            .push((thread_id.to_owned(), project_cwd.to_owned(), worktree_path.to_owned()));
        if self.fail {
            Err("terminal failed to start".into())
        } else {
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The manager
// ---------------------------------------------------------------------------------------------

/// `makeManager(input)` options.
pub struct ManagerOptions {
    pub gh: GhScenario,
    /// Replaces the GitHub provider over the fake gh.
    pub provider: Option<Arc<dyn SourceControlProvider>>,
    pub text_generation: FakeTextGeneration,
    pub settings: Value,
    pub providers: Vec<Value>,
    pub setup_scripts: Option<Arc<RecordingSetupScripts>>,
}

impl Default for ManagerOptions {
    fn default() -> Self {
        Self {
            gh: GhScenario::default(),
            provider: None,
            text_generation: FakeTextGeneration::default(),
            settings: json!({}),
            providers: Vec::new(),
            setup_scripts: None,
        }
    }
}

pub struct Harness {
    pub manager: GitManager,
    pub gh: Arc<FakeGh>,
    pub text: Arc<FakeTextGeneration>,
    pub temp: Tmp,
}

impl Harness {
    pub fn gh_calls(&self) -> Vec<String> {
        self.gh.calls()
    }
}

pub fn make_manager(options: ManagerOptions) -> Harness {
    isolate_git();
    let gh = FakeGh::new(options.gh);
    let provider = options.provider.unwrap_or_else(|| github_provider(&gh));
    let text = Arc::new(options.text_generation);
    let temp = Tmp::new("t3-git-manager-test-");
    let setup: Option<Arc<dyn PullRequestSetupScripts>> = options.setup_scripts.map(|s| s as Arc<dyn PullRequestSetupScripts>);
    let manager = GitManager::new(GitManagerDeps {
        git: GitVcsDriver::new(temp.path.join("worktrees")),
        providers: Arc::new(FixedProvider(provider)),
        text_generation: text.clone(),
        settings: SettingsSources {
            settings: MemorySettings::new(options.settings),
            provider_status: Arc::new(FixedProviders(options.providers)),
            projections: None,
        },
        setup_scripts: setup,
        temp_dir: temp.path.clone(),
        uuids: Arc::new(zc_core::uuid_v4),
    });
    Harness { manager, gh, text, temp }
}

/// A [`make_manager`] with defaults and a gh scenario.
pub fn manager_with(gh: GhScenario) -> Harness {
    make_manager(ManagerOptions {
        gh,
        ..ManagerOptions::default()
    })
}

/// The JSON of `GitRunStackedActionInput`.
pub fn action_input(cwd: &str, action: &str) -> zc_git::types::GitRunStackedActionInput {
    serde_json::from_value(json!({ "actionId": "test-action-id", "cwd": cwd, "action": action })).unwrap()
}

/// Collects progress events.
pub fn recorder() -> (zc_ports::git::GitRunStackedActionOptions, Arc<Mutex<Vec<Value>>>) {
    let events: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    (
        zc_ports::git::GitRunStackedActionOptions {
            action_id: Some("test-action-id".into()),
            progress_reporter: Some(Arc::new(move |event: zc_ports::contracts::GitActionProgressEvent| {
                sink.lock().unwrap().push(event.0)
            })),
        },
        events,
    )
}

/// `BTreeMap` of a JSON object, for order-insensitive comparisons in assertions.
pub fn keys(value: &Value) -> BTreeMap<String, Value> {
    value
        .as_object()
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default()
}
