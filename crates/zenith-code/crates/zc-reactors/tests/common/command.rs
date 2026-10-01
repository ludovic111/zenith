//! The `ProviderCommandReactor.test.ts` harness: a recording provider service whose calls can
//! be scripted, fake git / VCS status / text generation / terminals / auth, and an engine
//! wrapper with the TS harness's dispatch hooks.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::FutureExt;
use serde_json::{json, Value};
use zc_contracts::{OrchestrationCommand, OrchestrationEvent};
use zc_core::PubSub;
use zc_orchestration::engine::OrchestrationEngine;
use zc_ports::contracts as ports;
use zc_ports::git::{CreateWorktreeOptions, GitBranchPullRequest, GitRemoteStatusOptions, GitRunStackedActionOptions, RemoteTrackingCommit};
use zc_ports::orchestration::{ThreadReplayRange, ThreadReplayStats};
use zc_ports::provider::{ProviderAdapterCapabilities, ProviderContinuationIdentity, ProviderInstanceRoutingInfo, SessionModelSwitchMode};
use zc_ports::text_generation::{
    BranchNameGenerationInput, CommitMessageGenerationInput, CommitMessageGenerationResult, PrContentGenerationInput, PrContentGenerationResult,
    ThreadTitleGenerationInput, ThreadTitleGenerationResult,
};
use zc_ports::{
    DispatchResult, EventStream, GitWorkflow, OrchestrationDispatch, ProviderAuthCommands, ProviderService, ProviderStatusReads, TaggedError, TerminalManager,
    TextGeneration, VcsStatusRefresher,
};
use zc_reactors::command_reactor::CommandReactorDeps;
use zc_reactors::common::system_uuids;
use zc_reactors::registries::ThreadBackgroundLivenessRegistry;
use zc_reactors::{EventLogReactorReads, ManualClock, ProviderCommandReactor};

use super::{dispatch, driver_of, find, ok, s, MemorySettings, NOW};

pub type Hook<A, R> = Arc<dyn Fn(A) -> BoxFuture<'static, R> + Send + Sync>;

pub fn hook<A, R, F, Fut>(f: F) -> Hook<A, R>
where
    F: Fn(A) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = R> + Send + 'static,
{
    Arc::new(move |a| f(a).boxed())
}

/// A one-shot signal (`Deferred<void>`).
#[derive(Clone)]
pub struct Latch(Arc<tokio::sync::watch::Sender<bool>>);

impl Default for Latch {
    fn default() -> Self {
        Self::new()
    }
}

impl Latch {
    pub fn new() -> Self {
        Self(Arc::new(tokio::sync::watch::channel(false).0))
    }
    pub fn open(&self) {
        self.0.send_replace(true);
    }
    pub fn is_open(&self) -> bool {
        *self.0.borrow()
    }
    pub async fn wait(&self) {
        let mut receiver = self.0.subscribe();
        let _ = receiver.wait_for(|open| *open).await;
    }
}

/// `ProviderAdapterRequestError`.
pub fn request_error(provider: &str, method: &str, detail: &str) -> TaggedError {
    TaggedError::new(
        "ProviderAdapterRequestError",
        format!("Provider adapter request failed ({provider}) for {method}: {detail}"),
    )
    .with("provider", provider)
    .with("method", method)
    .with("detail", detail)
}

/// A recorded call, with a global order shared by every fake.
#[derive(Debug, Clone)]
pub struct Call {
    pub order: u64,
    pub args: Value,
}

#[derive(Default)]
pub struct Recorder {
    calls: Mutex<Vec<Call>>,
}

static ORDER: AtomicU64 = AtomicU64::new(0);

impl Recorder {
    pub fn record(&self, args: Value) {
        self.calls.lock().unwrap().push(Call {
            order: ORDER.fetch_add(1, Ordering::SeqCst),
            args,
        });
    }
    pub fn calls(&self) -> Vec<Value> {
        self.calls.lock().unwrap().iter().map(|call| call.args.clone()).collect()
    }
    pub fn count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
    pub fn first_order(&self) -> Option<u64> {
        self.calls.lock().unwrap().first().map(|call| call.order)
    }
    pub fn call(&self, index: usize) -> Value {
        self.calls().get(index).cloned().unwrap_or(Value::Null)
    }
}

pub type ResultHook = Hook<Value, Result<(), TaggedError>>;

