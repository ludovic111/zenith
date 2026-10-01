//! Port of `project/ProjectSetupScriptRunner.test.ts`: a recording terminal manager, a project
//! from the real engine, scripted settings.

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use regex::Regex;
use serde_json::{json, Value};
use zc_checkpoints::setup_script::{
    CompletionShell, ObserveCompletion, ProjectSetupScriptRunner, ProjectSetupScriptRunnerError, SetupScriptInput, SetupScriptOperation, SetupScriptResult,
    SetupScriptRunner,
};
use zc_ports::TaggedError;

fn setup_script(id: &str, command: &str) -> Value {
    json!({"id": id, "name": "Setup", "command": command, "icon": "configure", "runOnWorktreeCreate": true})
}

struct Fixture {
    runner: ProjectSetupScriptRunner,
    terminals: Arc<FakeTerminals>,
    _engine: zc_orchestration::engine::OrchestrationEngine,
}

async fn fixture(project_scripts: Value, settings: Value, shell: CompletionShell) -> Fixture {
    let engine = engine().await;
    dispatch(
        &engine,
        json!({"type": "project.create", "commandId": "cmd-project", "projectId": "project-1", "title": "Project", "workspaceRoot": "/repo/project",
               "defaultModelSelection": null, "createdAt": NOW}),
    )
    .await;
    if project_scripts.as_array().is_some_and(|s| !s.is_empty()) {
        dispatch(
            &engine,
            json!({"type": "project.meta.update", "commandId": "cmd-scripts", "projectId": "project-1", "scripts": project_scripts}),
        )
        .await;
    }
    let terminals = Arc::new(FakeTerminals::default());
    let runner = ProjectSetupScriptRunner::with_shell(
        Arc::new(EngineProjections { engine: engine.clone() }),
        terminals.clone(),
        MemorySettings::new(settings),
        shell,
    );
    Fixture {
        runner,
        terminals,
        _engine: engine,
    }
}

fn input(project_id: Option<&str>, project_cwd: Option<&str>, observe: Option<ObserveCompletion>) -> SetupScriptInput {
    SetupScriptInput {
        thread_id: "thread-1".into(),
        project_id: project_id.map(str::to_owned),
        project_cwd: project_cwd.map(str::to_owned),
        worktree_path: "/repo/worktrees/a".into(),
        preferred_terminal_id: None,
        observe_completion: observe,
    }
}

fn started(result: SetupScriptResult) -> zc_checkpoints::setup_script::SetupScriptStarted {
    match result {
        SetupScriptResult::Started(started) => started,
        SetupScriptResult::NoScript => panic!("expected a started script"),
    }
}

#[tokio::test]
async fn runs_the_inherited_machine_setup_action_in_the_checkouts_worktree() {
    let f = fixture(
        json!([]),
        json!({"defaultProjectScripts": [setup_script("default-setup", "npm install")]}),
        CompletionShell::Posix,
    )
    .await;
    let result = started(f.runner.run_for_thread(input(Some("project-1"), None, None)).await.unwrap());
    assert_eq!(result.script_id, "default-setup");
    assert_eq!(
        f.terminals.opens.lock().unwrap()[0],
        json!({"threadId": "thread-1", "terminalId": "setup-default-setup", "cwd": "/repo/worktrees/a", "worktreePath": "/repo/worktrees/a",
               "env": {"T3CODE_PROJECT_ROOT": "/repo/project", "T3CODE_WORKTREE_PATH": "/repo/worktrees/a", "NO_COLOR": "1", "FORCE_COLOR": "0"}})
    );
    assert_eq!(
        f.terminals.writes.lock().unwrap()[0],
        json!({"threadId": "thread-1", "terminalId": "setup-default-setup", "data": "npm install\r"})
    );
}

#[tokio::test]
async fn returns_no_script_when_no_setup_script_exists() {
    let f = fixture(json!([]), json!({}), CompletionShell::Posix).await;
    let result = f.runner.run_for_thread(input(Some("project-1"), None, None)).await.unwrap();
    assert!(matches!(result, SetupScriptResult::NoScript));
    assert!(f.terminals.opens.lock().unwrap().is_empty());
    assert!(f.terminals.writes.lock().unwrap().is_empty());
}

