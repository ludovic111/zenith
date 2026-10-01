//! Shared fixtures for the GitHub provider tests.
//!
//! [`FakeGh`] is a `ProcessRunner` standing in for `gh`, the Rust counterpart of the TS tests'
//! `Layer.mock(GitHubCli)({ execute })`: every invocation is recorded (argv, stdin, env) and
//! answered by the next queued reply (`mockReturnValueOnce`) or the standing one
//! (`mockReturnValue` / `mockImplementation`). It sits under the real `VcsProcess` and
//! `GitHubCli`, so classification of failures and pinned credentials are the real ones; the
//! quota probe `GitHubCli` sends before `pr list`/`pr view` (`gh api rate_limit`) is answered
//! with nothing to learn and left out of the record.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::FutureExt;
use serde_json::Value;
use zc_core::process::{ProcessRunError, ProcessRunInput, ProcessRunOutput, ProcessRunner};
use zc_core::vcs_process::VcsProcess;
use zc_pullrequest::github::cli::GitHubPullRequestCli;
use zc_pullrequest::provider::ChangeRequestRef;
use zc_sourcecontrol::github::GitHubCli;
use zc_sourcecontrol::util::ManualClock;

pub type Reply = Result<ProcessRunOutput, ProcessRunError>;
type Handler = Arc<dyn Fn(&ProcessRunInput) -> BoxFuture<'static, Reply> + Send + Sync>;

/// A successful `gh` answer.
pub fn ok(stdout: &str) -> Reply {
    Ok(ProcessRunOutput {
        stdout: stdout.into(),
        code: Some(0),
        ..ProcessRunOutput::default()
    })
}

/// A successful answer of JSON.
pub fn json(value: Value) -> Reply {
    ok(&value.to_string())
}

/// An answer cut at the output cap.
pub fn truncated(stdout: &str) -> Reply {
    Ok(ProcessRunOutput {
        stdout: stdout.into(),
        code: Some(0),
        stdout_truncated: true,
        ..ProcessRunOutput::default()
    })
}

/// An answer that was not valid UTF-8.
pub fn invalid_utf8(stdout: &str) -> Reply {
    Ok(ProcessRunOutput {
        stdout: stdout.into(),
        code: Some(0),
        stdout_invalid_utf8: true,
        ..ProcessRunOutput::default()
    })
}

/// A `gh` that ran and failed: `GitHubCliCommandError` unless `stderr` says otherwise.
pub fn failed(stderr: &str) -> Reply {
    Ok(ProcessRunOutput {
        stderr: stderr.into(),
        code: Some(1),
        ..ProcessRunOutput::default()
    })
}

/// `GitHubCliCommandError`.
pub fn command_failed() -> Reply {
    failed("HTTP 406: the diff exceeded the maximum number of files (300)")
}

/// `GitHubPullRequestNotFoundError`.
pub fn not_found() -> Reply {
    failed("GraphQL: pull request not found")
}

/// `GitHubCliAuthenticationError`.
pub fn unauthenticated() -> Reply {
    failed("To get started with GitHub CLI, please run:  gh auth login")
}

/// `GitHubCliRateLimitError`.
pub fn rate_limited() -> Reply {
    failed("HTTP 403: API rate limit exceeded for user")
}

fn is_quota(input: &ProcessRunInput) -> bool {
    input.args.first().map(String::as_str) == Some("api") && input.args.get(1).map(String::as_str) == Some("rate_limit")
}

/// The fake `gh`.
#[derive(Default)]
pub struct FakeGh {
    calls: Mutex<Vec<ProcessRunInput>>,
    queue: Mutex<VecDeque<Handler>>,
    standing: Mutex<Option<Handler>>,
}

impl FakeGh {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// `mockReturnValueOnce`.
    pub fn once(&self, reply: Reply) -> &Self {
        self.queue
            .lock()
            .unwrap()
            .push_back(Arc::new(move |_| futures::future::ready(reply.clone_reply()).boxed()));
        self
    }

    /// `mockReturnValue`.
    pub fn always(&self, reply: Reply) {
        *self.standing.lock().unwrap() = Some(Arc::new(move |_| futures::future::ready(reply.clone_reply()).boxed()));
    }

