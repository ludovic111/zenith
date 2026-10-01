//! Shared fixtures of the Azure DevOps tests.
//!
//! - [`QueuedRunner`]: a `ProcessRunner` answering each `az` call from a queue of responders
//!   (the TS tests' `mockedExecute.mockReturnValueOnce`), with an optional fallback
//!   (`mockImplementation`), recording every call. `VcsProcess` still applies its limits, its
//!   defaults and its stderr classification, and the runner cuts stdout at the ceiling the caller
//!   asked for, as the real runner does.
//! - [`FakeAz`]: a fake `az` executable for the golden test (`testUtils/fakeCli.ts`): a shell
//!   script printing a recorded output per argument line and logging its calls.
//! - [`MockCli`]: an `AzureDevOpsPullRequestCliApi` answering from closures (the TS tests'
//!   `Layer.mock(AzureDevOpsPullRequestCli)`); a method left unset panics, like Effect's
//!   `UnimplementedError`.

#![allow(dead_code, clippy::type_complexity)]

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::future::BoxFuture;
use zc_contracts::{PullRequestAction, PullRequestComment, PullRequestMergeMethod};
use zc_core::process::{ProcessRunError, ProcessRunInput, ProcessRunOutput, ProcessRunner, SystemProcessRunner};
use zc_core::vcs_process::VcsProcess;
use zc_pullrequest::azure::cli::{AzureDevOpsIterationChanges, AzureDevOpsPullRequestPage, CliResult, ListPullRequestsInput};
use zc_pullrequest::azure::json::{AzureDevOpsItemContent, AzureDevOpsIteration, AzureDevOpsPullRequest, AzureDevOpsRepositoryLocation};
use zc_pullrequest::azure::AzureDevOpsPullRequestCliApi;
use zc_sourcecontrol::azure::AzureDevOpsCli;

/// What VcsProcess allows a read that asked for no ceiling of its own.
pub const VCS_DEFAULT_MAX_OUTPUT_BYTES: usize = 1_000_000;

pub type Answer = Result<ProcessRunOutput, ProcessRunError>;
type Responder = Box<dyn Fn(&ProcessRunInput) -> Answer + Send + Sync>;

/// A successful `az` run printing `stdout`.
pub fn output(stdout: impl Into<String>) -> Answer {
    Ok(ProcessRunOutput {
        stdout: stdout.into(),
        code: Some(0),
        ..ProcessRunOutput::default()
    })
}

/// A failed `az` run, classified by VcsProcess from its stderr.
pub fn failure(stderr: &str) -> Answer {
    Ok(ProcessRunOutput {
        stderr: stderr.into(),
        code: Some(1),
        ..ProcessRunOutput::default()
    })
}

/// A recording runner answering each call from a queue.
pub struct QueuedRunner {
    calls: Mutex<Vec<ProcessRunInput>>,
    queue: Mutex<VecDeque<Responder>>,
    fallback: Mutex<Option<Responder>>,
}

impl QueuedRunner {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::default(),
            queue: Mutex::default(),
            fallback: Mutex::default(),
        })
    }

    /// The next call prints `stdout`.
    pub fn once(&self, stdout: impl Into<String>) -> &Self {
        let stdout = stdout.into();
        self.once_with(move |_| output(stdout.clone()))
    }

    pub fn once_with(&self, respond: impl Fn(&ProcessRunInput) -> Answer + Send + Sync + 'static) -> &Self {
        self.queue.lock().unwrap().push_back(Box::new(respond));
        self
    }

    /// Every call past the queue.
    pub fn always(&self, respond: impl Fn(&ProcessRunInput) -> Answer + Send + Sync + 'static) -> &Self {
        *self.fallback.lock().unwrap() = Some(Box::new(respond));
        self
    }

    pub fn cli(self: &Arc<Self>) -> AzureDevOpsCli {
        AzureDevOpsCli::new(VcsProcess::new(self.clone()))
    }

    pub fn calls(&self) -> Vec<ProcessRunInput> {
        self.calls.lock().unwrap().clone()
    }

    pub fn count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    /// The arguments of the nth call.
    pub fn args(&self, index: usize) -> Vec<String> {
        self.calls.lock().unwrap()[index].args.clone()
    }

    /// The output ceiling the nth call ran under.
    pub fn max_output_bytes(&self, index: usize) -> Option<usize> {
        self.calls.lock().unwrap()[index].max_output_bytes
    }
}

#[async_trait]
impl ProcessRunner for QueuedRunner {
    async fn run(&self, input: ProcessRunInput) -> Answer {
        self.calls.lock().unwrap().push(input.clone());
        let next = self.queue.lock().unwrap().pop_front();
        let mut answer = match next {
            Some(respond) => respond(&input),
            None => match self.fallback.lock().unwrap().as_ref() {
                Some(respond) => respond(&input),
                None => panic!("unexpected az call: {:?}", input.args),
            },
        };
        // The runner as it really behaves: stdout cut at the ceiling the caller asked for.
        if let (Ok(result), Some(ceiling)) = (&mut answer, input.max_output_bytes) {
            if result.stdout.len() > ceiling {
                let mut cut = ceiling;
                while !result.stdout.is_char_boundary(cut) {
                    cut -= 1;
                }
                result.stdout.truncate(cut);
                result.stdout_truncated = true;
            }
        }
        answer
    }
}

/// A fake `az` in a temp `bin` directory.
pub struct FakeAz {
    pub dir: tempfile::TempDir,
    entries: Mutex<BTreeMap<String, (String, i32)>>,
}