/// The scripted provider service of the TS harness.
pub struct CommandProviders {
    pub sessions: Mutex<Vec<Value>>,
    next_session: AtomicUsize,
    default_selection: Value,
    pub session_model_switch: Mutex<SessionModelSwitchMode>,
    pub start_session: Recorder,
    pub send_turn: Recorder,
    pub compact_thread: Recorder,
    pub interrupt_turn: Recorder,
    pub respond_to_request: Recorder,
    pub respond_to_user_input: Recorder,
    pub stop_session: Recorder,
    pub start_hook: Mutex<Option<Hook<Value, Result<Value, TaggedError>>>>,
    /// `mockImplementationOnce` failures of startSession.
    pub start_failures_once: Mutex<Vec<TaggedError>>,
    pub send_hook: Mutex<Option<ResultHook>>,
    pub compact_hook: Mutex<Option<ResultHook>>,
    pub interrupt_hook: Mutex<Option<ResultHook>>,
    pub stop_hook: Mutex<Option<ResultHook>>,
    pub respond_hook: Mutex<Option<ResultHook>>,
    pub user_input_hook: Mutex<Option<ResultHook>>,
    events: PubSub<ports::ProviderRuntimeEvent>,
}

impl CommandProviders {
    pub fn new(default_selection: Value, session_model_switch: SessionModelSwitchMode) -> Arc<Self> {
        Arc::new(Self {
            sessions: Mutex::new(Vec::new()),
            next_session: AtomicUsize::new(1),
            default_selection,
            session_model_switch: Mutex::new(session_model_switch),
            start_session: Recorder::default(),
            send_turn: Recorder::default(),
            compact_thread: Recorder::default(),
            interrupt_turn: Recorder::default(),
            respond_to_request: Recorder::default(),
            respond_to_user_input: Recorder::default(),
            stop_session: Recorder::default(),
            start_hook: Mutex::new(None),
            start_failures_once: Mutex::new(Vec::new()),
            send_hook: Mutex::new(None),
            compact_hook: Mutex::new(None),
            interrupt_hook: Mutex::new(None),
            stop_hook: Mutex::new(None),
            respond_hook: Mutex::new(None),
            user_input_hook: Mutex::new(None),
            events: PubSub::new(),
        })
    }

    async fn run(hook: &Mutex<Option<ResultHook>>, args: Value) -> Result<(), TaggedError> {
        let hook = hook.lock().unwrap().clone();
        match hook {
            Some(hook) => hook(args).await,
            None => Ok(()),
        }
    }
}