#[tokio::test]
async fn opens_the_deterministic_setup_terminal_with_worktree_env_and_writes_the_command() {
    let f = fixture(json!([setup_script("setup", "bun install")]), json!({}), CompletionShell::Posix).await;
    let result = started(f.runner.run_for_thread(input(None, Some("/repo/project"), None)).await.unwrap());
    assert_eq!(
        (
            result.script_id.as_str(),
            result.script_name.as_str(),
            result.script_command.as_str(),
            result.terminal_id.as_str(),
            result.cwd.as_str()
        ),
        ("setup", "Setup", "bun install", "setup-setup", "/repo/worktrees/a")
    );
    assert!(result.r#async);
    assert!(result.completion.is_none());
    assert_eq!(f.terminals.opens.lock().unwrap()[0]["env"]["NO_COLOR"], json!("1"));
    assert_eq!(
        f.terminals.writes.lock().unwrap()[0],
        json!({"threadId": "thread-1", "terminalId": "setup-setup", "data": "bun install\r"})
    );
}

fn output(f: &Fixture, data: &str) {
    f.terminals
        .emit(json!({"threadId": "thread-1", "terminalId": "setup-setup", "type": "output", "data": data}));
}

#[tokio::test]
async fn wraps_the_command_with_a_completion_sentinel_and_resolves_the_exit_code_from_terminal_output() {
    let f = fixture(json!([setup_script("setup", "bun install")]), json!({}), CompletionShell::Posix).await;
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = seen.clone();
    let observe = ObserveCompletion {
        on_output_line: Some(Arc::new(move |line: &str| sink.lock().unwrap().push(line.to_owned()))),
    };
    let result = started(f.runner.run_for_thread(input(None, Some("/repo/project"), Some(observe))).await.unwrap());
    // The subscription is attached before the command is written.
    assert_eq!(f.terminals.subscribers(), 1);
    let written = f.terminals.writes.lock().unwrap()[0]["data"].as_str().unwrap().to_owned();
    let sentinel = Regex::new(r"__T3_SETUP_DONE___[0-9a-f]{32}:")
        .unwrap()
        .find(&written)
        .unwrap()
        .as_str()
        .to_owned();
    assert_eq!(written, format!("( bun install\r); printf '\\n{sentinel}%s\\n' \"$?\"\r"));

    output(&f, &format!("( bun install\r\n> ); printf '\\n{sentinel}%s\\n' \"$?\"\r\n"));
    output(&f, "\u{1b}[32mResolving");
    output(&f, " deps\u{1b}[0m\r\n");
    output(&f, "Progress: 1/3\rProgress: 2/3\rProgress: 3/3\r\nDone in 2s\r\n");
    output(&f, "__T3_SETUP_DONE__:0\r\n");
    output(&f, &format!("__T3_SETUP_DONE___{}:0\r\n", "0".repeat(32)));
    output(&f, &format!("{sentinel}3\r\n"));

    let completion = result.completion.unwrap().await;
    assert_eq!(completion.exit_code, Some(3));
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            "Resolving deps".to_owned(),
            "Progress: 1/3".into(),
            "Progress: 2/3".into(),
            "Progress: 3/3".into(),
            "Done in 2s".into(),
            "__T3_SETUP_DONE__:0".into(),
            format!("__T3_SETUP_DONE___{}:0", "0".repeat(32)),
        ]
    );
    // The subscription is torn down once the sentinel arrives; a failed run keeps its shell.
    wait_for(|| f.terminals.subscribers() == 0).await;
    assert!(f.terminals.close_idles.lock().unwrap().is_empty());
}

#[tokio::test]
async fn closes_the_idle_setup_shell_after_a_clean_exit() {
    let f = fixture(json!([setup_script("setup", "bun install")]), json!({}), CompletionShell::Posix).await;
    let result = started(
        f.runner
            .run_for_thread(input(None, Some("/repo/project"), Some(ObserveCompletion::default())))
            .await
            .unwrap(),
    );
    let written = f.terminals.writes.lock().unwrap()[0]["data"].as_str().unwrap().to_owned();
    let sentinel = Regex::new(r"__T3_SETUP_DONE___[0-9a-f]{32}:")
        .unwrap()
        .find(&written)
        .unwrap()
        .as_str()
        .to_owned();
    output(&f, &format!("{sentinel}0\r\n"));
    assert_eq!(result.completion.unwrap().await.exit_code, Some(0));
    assert_eq!(
        *f.terminals.close_idles.lock().unwrap(),
        vec![("thread-1".to_owned(), Some("setup-setup".to_owned()))]
    );
}

