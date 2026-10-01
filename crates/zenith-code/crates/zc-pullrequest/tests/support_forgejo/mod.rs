//! Fixtures of the Forgejo tests: a scripted Forgejo host behind a fake `tea` (the scripted
//! runner, fake CLIs and JSON shapes are shared with the GitLab tests).

#![allow(dead_code)]

#[path = "../support_gitlab/mod.rs"]
mod shared;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
pub use shared::*;
use zc_core::process::ProcessRunInput;
use zc_pullrequest::forgejo::ForgejoPullRequestProvider;
use zc_pullrequest::provider::ChangeRequestRef;
use zc_sourcecontrol::forgejo::{ForgejoCli, ForgejoEnvironment};
use zc_sourcecontrol::util::system_clock;

pub const BASE: &str = "https://forge.example.test";

/// One recorded answer: HTTP status, body and `link` header.
#[derive(Debug, Clone)]
pub struct Answer {
    pub status: u16,
    pub body: String,
    pub link: Option<String>,
    pub truncated: bool,
}

pub fn ok(body: Value) -> Answer {
    Answer {
        status: 200,
        body: body.to_string(),
        link: None,
        truncated: false,
    }
}

pub fn status(status: u16, body: &str) -> Answer {
    Answer {
        status,
        body: body.into(),
        link: None,
        truncated: false,
    }
}

/// A Forgejo host as `tea api --include` sees it: answers by `METHOD path` (the path below
/// `/api/v1/`, query included), and every API call it was asked for, with its body.
#[derive(Default)]
pub struct Host {
    answers: Mutex<HashMap<String, Answer>>,
    pub calls: Mutex<Vec<(String, Option<Value>)>>,
    /// `tea` is not installed.
    pub missing_tea: Mutex<bool>,
}

impl Host {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn on(&self, method_path: &str, answer: Answer) -> &Self {
        self.answers.lock().unwrap().insert(method_path.into(), answer);
        self
    }

    /// The `METHOD path` of every API call, in order.
    pub fn requests(&self) -> Vec<String> {
        self.calls.lock().unwrap().iter().map(|(request, _)| request.clone()).collect()
    }

    /// The body of the call to `METHOD path`.
    pub fn body(&self, method_path: &str) -> Option<Value> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .find(|(request, _)| request == method_path)
            .and_then(|(_, body)| body.clone())
    }

    fn answer(&self, input: &ProcessRunInput) -> Reply {
        let args = &input.args;
        if input.command == "git" {
            return exit(1, "", "fatal: not a git repository");
        }
        if *self.missing_tea.lock().unwrap() {
            return Err(zc_core::process::ProcessRunError::Spawn {
                invocation: zc_core::process::ProcessInvocation {
                    command: "tea".into(),
                    argument_count: args.len(),
                    cwd: None,
                    spawn_cwd: None,
                },
                resolved_command: None,
                resolved_argument_count: None,
                shell: Some(false),
                cause: zc_core::defect::Defect::error("Error", "No such file or directory (os error 2)"),
            });
        }
        if args.join(" ") == "login list --output json" {
            return out(&json!([{"name": "forge", "url": BASE, "ssh_host": "", "user": "maria", "default": "true"}]).to_string());
        }
        let method = args.iter().position(|arg| arg == "--method").map(|at| args[at + 1].clone()).unwrap_or_default();
        let url = args.last().cloned().unwrap_or_default();
        let path = url.strip_prefix(&format!("{BASE}/api/v1/")).unwrap_or(&url).to_owned();
        let request = format!("{method} {path}");
        let body = args
            .contains(&"--data".to_owned())
            .then(|| serde_json::from_str(input.stdin.as_deref().unwrap_or("null")).unwrap());
        self.calls.lock().unwrap().push((request.clone(), body));
        let answer = self.answers.lock().unwrap().get(&request).cloned().unwrap_or(Answer {
            status: 404,
            body: r#"{"message":"not found"}"#.into(),
            link: None,
            truncated: false,
        });
        let headers = format!(
            "HTTP/1.1 {} X\n{}",
            answer.status,
            answer.link.as_deref().map(|link| format!("Link: {link}\n")).unwrap_or_default()
        );
        if answer.truncated {
            return Ok(zc_core::process::ProcessRunOutput {
                stdout: answer.body,
                stderr: headers,
                code: Some(0),
                stdout_truncated: true,
                ..Default::default()
            });
        }
        exit(0, &answer.body, &headers)
    }

    /// The provider over this host, with no `fj` keys (an empty home), so every call goes through
    /// `tea`.
    pub fn provider(self: &Arc<Self>, home: &std::path::Path) -> ForgejoPullRequestProvider {
        let host = self.clone();
        let runner = ScriptedRunner::new();
        runner.implement(move |input| host.answer(input));
        let environment = ForgejoEnvironment {
            home: home.to_path_buf(),
            data_home: None,
            app_data: None,
            ..ForgejoEnvironment::from_process()
        };
        ForgejoPullRequestProvider::new(ForgejoCli::new(runner.process(), environment, system_clock()))
    }
}

pub fn reference(number: i64) -> ChangeRequestRef {
    ChangeRequestRef {
        cwd: "/w".into(),
        repository: "acme/web".into(),
        host: "forge.example.test".into(),
        number,
    }
}

/// A pull request as `GET repos/acme/web/pulls/:n` answers.
pub fn pull(number: i64, extra: Value) -> Value {
    let mut value = json!({
        "number": number,
        "title": format!("Pull request {number}"),
        "body": "Ships it.",
        "html_url": format!("{BASE}/acme/web/pulls/{number}"),
        "user": {"login": "maria", "full_name": "Maria Example"},
        "state": "open",
        "merged": false,
        "mergeable": true,
        "head": {"ref": format!("feat/{number}"), "sha": format!("head{number}"), "repo": {"full_name": "maria/web"}},
        "base": {"ref": "main", "sha": "base", "repo": {"full_name": "acme/web"}},
        "merge_base": "base",
        "created_at": "2026-07-01T00:00:00Z",
        "updated_at": "2026-07-02T00:00:00Z",
        "closed_at": null,
        "merged_at": null,
        "labels": [{"id": 1, "name": "backend", "color": "00ff00"}],
        "requested_reviewers": [{"login": "kit"}],
    });
    for (key, field) in extra.as_object().unwrap() {
        value[key] = field.clone();
    }
    value
}