    /// `mockImplementation`.
    pub fn respond(&self, respond: impl Fn(&ProcessRunInput) -> Reply + Send + Sync + 'static) {
        *self.standing.lock().unwrap() = Some(Arc::new(move |input| futures::future::ready(respond(input)).boxed()));
    }

    /// `mockImplementation` with an answer that takes its time.
    pub fn respond_async<F>(&self, respond: impl Fn(&ProcessRunInput) -> F + Send + Sync + 'static)
    where
        F: Future<Output = Reply> + Send + 'static,
    {
        *self.standing.lock().unwrap() = Some(Arc::new(move |input| respond(input).boxed()));
    }

    pub fn calls(&self) -> Vec<ProcessRunInput> {
        self.calls.lock().unwrap().clone()
    }

    pub fn count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    /// The whole invocation the nth call made.
    pub fn call(&self, index: usize) -> ProcessRunInput {
        self.calls.lock().unwrap().get(index).cloned().unwrap_or_else(|| panic!("no call {index}"))
    }

    pub fn args(&self, index: usize) -> Vec<String> {
        self.call(index).args
    }

    pub fn stdin(&self, index: usize) -> String {
        self.call(index).stdin.unwrap_or_default()
    }

    /// The stdin of the nth call, parsed.
    pub fn body(&self, index: usize) -> Value {
        serde_json::from_str(&self.stdin(index)).unwrap()
    }

    pub fn env(&self, index: usize, key: &str) -> Option<String> {
        self.call(index).env.as_ref()?.get(key).cloned().flatten()
    }

    /// The one argument `--search` carries, `None` for a read with no `--search` at all.
    pub fn search_of(&self, index: usize) -> Option<String> {
        let args = self.args(index);
        let flag = args.iter().position(|arg| arg == "--search")?;
        args.get(flag + 1).cloned()
    }

    /// The search a batched read sent, which travels in the request body.
    pub fn search_query_of(&self, index: usize) -> Option<String> {
        self.body(index)["variables"]["q"].as_str().map(str::to_owned)
    }

    /// The value after `--limit`.
    pub fn limit_of(&self, index: usize) -> String {
        let args = self.args(index);
        let flag = args.iter().position(|arg| arg == "--limit").unwrap();
        args[flag + 1].clone()
    }
}

trait CloneReply {
    fn clone_reply(&self) -> Reply;
}

impl CloneReply for Reply {
    fn clone_reply(&self) -> Reply {
        match self {
            Ok(output) => Ok(output.clone()),
            Err(error) => Err(error.clone()),
        }
    }
}

#[async_trait]
impl ProcessRunner for FakeGh {
    async fn run(&self, input: ProcessRunInput) -> Reply {
        if is_quota(&input) {
            return ok("{}");
        }
        self.calls.lock().unwrap().push(input.clone());
        let handler = self.queue.lock().unwrap().pop_front().or_else(|| self.standing.lock().unwrap().clone());
        match handler {
            Some(handler) => handler(&input).await,
            None => panic!("unexpected gh call: {}", input.args.join(" ")),
        }
    }
}

/// The real `GitHubCli` over a fake `gh`.
pub fn github(gh: &Arc<FakeGh>, clock: &Arc<ManualClock>) -> GitHubCli {
    GitHubCli::new(VcsProcess::new(gh.clone()), clock.clone())
}

/// A fresh service over a fake `gh`, with a clock at 0 (Effect's `TestClock`).
pub fn setup() -> (Arc<FakeGh>, GitHubPullRequestCli, Arc<ManualClock>) {
    let gh = FakeGh::new();
    let clock = ManualClock::new(0);
    let cli = GitHubPullRequestCli::new(github(&gh, &clock), clock.clone());
    (gh, cli, clock)
}

/// `{cwd: "/w", repository: "acme/web", host, number}`.
pub fn pr(host: &str, number: i64) -> ChangeRequestRef {
    ChangeRequestRef {
        cwd: "/w".into(),
        repository: "acme/web".into(),
        host: host.into(),
        number,
    }
}

/// Owned strings.
pub fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}