#[tokio::test]
async fn an_exit_before_the_sentinel_settles_without_an_exit_code() {
    let f = fixture(json!([setup_script("setup", "bun install")]), json!({}), CompletionShell::Posix).await;
    let result = started(
        f.runner
            .run_for_thread(input(None, Some("/repo/project"), Some(ObserveCompletion::default())))
            .await
            .unwrap(),
    );
    f.terminals
        .emit(json!({"threadId": "thread-1", "terminalId": "setup-setup", "type": "exited", "exitCode": 1, "exitSignal": null}));
    assert_eq!(result.completion.unwrap().await.exit_code, None);
}

#[tokio::test]
async fn unsubscribes_from_terminal_output_when_the_command_cannot_be_written() {
    let f = fixture(json!([setup_script("setup", "bun install")]), json!({}), CompletionShell::Posix).await;
    *f.terminals.write_error.lock().unwrap() = Some(TaggedError::new("TerminalCwdStatError", "Terminal cwd stat failed: /repo/worktrees/a"));
    let error = f
        .runner
        .run_for_thread(input(None, Some("/repo/project"), Some(ObserveCompletion::default())))
        .await
        .err()
        .unwrap();
    assert!(matches!(
        error,
        ProjectSetupScriptRunnerError::Operation {
            operation: SetupScriptOperation::WriteCommand,
            ..
        }
    ));
    wait_for(|| f.terminals.subscribers() == 0).await;
}

#[tokio::test]
async fn wraps_the_command_for_each_shell_syntax() {
    for (shell, expected) in [
        (
            CompletionShell::Fish,
            r"^begin\rbun install\rend; printf '\\n__T3_SETUP_DONE___[0-9a-f]{32}:%s\\n' \$status\r$",
        ),
        (
            CompletionShell::Posix,
            r#"^\( bun install\r\); printf '\\n__T3_SETUP_DONE___[0-9a-f]{32}:%s\\n' "\$\?"\r$"#,
        ),
    ] {
        let f = fixture(json!([setup_script("setup", "bun install")]), json!({}), shell).await;
        f.runner
            .run_for_thread(input(None, Some("/repo/project"), Some(ObserveCompletion::default())))
            .await
            .unwrap();
        let written = f.terminals.writes.lock().unwrap()[0]["data"].as_str().unwrap().to_owned();
        assert!(Regex::new(expected).unwrap().is_match(&written), "{shell:?}: {written:?}");
    }
}

#[tokio::test]
async fn keeps_terminal_failures_as_the_cause_of_a_structured_operation_error() {
    let f = fixture(json!([setup_script("setup", "bun install")]), json!({}), CompletionShell::Posix).await;
    *f.terminals.open_error.lock().unwrap() = Some(TaggedError::new("TerminalCwdStatError", "stat failed"));
    let error = f.runner.run_for_thread(input(Some("project-1"), None, None)).await.err().unwrap();
    let ProjectSetupScriptRunnerError::Operation { context, operation, cause } = &error else {
        panic!("operation error")
    };
    assert_eq!(*operation, SetupScriptOperation::OpenTerminal);
    assert_eq!(context.thread_id, "thread-1");
    assert_eq!(context.project_id.as_deref(), Some("project-1"));
    assert_eq!(context.worktree_path, "/repo/worktrees/a");
    assert_eq!(cause.0, json!({"name": "TerminalCwdStatError", "message": "stat failed"}));
    assert_eq!(
        serde_json::to_value(&error).unwrap(),
        json!({"_tag": "ProjectSetupScriptOperationError", "threadId": "thread-1", "projectId": "project-1", "worktreePath": "/repo/worktrees/a",
               "operation": "openTerminal", "cause": {"name": "TerminalCwdStatError", "message": "stat failed"}})
    );
    assert_eq!(
        error.message(),
        "Project setup script operation 'openTerminal' failed for thread 'thread-1' in '/repo/worktrees/a'."
    );
    assert_eq!(error.compatibility_detail(), "stat failed");
}

#[tokio::test]
async fn an_unknown_project_is_not_found() {
    let f = fixture(json!([]), json!({}), CompletionShell::Posix).await;
    let error = f
        .runner
        .run_for_thread(input(Some("project-unknown"), Some("/elsewhere"), None))
        .await
        .err()
        .unwrap();
    assert_eq!(error.tag(), "ProjectSetupScriptProjectNotFoundError");
    assert_eq!(error.compatibility_detail(), "Project was not found for setup script execution.");
}