#[async_trait]
impl ProviderService for CommandProviders {
    async fn start_session(&self, thread_id: &ports::ThreadId, input: ports::ProviderSessionStartInput) -> Result<ports::ProviderSession, TaggedError> {
        let input = input.0;
        self.start_session.record(json!([thread_id, input]));
        let failure = {
            let mut failures = self.start_failures_once.lock().unwrap();
            (!failures.is_empty()).then(|| failures.remove(0))
        };
        if let Some(failure) = failure {
            return Err(failure);
        }
        let index = self.next_session.fetch_add(1, Ordering::SeqCst);
        let selection = input.get("modelSelection").cloned();
        let instance_id = input
            .get("providerInstanceId")
            .cloned()
            .or_else(|| selection.as_ref().map(|selection| selection["instanceId"].clone()));
        let provider = input
            .get("provider")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| s(selection.as_ref().unwrap_or(&self.default_selection), "instanceId").to_owned());
        let mut session = json!({
            "provider": provider,
            "status": "ready",
            "runtimeMode": if input["runtimeMode"] == "approval-required" { "approval-required" } else { "full-access" },
            "threadId": input.get("threadId").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| format!("thread-{index}")),
            "resumeCursor": input.get("resumeCursor").cloned().unwrap_or(json!({"opaque": format!("resume-{index}")})),
            "createdAt": NOW,
            "updatedAt": NOW,
        });
        if let Some(instance_id) = instance_id {
            session["providerInstanceId"] = instance_id;
        }
        if let Some(cwd) = input.get("cwd").and_then(Value::as_str) {
            session["cwd"] = json!(cwd);
        }
        let model = selection
            .as_ref()
            .and_then(|selection| selection["model"].as_str())
            .or_else(|| self.default_selection["model"].as_str());
        if let Some(model) = model {
            session["model"] = json!(model);
        }
        let hook = self.start_hook.lock().unwrap().clone();
        let session = match hook {
            Some(hook) => hook(session).await?,
            None => session,
        };
        self.sessions.lock().unwrap().push(session.clone());
        Ok(ports::ProviderSession(session))
    }
    async fn send_turn(&self, input: ports::ProviderSendTurnInput) -> Result<ports::ProviderTurnStartResult, TaggedError> {
        self.send_turn.record(input.0.clone());
        Self::run(&self.send_hook, input.0).await?;
        Ok(ports::ProviderTurnStartResult(json!({"threadId": "thread-1", "turnId": "turn-1"})))
    }
    async fn compact_thread(
        &self,
        thread_id: &ports::ThreadId,
        model: Option<ports::ModelSelection>,
        request: Option<ports::MessageId>,
    ) -> Result<(), TaggedError> {
        let args = json!([thread_id, model.map(|model| model.0), request]);
        self.compact_thread.record(args.clone());
        Self::run(&self.compact_hook, args).await
    }
    async fn interrupt_turn(&self, input: ports::ProviderInterruptTurnInput) -> Result<(), TaggedError> {
        self.interrupt_turn.record(input.0.clone());
        Self::run(&self.interrupt_hook, input.0).await
    }
    async fn respond_to_request(&self, input: ports::ProviderRespondToRequestInput) -> Result<(), TaggedError> {
        self.respond_to_request.record(input.0.clone());
        Self::run(&self.respond_hook, input.0).await
    }
    async fn respond_to_user_input(&self, input: ports::ProviderRespondToUserInputInput) -> Result<(), TaggedError> {
        self.respond_to_user_input.record(input.0.clone());
        Self::run(&self.user_input_hook, input.0).await
    }
    async fn stop_session(&self, input: ports::ProviderStopSessionInput) -> Result<(), TaggedError> {
        self.stop_session.record(input.0.clone());
        Self::run(&self.stop_hook, input.0.clone()).await?;
        let thread_id = input.0["threadId"].clone();
        self.sessions.lock().unwrap().retain(|session| session["threadId"] != thread_id);
        Ok(())
    }
    async fn list_sessions(&self) -> Vec<ports::ProviderSession> {
        self.sessions.lock().unwrap().iter().cloned().map(ports::ProviderSession).collect()
    }
    async fn get_capabilities(&self, _instance_id: &ports::ProviderInstanceId) -> Result<ProviderAdapterCapabilities, TaggedError> {
        Ok(ProviderAdapterCapabilities {
            session_model_switch: *self.session_model_switch.lock().unwrap(),
            promptless_turn_continuation: None,
            supports_conversation_rollback: None,
        })
    }
    async fn get_instance_info(&self, instance_id: &ports::ProviderInstanceId) -> Result<ProviderInstanceRoutingInfo, TaggedError> {
        let driver = driver_of(instance_id.as_str());
        let continuation_key = if driver == "codex" {
            "codex:home:/shared-codex".to_owned()
        } else {
            format!("{driver}:instance:{instance_id}")
        };
        Ok(ProviderInstanceRoutingInfo {
            instance_id: instance_id.clone(),
            driver_kind: ports::ProviderDriverKind::new(&driver),
            display_name: None,
            accent_color: None,
            enabled: true,
            continuation_identity: ProviderContinuationIdentity {
                driver_kind: ports::ProviderDriverKind::new(&driver),
                continuation_key,
            },
        })
    }
    async fn assert_conversation_rollback_supported(&self, _thread_id: &ports::ThreadId) -> Result<(), TaggedError> {
        Err(TaggedError::new("Defect", "Unsupported provider call in test"))
    }
    async fn rollback_conversation(&self, _thread_id: &ports::ThreadId, _num_turns: u32) -> Result<(), TaggedError> {
        Err(TaggedError::new("Defect", "Unsupported provider call in test"))
    }
    async fn upload_feedback(&self, _input: ports::ProviderUploadFeedbackInput) -> Result<ports::ProviderUploadFeedbackResult, TaggedError> {
        Err(TaggedError::new("Defect", "Unsupported provider call in test"))
    }
    fn subscribe_events(&self) -> EventStream<ports::ProviderRuntimeEvent> {
        futures::StreamExt::boxed(self.events.subscribe())
    }
}

/// `ProviderRegistry.getProviders` of the harness.
pub struct StatusReads(pub Vec<Value>);

#[async_trait]
impl ProviderStatusReads for StatusReads {
    async fn get_providers(&self) -> Vec<ports::ServerProvider> {
        self.0.iter().cloned().map(ports::ServerProvider).collect()
    }
}

/// `ProviderAuthService.tryHandlePromptCommand`.
pub struct FakeAuth {
    pub calls: Recorder,
    pub hook: Mutex<Option<Hook<Value, Result<bool, TaggedError>>>>,
}

