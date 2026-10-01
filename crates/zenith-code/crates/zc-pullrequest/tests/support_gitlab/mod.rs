//! Fixtures of the GitLab tests: a scripted process runner standing in for `glab` (the TS tests'
//! `Layer.mock(GitLabCli)({execute: vi.fn()})`), one level lower so the real `GitLabCli` and
//! `VcsProcess` still build the invocation and classify failures.

#![allow(dead_code)]

pub mod fake;
pub mod shape;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use zc_core::process::{ProcessRunError, ProcessRunInput, ProcessRunOutput, ProcessRunner};
use zc_core::vcs_process::VcsProcess;
use zc_pullrequest::gitlab::{GitLabPullRequestCli, GitLabPullRequestProvider};
use zc_sourcecontrol::gitlab::GitLabCli;

pub type Reply = Result<ProcessRunOutput, ProcessRunError>;
type Implementation = Box<dyn Fn(&ProcessRunInput) -> Reply + Send + Sync>;

/// A recording runner: one-shot replies first (`mockReturnValueOnce`), then the standing one
/// (`mockReturnValue` / `mockImplementation`).
#[derive(Default)]
pub struct ScriptedRunner {
    calls: Mutex<Vec<ProcessRunInput>>,
    once: Mutex<VecDeque<Reply>>,
    always: Mutex<Option<Implementation>>,
}

impl ScriptedRunner {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn once(self: &Arc<Self>, reply: Reply) -> &Arc<Self> {
        self.once.lock().unwrap().push_back(reply);
        self
    }

    pub fn always(self: &Arc<Self>, reply: Reply) -> &Arc<Self> {
        *self.always.lock().unwrap() = Some(Box::new(move |_| reply.clone()));
        self
    }

    pub fn implement(self: &Arc<Self>, implementation: impl Fn(&ProcessRunInput) -> Reply + Send + Sync + 'static) -> &Arc<Self> {
        *self.always.lock().unwrap() = Some(Box::new(implementation));
        self
    }

    pub fn calls(&self) -> Vec<ProcessRunInput> {
        self.calls.lock().unwrap().clone()
    }

    pub fn count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    /// The argv of the nth call (`argsOfCall`).
    pub fn args(&self, index: usize) -> Vec<String> {
        self.calls.lock().unwrap()[index].args.clone()
    }

    /// The endpoint of the nth `glab api` call (`argsOfCall(index)[1]`).
    pub fn path(&self, index: usize) -> String {
        self.args(index)[1].clone()
    }

    pub fn stdin(&self, index: usize) -> Option<String> {
        self.calls.lock().unwrap()[index].stdin.clone()
    }

    /// The nth call's stdin, parsed.
    pub fn body(&self, index: usize) -> serde_json::Value {
        serde_json::from_str(&self.stdin(index).unwrap_or_else(|| "{}".into())).unwrap()
    }

    pub fn process(self: &Arc<Self>) -> VcsProcess {
        VcsProcess::new(self.clone())
    }

    pub fn cli(self: &Arc<Self>) -> GitLabPullRequestCli {
        GitLabPullRequestCli::new(GitLabCli::new(self.process()))
    }

    pub fn provider(self: &Arc<Self>) -> GitLabPullRequestProvider {
        GitLabPullRequestProvider::new(GitLabCli::new(self.process()))
    }
}

#[async_trait]
impl ProcessRunner for ScriptedRunner {
    async fn run(&self, input: ProcessRunInput) -> Reply {
        self.calls.lock().unwrap().push(input.clone());
        let once = self.once.lock().unwrap().pop_front();
        if let Some(reply) = once {
            return reply;
        }
        match &*self.always.lock().unwrap() {
            Some(implementation) => implementation(&input),
            None => exit(1, "", "unscripted call"),
        }
    }
}

/// A successful `glab` run printing `stdout`.
pub fn out(stdout: &str) -> Reply {
    Ok(ProcessRunOutput {
        stdout: stdout.into(),
        code: Some(0),
        ..ProcessRunOutput::default()
    })
}

/// A run whose stdout hit the output limit.
pub fn out_truncated(stdout: &str) -> Reply {
    Ok(ProcessRunOutput {
        stdout: stdout.into(),
        code: Some(0),
        stdout_truncated: true,
        ..ProcessRunOutput::default()
    })
}

/// A run whose stdout was not valid UTF-8.
pub fn out_invalid_utf8(stdout: &str) -> Reply {
    Ok(ProcessRunOutput {
        stdout: stdout.into(),
        code: Some(0),
        stdout_invalid_utf8: true,
        ..ProcessRunOutput::default()
    })
}

/// A finished run with an exit code and stderr.
pub fn exit(code: i32, stdout: &str, stderr: &str) -> Reply {
    Ok(ProcessRunOutput {
        stdout: stdout.into(),
        stderr: stderr.into(),
        code: Some(code),
        ..ProcessRunOutput::default()
    })
}