impl FakeAz {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            dir: tempfile::Builder::new().prefix("zc-pullrequest-az-").tempdir().unwrap(),
            entries: Mutex::default(),
        })
    }

    pub fn path(&self) -> std::path::PathBuf {
        std::fs::canonicalize(self.dir.path()).unwrap()
    }

    /// `az <args joined by spaces>` prints `stdout` (and `stderr`) and exits with `code`; other
    /// argument lines print `unknown command` and exit 1.
    pub fn respond(&self, args: &str, stdout: &str, stderr: &str, code: i32) {
        let mut entries = self.entries.lock().unwrap();
        let file = format!("az-{}", entries.len());
        std::fs::write(self.dir.path().join(format!("{file}.out")), stdout).unwrap();
        std::fs::write(self.dir.path().join(format!("{file}.err")), stderr).unwrap();
        entries.insert(args.to_owned(), (file, code));
        let mut script = String::from("#!/bin/sh\nd=\"$(dirname \"$0\")\"\nprintf '%s\\n' \"$*\" >> \"$d/az.log\"\ncase \"$*\" in\n");
        for (pattern, (file, code)) in entries.iter() {
            script.push_str(&format!(
                "  '{}') cat \"$d/{file}.out\"; cat \"$d/{file}.err\" >&2; exit {code} ;;\n",
                pattern.replace('\'', "'\\''")
            ));
        }
        script.push_str("  *) echo \"unknown command: $*\" >&2; exit 1 ;;\nesac\n");
        let path = self.dir.path().join("az");
        std::fs::write(&path, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A process whose `az` is this one; everything else runs for real.
    pub fn process(self: &Arc<Self>) -> VcsProcess {
        VcsProcess::new(Arc::new(FakeAzRunner { az: self.clone() }))
    }
}

struct FakeAzRunner {
    az: Arc<FakeAz>,
}

#[async_trait]
impl ProcessRunner for FakeAzRunner {
    async fn run(&self, mut input: ProcessRunInput) -> Answer {
        if input.command == "az" {
            input.command = self.az.path().join("az").to_string_lossy().into_owned();
        }
        SystemProcessRunner.run(input).await
    }
}

type Handler<I, O> = Box<dyn Fn(I) -> BoxFuture<'static, CliResult<O>> + Send + Sync>;

/// An `AzureDevOpsPullRequestCliApi` answering from closures.
#[derive(Default)]
pub struct MockCli {
    pub get_pull_request: Option<Handler<(String, i64), AzureDevOpsPullRequest>>,
    pub list_iterations: Option<Handler<i64, Vec<AzureDevOpsIteration>>>,
    pub list_iteration_changes: Option<Handler<i64, AzureDevOpsIterationChanges>>,
    /// `(path, commit)`.
    pub read_item_content: Option<Handler<(String, String), AzureDevOpsItemContent>>,
    pub list_threads: Option<Handler<i64, Vec<PullRequestComment>>>,
}

fn unimplemented<T>(method: &str) -> T {
    panic!("MockCli.{method} is not implemented")
}

#[async_trait]
impl AzureDevOpsPullRequestCliApi for MockCli {
    async fn get_viewer(&self, _cwd: &str) -> CliResult<String> {
        unimplemented("get_viewer")
    }
    async fn list_pull_requests(&self, _input: ListPullRequestsInput) -> CliResult<AzureDevOpsPullRequestPage> {
        unimplemented("list_pull_requests")
    }
    async fn get_pull_request(&self, cwd: &str, number: i64) -> CliResult<AzureDevOpsPullRequest> {
        (self.get_pull_request.as_ref().unwrap_or_else(|| unimplemented("get_pull_request")))((cwd.to_owned(), number)).await
    }
    async fn list_threads(&self, _cwd: &str, _location: &AzureDevOpsRepositoryLocation, number: i64) -> CliResult<Vec<PullRequestComment>> {
        (self.list_threads.as_ref().unwrap_or_else(|| unimplemented("list_threads")))(number).await
    }
    async fn list_iterations(&self, _cwd: &str, _location: &AzureDevOpsRepositoryLocation, number: i64) -> CliResult<Vec<AzureDevOpsIteration>> {
        (self.list_iterations.as_ref().unwrap_or_else(|| unimplemented("list_iterations")))(number).await
    }
    async fn list_iteration_changes(
        &self,
        _cwd: &str,
        _location: &AzureDevOpsRepositoryLocation,
        _number: i64,
        iteration_id: i64,
    ) -> CliResult<AzureDevOpsIterationChanges> {
        (self.list_iteration_changes.as_ref().unwrap_or_else(|| unimplemented("list_iteration_changes")))(iteration_id).await
    }
    async fn read_item_content(&self, _cwd: &str, _location: &AzureDevOpsRepositoryLocation, path: &str, commit: &str) -> CliResult<AzureDevOpsItemContent> {
        (self.read_item_content.as_ref().unwrap_or_else(|| unimplemented("read_item_content")))((path.to_owned(), commit.to_owned())).await
    }
    async fn run_pull_request_action(
        &self,
        _cwd: &str,
        _number: i64,
        _action: PullRequestAction,
        _merge_method: Option<PullRequestMergeMethod>,
    ) -> CliResult<()> {
        unimplemented("run_pull_request_action")
    }
    async fn update_pull_request(&self, _cwd: &str, _number: i64, _title: Option<&str>, _body: Option<&str>) -> CliResult<()> {
        unimplemented("update_pull_request")
    }
    async fn set_pull_request_reviewers(&self, _cwd: &str, _number: i64, _reviewers: &[String], _requested: bool) -> CliResult<()> {
        unimplemented("set_pull_request_reviewers")
    }
}