#[async_trait]
impl ProviderAuthCommands for FakeAuth {
    async fn try_handle_prompt_command(&self, instance_id: &ports::ProviderInstanceId, text: &str, has_attachments: bool) -> Result<bool, TaggedError> {
        let args = json!({"instanceId": instance_id, "text": text, "hasAttachments": has_attachments});
        self.calls.record(args.clone());
        let hook = self.hook.lock().unwrap().clone();
        match hook {
            Some(hook) => hook(args).await,
            None => Ok(false),
        }
    }
}

fn unsupported<T>() -> Result<T, TaggedError> {
    Err(TaggedError::new("Defect", "unexpected git call in test"))
}

/// The git workflow mock: rename, prune and create are recorded.
#[derive(Default)]
pub struct FakeGit {
    pub rename_branch: Recorder,
    pub prune_worktrees: Recorder,
    pub create_worktree: Recorder,
}

#[async_trait]
impl GitWorkflow for FakeGit {
    async fn is_repository(&self, _cwd: &str) -> Result<bool, TaggedError> {
        Ok(true)
    }
    async fn has_commit(&self, _cwd: &str, _ref_name: &str) -> Result<bool, TaggedError> {
        unsupported()
    }
    async fn status(&self, _input: ports::VcsStatusInput) -> Result<ports::VcsStatusResult, TaggedError> {
        unsupported()
    }
    async fn local_status(&self, _input: ports::VcsStatusInput) -> Result<ports::VcsStatusLocalResult, TaggedError> {
        unsupported()
    }
    async fn remote_status(
        &self,
        _input: ports::VcsStatusInput,
        _options: GitRemoteStatusOptions,
    ) -> Result<Option<ports::VcsStatusRemoteResult>, TaggedError> {
        unsupported()
    }
    async fn branch_pull_request(&self, _cwd: &str, _branch: &str, _refresh: bool) -> Result<Option<GitBranchPullRequest>, TaggedError> {
        unsupported()
    }
    async fn invalidate_local_status(&self, _cwd: &str) {}
    async fn invalidate_remote_status(&self, _cwd: &str) {}
    async fn invalidate_status(&self, _cwd: &str) {}
    async fn pull_current_branch(&self, _cwd: &str) -> Result<ports::VcsPullResult, TaggedError> {
        unsupported()
    }
    async fn run_stacked_action(
        &self,
        _input: ports::GitRunStackedActionInput,
        _options: GitRunStackedActionOptions,
    ) -> Result<ports::GitRunStackedActionResult, TaggedError> {
        unsupported()
    }
    async fn resolve_pull_request(&self, _input: ports::GitPullRequestRefInput) -> Result<ports::GitResolvePullRequestResult, TaggedError> {
        unsupported()
    }
    async fn prepare_pull_request_thread(
        &self,
        _input: ports::GitPreparePullRequestThreadInput,
    ) -> Result<ports::GitPreparePullRequestThreadResult, TaggedError> {
        unsupported()
    }
    async fn list_refs(&self, _input: ports::VcsListRefsInput) -> Result<ports::VcsListRefsResult, TaggedError> {
        unsupported()
    }
    async fn create_worktree(
        &self,
        input: ports::VcsCreateWorktreeInput,
        options: CreateWorktreeOptions,
    ) -> Result<ports::VcsCreateWorktreeResult, TaggedError> {
        self.create_worktree.record(json!([input.0, {"submodules": options.submodules}]));
        Ok(ports::VcsCreateWorktreeResult(
            json!({"worktree": {"path": input.0["path"], "refName": input.0["refName"]}}),
        ))
    }
    async fn fetch_remote(&self, _cwd: &str, _remote: &str, _ref_name: Option<&str>) -> Result<(), TaggedError> {
        unsupported()
    }
    async fn remote_exists(&self, _cwd: &str, _remote: &str) -> Result<bool, TaggedError> {
        unsupported()
    }
    async fn remote_branch_exists(&self, _cwd: &str, _remote: &str, _ref_name: &str) -> Result<bool, TaggedError> {
        unsupported()
    }
    async fn resolve_remote_tracking_commit(&self, _cwd: &str, _ref_name: &str, _fallback: &str) -> Result<RemoteTrackingCommit, TaggedError> {
        unsupported()
    }
    async fn remove_worktree(&self, _input: ports::VcsRemoveWorktreeInput) -> Result<(), TaggedError> {
        unsupported()
    }
    async fn prune_worktrees(&self, cwd: &str) -> Result<(), TaggedError> {
        self.prune_worktrees.record(json!({"cwd": cwd}));
        Ok(())
    }
    async fn create_ref(&self, _input: ports::VcsCreateRefInput) -> Result<ports::VcsCreateRefResult, TaggedError> {
        unsupported()
    }
    async fn switch_ref(&self, _input: ports::VcsSwitchRefInput) -> Result<ports::VcsSwitchRefResult, TaggedError> {
        unsupported()
    }
    async fn rename_branch(&self, cwd: &str, old_branch: &str, new_branch: &str) -> Result<String, TaggedError> {
        self.rename_branch.record(json!({"cwd": cwd, "oldBranch": old_branch, "newBranch": new_branch}));
        Ok(new_branch.to_owned())
    }
}

/// `VcsStatusBroadcaster.refreshStatus`.
#[derive(Default)]
pub struct FakeVcsStatus {
    pub refresh_status: Recorder,
    pub refreshed: Latch,
}

#[async_trait]
impl VcsStatusRefresher for FakeVcsStatus {
    async fn refresh_local_status(&self, _cwd: &str) -> Result<ports::VcsStatusLocalResult, TaggedError> {
        Err(TaggedError::new("Defect", "refreshLocalStatus should not be called in this test"))
    }
    async fn refresh_status(&self, cwd: &str) -> Result<ports::VcsStatusResult, TaggedError> {
        self.refresh_status.record(json!(cwd));
        self.refreshed.open();
        Ok(ports::VcsStatusResult(json!({"isRepo": true, "refName": "renamed-branch", "pr": null})))
    }
    async fn refresh_pull_request_status(&self, _cwd: &str) -> Result<Option<ports::VcsStatusRemoteResult>, TaggedError> {
        Err(TaggedError::new("Defect", "refreshPullRequestStatus should not be called in this test"))
    }
}

type TitleHook = Hook<Value, Result<ThreadTitleGenerationResult, TaggedError>>;

/// The text generation mock: both generators fail ("disabled in test harness") unless scripted.
#[derive(Default)]
pub struct FakeTextGeneration {
    pub branch_calls: Recorder,
    pub title_calls: Recorder,
    pub branch_hook: Mutex<Option<Hook<Value, Result<String, TaggedError>>>>,
    pub title_hook: Mutex<Option<TitleHook>>,
    pub title_once: Mutex<Vec<TitleHook>>,
}

fn disabled(operation: &str) -> TaggedError {
    TaggedError::new(
        "TextGenerationError",
        format!("Text generation failed in {operation}: disabled in test harness"),
    )
    .with("operation", operation)
    .with("detail", "disabled in test harness")
}

impl FakeTextGeneration {
    /// `generateThreadTitle.mockReturnValue(Effect.succeed({title}))`.
    pub fn titles(&self, title: &str) {
        let title = title.to_owned();
        *self.title_hook.lock().unwrap() = Some(hook(move |_| {
            let title = title.clone();
            async move { Ok(ThreadTitleGenerationResult { title, needs_refinement: None }) }
        }));
    }

    /// `mockReturnValueOnce(Effect.succeed({title}))`.
    pub fn title_once(&self, title: &str) {
        let title = title.to_owned();
        self.title_once.lock().unwrap().push(hook(move |_| {
            let title = title.clone();
            async move { Ok(ThreadTitleGenerationResult { title, needs_refinement: None }) }
        }));
    }
}

#[async_trait]
impl TextGeneration for FakeTextGeneration {
    async fn generate_commit_message(&self, _input: CommitMessageGenerationInput) -> Result<CommitMessageGenerationResult, TaggedError> {
        Err(disabled("generateCommitMessage"))
    }
    async fn generate_pr_content(&self, _input: PrContentGenerationInput) -> Result<PrContentGenerationResult, TaggedError> {
        Err(disabled("generatePrContent"))
    }
    async fn generate_branch_name(&self, input: BranchNameGenerationInput) -> Result<String, TaggedError> {
        let args = json!({
            "cwd": input.cwd, "message": input.message,
            "attachments": input.attachments.iter().map(|a| a.0.clone()).collect::<Vec<_>>(),
            "modelSelection": input.model_selection.0,
        });
        self.branch_calls.record(args.clone());
        let hook = self.branch_hook.lock().unwrap().clone();
        match hook {
            Some(hook) => hook(args).await,
            None => Err(disabled("generateBranchName")),
        }
    }
    async fn generate_thread_title(&self, input: ThreadTitleGenerationInput) -> Result<ThreadTitleGenerationResult, TaggedError> {
        let mut args = json!({
            "cwd": input.cwd, "message": input.message,
            "attachments": input.attachments.iter().map(|a| a.0.clone()).collect::<Vec<_>>(),
            "modelSelection": input.model_selection.0,
        });
        if let Some(previous) = input.previous_title {
            args["previousTitle"] = json!(previous);
        }
        self.title_calls.record(args.clone());
        let once = {
            let mut once = self.title_once.lock().unwrap();
            (!once.is_empty()).then(|| once.remove(0))
        };
        let hook = once.or_else(|| self.title_hook.lock().unwrap().clone());
        match hook {
            Some(hook) => hook(args).await,
            None => Err(disabled("generateThreadTitle")),
        }
    }
}

/// `TerminalManager.closeIdle` mock.
#[derive(Default)]
pub struct FakeTerminals {
    pub close_idle: Recorder,
    pub close_idle_once: Mutex<Vec<Hook<(), ()>>>,
    pub close_idle_hook: Mutex<Option<Hook<(), ()>>>,
    pub close: Recorder,
    pub close_hook: Mutex<Option<ResultHook>>,
}

fn no_terminal<T>() -> Result<T, TaggedError> {
    Err(TaggedError::new("Defect", "unexpected terminal call in test"))
}

#[async_trait]
impl TerminalManager for FakeTerminals {
    async fn open(&self, _input: ports::TerminalOpenInput) -> Result<ports::TerminalSessionSnapshot, TaggedError> {
        no_terminal()
    }
    async fn attach(&self, _input: ports::TerminalAttachInput) -> Result<EventStream<ports::TerminalAttachStreamEvent>, TaggedError> {
        no_terminal()
    }
    async fn write(&self, _input: ports::TerminalWriteInput) -> Result<(), TaggedError> {
        no_terminal()
    }
    async fn resize(&self, _input: ports::TerminalResizeInput) -> Result<(), TaggedError> {
        no_terminal()
    }
    async fn clear(&self, _input: ports::TerminalClearInput) -> Result<(), TaggedError> {
        no_terminal()
    }
    async fn restart(&self, _input: ports::TerminalRestartInput) -> Result<ports::TerminalSessionSnapshot, TaggedError> {
        no_terminal()
    }
    async fn close(&self, input: ports::TerminalCloseInput) -> Result<(), TaggedError> {
        self.close.record(input.0.clone());
        let hook = self.close_hook.lock().unwrap().clone();
        match hook {
            Some(hook) => hook(input.0).await,
            None => Ok(()),
        }
    }
    async fn close_idle(&self, thread_id: &ports::ThreadId, _terminal_id: Option<&str>) {
        self.close_idle.record(json!({"threadId": thread_id}));
        let once = {
            let mut once = self.close_idle_once.lock().unwrap();
            (!once.is_empty()).then(|| once.remove(0))
        };
        let hook = once.or_else(|| self.close_idle_hook.lock().unwrap().clone());
        if let Some(hook) = hook {
            hook(()).await;
        }
    }
    fn subscribe(&self) -> EventStream<ports::TerminalEvent> {
        futures::StreamExt::boxed(futures::stream::empty())
    }
    fn subscribe_metadata(&self) -> EventStream<ports::TerminalMetadataStreamEvent> {
        futures::StreamExt::boxed(futures::stream::empty())
    }
}

/// The TS harness's `reactorOrchestrationLayer`: dispatch hooks around the real engine.
pub struct HookedEngine {
    pub inner: Arc<OrchestrationEngine>,
    pub completion_failures: usize,
    pub completion_attempts: AtomicUsize,
    pub before_ready: Mutex<Option<Hook<(), ()>>>,
    pub before_replay: Mutex<Option<Hook<(), ()>>>,
    pub after_replay: Mutex<Option<Hook<(), ()>>>,
}

#[async_trait]
impl OrchestrationDispatch for HookedEngine {
    async fn dispatch(&self, command: OrchestrationCommand, origin: Option<ports::OrchestrationClientOrigin>) -> Result<DispatchResult, TaggedError> {
        let value = serde_json::to_value(&command).unwrap_or(Value::Null);
        let kind = s(&value, "type");
        if kind == "thread.title.regeneration.complete" {
            let attempt = self.completion_attempts.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt <= self.completion_failures {
                return Err(TaggedError::new("Defect", "Injected title regeneration completion failure"));
            }
        }
        let is_replay = kind == "thread.turn.start" && s(&value, "commandId").starts_with("server:after-compaction:");
        let before = if kind == "thread.session.set" && value["session"]["status"] == "ready" {
            self.before_ready.lock().unwrap().clone()
        } else if is_replay {
            self.before_replay.lock().unwrap().clone()
        } else {
            None
        };
        if let Some(before) = before {
            before(()).await;
        }
        let result = OrchestrationDispatch::dispatch(&*self.inner, command, origin).await?;
        if is_replay {
            let after = self.after_replay.lock().unwrap().clone();
            if let Some(after) = after {
                after(()).await;
            }
        }
        Ok(result)
    }
    fn subscribe_domain_events(&self) -> EventStream<OrchestrationEvent> {
        self.inner.subscribe_domain_events()
    }
    async fn latest_sequence(&self) -> i64 {
        OrchestrationDispatch::latest_sequence(&*self.inner).await
    }
    fn read_events(&self, from: i64, limit: Option<u32>) -> EventStream<Result<OrchestrationEvent, TaggedError>> {
        OrchestrationDispatch::read_events(&*self.inner, from, limit)
    }
    fn read_thread_events(&self, range: ThreadReplayRange, limit: Option<u32>) -> EventStream<Result<OrchestrationEvent, TaggedError>> {
        OrchestrationDispatch::read_thread_events(&*self.inner, range, limit)
    }
    async fn get_thread_replay_stats(&self, range: ThreadReplayRange, max_events: u32) -> Result<ThreadReplayStats, TaggedError> {
        OrchestrationDispatch::get_thread_replay_stats(&*self.inner, range, max_events).await
    }
}

#[derive(Default)]
pub struct CommandOptions {
    pub initial_title: Option<String>,
    pub defer_start: bool,
    pub thread_model_selection: Option<Value>,
    pub unsupported_model_switch: bool,
    pub requires_new_thread_for_model_change: bool,
    pub completion_failures: usize,
    pub title_regeneration_before_start: usize,
    pub activation: Option<Latch>,
}

pub struct CommandHarness {
    pub engine: Arc<HookedEngine>,
    pub reads: Arc<EventLogReactorReads>,
    pub reactor: ProviderCommandReactor,
    pub providers: Arc<CommandProviders>,
    pub auth: Arc<FakeAuth>,
    pub git: Arc<FakeGit>,
    pub vcs: Arc<FakeVcsStatus>,
    pub text: Arc<FakeTextGeneration>,
    pub terminals: Arc<FakeTerminals>,
    pub settings: Arc<MemorySettings>,
    pub state_dir: tempfile::TempDir,
    activation: Option<Latch>,
}

pub fn turn_start_for(thread_id: &str, command_id: &str, message_id: &str, text: &str, created_at: &str) -> Value {
    json!({
        "type": "thread.turn.start", "commandId": command_id, "threadId": thread_id,
        "message": {"messageId": message_id, "role": "user", "text": text, "attachments": []},
        "interactionMode": "default", "runtimeMode": "approval-required", "createdAt": created_at,
    })
}

impl CommandHarness {
    pub async fn new(options: CommandOptions) -> Self {
        let model_selection = options
            .thread_model_selection
            .clone()
            .unwrap_or(json!({"instanceId": "codex", "model": "gpt-5-codex"}));
        let liveness = Arc::new(ThreadBackgroundLivenessRegistry::new());
        let (_db, inner) = super::engine(liveness).await;
        let engine = Arc::new(HookedEngine {
            inner: Arc::new(inner),
            completion_failures: options.completion_failures,
            completion_attempts: AtomicUsize::new(0),
            before_ready: Mutex::new(None),
            before_replay: Mutex::new(None),
            after_replay: Mutex::new(None),
        });
        let reads = Arc::new(EventLogReactorReads::new(engine.clone()));
        let providers = CommandProviders::new(
            model_selection.clone(),
            if options.unsupported_model_switch {
                SessionModelSwitchMode::Unsupported
            } else {
                SessionModelSwitchMode::InSession
            },
        );
        let mut snapshot = json!({"instanceId": model_selection["instanceId"]});
        if options.requires_new_thread_for_model_change {
            snapshot["requiresNewThreadForModelChange"] = json!(true);
        }
        let auth = Arc::new(FakeAuth {
            calls: Recorder::default(),
            hook: Mutex::new(None),
        });
        let git = Arc::new(FakeGit::default());
        let vcs = Arc::new(FakeVcsStatus::default());
        let text = Arc::new(FakeTextGeneration::default());
        let terminals = Arc::new(FakeTerminals::default());
        let settings = MemorySettings::new(json!({}));
        let state_dir = tempfile::tempdir().unwrap();
        let reactor = ProviderCommandReactor::new(
            CommandReactorDeps {
                engine: engine.clone(),
                reads: reads.clone(),
                providers: providers.clone(),
                provider_status: Arc::new(StatusReads(vec![snapshot])),
                provider_auth: auth.clone(),
                workspace_snapshots: None,
                git: git.clone(),
                vcs_status: vcs.clone(),
                text_generation: text.clone(),
                settings: settings.clone(),
                terminals: terminals.clone(),
                clock: Arc::new(ManualClock::shifted()),
                uuids: system_uuids(),
                path_exists: Arc::new(|path: &str| std::path::Path::new(path).exists()),
                title_retry_base: Duration::from_millis(10),
            },
            tokio_util::sync::CancellationToken::new(),
        );
        let harness = Self {
            engine,
            reads,
            reactor,
            providers,
            auth,
            git,
            vcs,
            text,
            terminals,
            settings,
            state_dir,
            activation: options.activation.clone(),
        };
        harness
            .dispatch(json!({
                "type": "project.create", "commandId": "cmd-project-create", "projectId": "project-1", "title": "Provider Project",
                "workspaceRoot": "/tmp/provider-project", "defaultModelSelection": model_selection, "createdAt": NOW,
            }))
            .await;
        harness
            .dispatch(json!({
                "type": "thread.create", "commandId": "cmd-thread-create", "threadId": "thread-1", "projectId": "project-1",
                "title": options.initial_title.clone().unwrap_or_else(|| "Thread".into()), "modelSelection": model_selection,
                "interactionMode": "default", "runtimeMode": "approval-required", "branch": null, "worktreePath": null, "createdAt": NOW,
            }))
            .await;
        if options.title_regeneration_before_start == 2 {
            harness
                .dispatch(json!({
                    "type": "thread.create", "commandId": "cmd-thread-create-2", "threadId": "thread-2", "projectId": "project-1",
                    "title": "Thread 2", "modelSelection": model_selection, "interactionMode": "default", "runtimeMode": "approval-required",
                    "branch": null, "worktreePath": null, "createdAt": NOW,
                }))
                .await;
        }
        for index in 0..options.title_regeneration_before_start {
            harness
                .dispatch(json!({
                    "type": "thread.meta.update", "commandId": format!("cmd-thread-title-regeneration-before-reactor-start-{}", index + 1),
                    "threadId": format!("thread-{}", index + 1), "regenerateTitle": true,
                }))
                .await;
        }
        if !options.defer_start {
            harness.start().await;
        }
        harness
    }

    pub async fn start(&self) {
        let activation = self.activation.clone().map(|latch| async move { latch.wait().await }.boxed());
        self.reactor.start_with_activation(activation).await;
    }

    pub async fn dispatch(&self, command: Value) -> i64 {
        ok(dispatch(&*self.engine, command).await)
    }

    pub async fn try_dispatch(&self, command: Value) -> Result<i64, TaggedError> {
        dispatch(&*self.engine, command).await
    }

    /// `thread.turn.start` on thread-1.
    pub async fn turn(&self, command_id: &str, message_id: &str, text: &str, created_at: &str) -> i64 {
        self.dispatch(turn_start_for("thread-1", command_id, message_id, text, created_at)).await
    }

    pub async fn drain(&self) {
        self.reactor.drain().await;
    }

    pub async fn read_model(&self) -> Value {
        self.reads.read_model().await.expect("read model")
    }

    pub async fn thread(&self, thread_id: &str) -> Value {
        let model = self.read_model().await;
        find(&model["threads"], |thread| s(thread, "id") == thread_id).cloned().unwrap_or(Value::Null)
    }

    pub async fn shell(&self, thread_id: &str) -> Value {
        zc_reactors::ReactorReads::thread_shell(&*self.reads, &zc_contracts::ThreadId::new(thread_id))
            .await
            .unwrap()
            .unwrap_or(Value::Null)
    }

    /// `readPendingTurnStarts()`: the threads with a pending turn start row.
    pub async fn pending_turn_starts(&self) -> Vec<String> {
        let model = self.read_model().await;
        let mut out = Vec::new();
        for thread in model["threads"].as_array().cloned().unwrap_or_default() {
            let id = s(&thread, "id").to_owned();
            let rows = self.reads.turns_of(&zc_contracts::ThreadId::new(&id)).await.unwrap();
            for row in rows {
                if row.turn_id.is_none() && row.state == "pending" {
                    out.push(id.clone());
                }
            }
        }
        out
    }

    pub async fn activities(&self, thread_id: &str) -> Value {
        self.thread(thread_id).await["activities"].clone()
    }
}

/// `waitFor(predicate)`: polls until true (10 s).
pub async fn wait_for<F, Fut>(predicate: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if predicate().await {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("Timed out waiting for expectation.");
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}
