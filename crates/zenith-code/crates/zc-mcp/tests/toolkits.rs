//! Ports of `toolkits/pullRequests/handlers.test.ts` and of the `McpHttpServer.test.ts` cases
//! that call tools directly (`server.callTool`), through [`zc_mcp::Toolkit::call`].

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::Engine as _;
use futures::StreamExt;
use serde_json::{json, Value};
use zc_mcp::broker::{PreviewAutomationHost, PreviewAutomationResponse};
use zc_mcp::tools::pull_requests::list_thread_pull_requests;
use zc_mcp::tools::DispatchFailure;
use zc_mcp::{McpCapability, McpInvocationScope, McpServices, PreviewAutomationBroker, PullRequestBackend, Toolkit};

const THREAD_ID: &str = "thread-1";

fn scope(capabilities: &[McpCapability]) -> McpInvocationScope {
    McpInvocationScope {
        environment_id: "environment-1".into(),
        thread_id: THREAD_ID.into(),
        provider_session_id: "provider-session-1".into(),
        provider_instance_id: "codex".into(),
        capabilities: capabilities.iter().copied().collect::<HashSet<_>>(),
        issued_at: 1,
    }
}

fn github_identity() -> Value {
    json!({
        "canonicalKey": "github.com/acme/widgets",
        "locator": {"source": "git-remote", "remoteName": "origin", "remoteUrl": "git@github.com:Acme/Widgets.git"},
        "provider": "github", "displayName": "Acme/Widgets", "owner": "Acme", "name": "Widgets",
    })
}

fn project(identity: Value) -> Value {
    json!({
        "id": "project-1", "title": "Project", "workspaceRoot": "/workspace/project", "defaultModelSelection": null,
        "scripts": [], "repositoryIdentity": identity,
        "createdAt": "2026-08-01T00:00:00.000Z", "updatedAt": "2026-08-01T00:00:00.000Z",
    })
}

fn thread(pull_requests: Vec<Value>) -> Value {
    json!({
        "id": THREAD_ID, "projectId": "project-1", "title": "Thread",
        "modelSelection": {"instanceId": "codex", "model": "gpt-5"}, "runtimeMode": "full-access",
        "interactionMode": "default", "branch": null, "worktreePath": null, "pullRequests": pull_requests,
        "latestTurn": null, "createdAt": "2026-08-01T00:00:00.000Z", "updatedAt": "2026-08-20T00:00:00.000Z",
        "archivedAt": null, "settledOverride": null, "settledAt": null, "session": null,
        "latestUserMessageAt": "2026-08-20T00:00:00.000Z", "hasPendingApprovals": false,
        "hasPendingUserInput": false, "hasActionableProposedPlan": false,
    })
}

fn link(number: i64, head: Option<(&str, &str)>, source: &str) -> Value {
    json!({
        "host": "github.com", "repository": "acme/widgets", "number": number,
        "url": format!("https://github.com/acme/widgets/pull/{number}"), "source": source,
        "linkedAt": "2026-08-10T00:00:00.000Z",
        "snapshot": head.map(|(head, base)| json!({
            "state": "open", "title": format!("PR {number}"), "headBranch": head, "baseBranch": base,
            "isDraft": false, "updatedAt": null, "syncedAt": "2026-08-27T00:00:00.000Z",
        })),
        "stack": null,
    })
}

type Reject = Box<dyn Fn(&Value) -> bool + Send + Sync>;

struct Backend {
    thread: Value,
    project: Value,
    reject: Reject,
    commands: Mutex<Vec<Value>>,
}

#[async_trait]
impl PullRequestBackend for Backend {
    async fn thread_shell(&self, thread_id: &str) -> Result<Option<Value>, String> {
        Ok((thread_id == THREAD_ID && !self.thread.is_null()).then(|| self.thread.clone()))
    }
    async fn project_shell(&self, _project_id: &str) -> Result<Option<Value>, String> {
        Ok((!self.project.is_null()).then(|| self.project.clone()))
    }
    async fn dispatch(&self, command: Value) -> Result<(), DispatchFailure> {
        if (self.reject)(&command) {
            return Err(DispatchFailure::Invariant);
        }
        self.commands.lock().unwrap().push(command);
        Ok(())
    }
}

struct Harness {
    toolkit: Toolkit,
    backend: Arc<Backend>,
    broker: PreviewAutomationBroker,
    _dir: tempfile::TempDir,
}

fn harness_with(thread: Value, project: Value, reject: Reject) -> Harness {
    let backend = Arc::new(Backend {
        thread,
        project,
        reject,
        commands: Mutex::new(Vec::new()),
    });
    let broker = PreviewAutomationBroker::new();
    let dir = tempfile::tempdir().unwrap();
    let toolkit = Toolkit::new(McpServices {
        broker: broker.clone(),
        pull_requests: backend.clone(),
        attachments_dir: dir.path().join("attachments"),
        browser_artifacts_dir: dir.path().join("browser-artifacts"),
    });
    Harness {
        toolkit,
        backend,
        broker,
        _dir: dir,
    }
}

fn harness() -> Harness {
    harness_with(thread(Vec::new()), project(github_identity()), Box::new(|_| false))
}

impl Harness {
    async fn call(&self, tool: &str, arguments: Value, capabilities: &[McpCapability]) -> Value {
        self.toolkit.call(tool, Some(&arguments), &scope(capabilities)).await.expect("the call decodes")
    }

    async fn pr(&self, tool: &str, arguments: Value) -> Value {
        self.call(tool, arguments, &[McpCapability::PullRequests]).await
    }

    fn commands(&self) -> Vec<Value> {
        self.backend.commands.lock().unwrap().clone()
    }
}

fn structured(result: &Value) -> &Value {
    assert_eq!(result["isError"], false, "{result}");
    &result["structuredContent"]
}

fn error_text(result: &Value) -> &str {
    assert_eq!(result["isError"], true, "{result}");
    result["content"][0]["text"].as_str().unwrap()
}

// pullRequests/handlers.test.ts

#[tokio::test]
async fn refuses_a_credential_without_the_pull_requests_capability() {
    let harness = harness();
    let result = harness.call("list_thread_pull_requests", json!({}), &[McpCapability::Preview]).await;
    assert_eq!(error_text(&result), "MCP credential does not grant the pull-requests capability.");
    assert!(harness.commands().is_empty());
}

#[tokio::test]
async fn links_by_url_with_source_agent_on_the_tokens_thread() {
    let harness = harness();
    let result = harness
        .pr("link_pull_request", json!({"url": "https://github.com/Acme/Widgets/pull/123/files"}))
        .await;
    assert_eq!(
        structured(&result),
        &json!({"host": "github.com", "repository": "acme/widgets", "number": 123, "url": "https://github.com/Acme/Widgets/pull/123/files", "alreadyLinked": false})
    );
    let commands = harness.commands();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0]["type"], "thread.pull-request.link");
    assert_eq!(commands[0]["threadId"], THREAD_ID);
    assert_eq!(commands[0]["host"], "github.com");
    assert_eq!(commands[0]["repository"], "acme/widgets");
    assert_eq!(commands[0]["number"], 123);
    assert_eq!(commands[0]["source"], "agent");
    assert!(commands[0]["commandId"].as_str().unwrap().starts_with("server:mcp-pr-link:thread-1:"));
}

#[tokio::test]
async fn links_by_repository_and_number_defaulting_the_host_to_the_projects() {
    let harness = harness();
    let result = harness.pr("link_pull_request", json!({"repository": "Acme/Other", "number": 7})).await;
    assert_eq!(
        structured(&result),
        &json!({"host": "github.com", "repository": "acme/other", "number": 7, "url": "https://github.com/acme/other/pull/7", "alreadyLinked": false})
    );
}

#[tokio::test]
async fn builds_the_url_in_the_project_hosts_own_shape() {
    let harness = harness_with(
        thread(Vec::new()),
        project(json!({
            "canonicalKey": "gitlab.com/group/sub/project",
            "locator": {"source": "git-remote", "remoteName": "origin", "remoteUrl": "git@gitlab.com:group/sub/project.git"},
            "provider": "gitlab", "displayName": "group/sub/project",
        })),
        Box::new(|_| false),
    );
    let result = harness.pr("link_pull_request", json!({"repository": "group/sub/project", "number": 42})).await;
    assert_eq!(structured(&result)["url"], "https://gitlab.com/group/sub/project/-/merge_requests/42");
    assert_eq!(structured(&result)["host"], "gitlab.com");
}

#[tokio::test]
async fn links_a_numeric_forgejo_reference_with_its_remotes_web_origin_and_mount_path() {
    let harness = harness_with(
        thread(Vec::new()),
        project(json!({
            "canonicalKey": "forge.example/git/owner/repo",
            "locator": {"source": "git-remote", "remoteName": "origin", "remoteUrl": "http://forge.example:3000/git/owner/repo.git"},
            "provider": "forgejo", "displayName": "git/owner/repo",
        })),
        Box::new(|_| false),
    );
    let result = harness.pr("link_pull_request", json!({"repository": "git/owner/repo", "number": 42})).await;
    assert_eq!(
        structured(&result),
        &json!({"host": "forge.example:3000", "repository": "git/owner/repo", "number": 42, "url": "http://forge.example:3000/git/owner/repo/pulls/42", "alreadyLinked": false})
    );
    let other = harness
        .pr("link_pull_request", json!({"host": "other.example", "repository": "owner/repo", "number": 42}))
        .await;
    assert_eq!(structured(&other)["url"], "https://other.example/owner/repo/pull/42");
}

#[tokio::test]
async fn rejects_a_target_that_names_neither_a_url_nor_repository_and_number() {
    let harness = harness();
    let incomplete = harness.pr("link_pull_request", json!({"repository": "x/y"})).await;
    assert_eq!(error_text(&incomplete), "Pass either url, or both repository and number.");
    let unknown = harness
        .pr(
            "link_pull_request",
            json!({"url": "https://github.com/acme/widgets/issues/1?token=private-value"}),
        )
        .await;
    assert_eq!(
        error_text(&unknown),
        "This is not a recognised pull request URL. Pass repository and number instead."
    );
    assert!(!error_text(&unknown).contains("private-value"));
    assert!(harness.commands().is_empty());
}

#[tokio::test]
async fn reports_a_missing_project_host() {
    let harness = harness_with(thread(Vec::new()), Value::Null, Box::new(|_| false));
    let result = harness.pr("link_pull_request", json!({"repository": "acme/widgets", "number": 1})).await;
    assert_eq!(error_text(&result), "This thread's project has no recognised remote. Pass host or url.");
}

#[tokio::test]
async fn treats_a_duplicate_link_as_already_linked_rather_than_an_error() {
    let harness = harness_with(
        thread(vec![link(123, None, "manual")]),
        project(github_identity()),
        Box::new(|command| command["type"] == "thread.pull-request.link"),
    );
    let result = harness
        .pr("link_pull_request", json!({"url": "https://github.com/acme/widgets/pull/123"}))
        .await;
    assert_eq!(structured(&result)["alreadyLinked"], true);
}

#[tokio::test]
async fn unlinks_a_linked_pull_request_and_reports_a_missing_one_as_was_linked_false() {
    let harness = harness_with(
        thread(vec![link(5, None, "manual")]),
        project(github_identity()),
        Box::new(|command| command["type"] == "thread.pull-request.unlink" && command["number"] != 5),
    );
    let linked = harness.pr("unlink_pull_request", json!({"repository": "acme/widgets", "number": 5})).await;
    assert_eq!(
        structured(&linked),
        &json!({"host": "github.com", "repository": "acme/widgets", "number": 5, "wasLinked": true})
    );
    let missing = harness
        .pr("unlink_pull_request", json!({"url": "https://github.com/acme/widgets/pull/9"}))
        .await;
    assert_eq!(structured(&missing)["wasLinked"], false);
    let commands = harness.commands();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0]["type"], "thread.pull-request.unlink");
    assert_eq!(commands[0]["number"], 5);
}

#[test]
fn reports_an_older_forgejo_links_http_port_when_listing_thread_links() {
    let mut forgejo = link(42, None, "manual");
    forgejo["host"] = json!("forge.example");
    forgejo["url"] = json!("http://forge.example:3000/acme/widgets/pulls/42");
    let result = list_thread_pull_requests(&thread(vec![forgejo]));
    assert_eq!(result["pullRequests"][0]["host"], "forge.example:3000");
}

#[tokio::test]
async fn fails_cleanly_when_the_tokens_thread_no_longer_exists() {
    let harness = harness_with(Value::Null, project(github_identity()), Box::new(|_| false));
    let result = harness.pr("list_thread_pull_requests", json!({})).await;
    assert_eq!(error_text(&result), "Thread thread-1 was not found.");
}

#[tokio::test]
async fn lists_visible_links_with_host_state_and_derived_chain_order() {
    let harness = harness_with(
        thread(vec![
            link(3, Some(("feat-c", "feat-b")), "agent"),
            link(1, Some(("feat-a", "main")), "created"),
            link(2, Some(("feat-b", "feat-a")), "agent"),
            link(9, None, "stack-dismissed"),
            link(10, None, "manual"),
        ]),
        project(github_identity()),
        Box::new(|_| false),
    );
    let result = harness.pr("list_thread_pull_requests", json!({})).await;
    let listed = structured(&result);
    let numbers: Vec<i64> = listed["pullRequests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["number"].as_i64().unwrap())
        .collect();
    assert_eq!(numbers, vec![3, 1, 2, 10]);
    assert_eq!(
        listed["pullRequests"][0],
        json!({
            "host": "github.com", "repository": "acme/widgets", "number": 3, "url": "https://github.com/acme/widgets/pull/3",
            "source": "agent", "state": "open", "title": "PR 3", "headBranch": "feat-c", "baseBranch": "feat-b",
            "isDraft": false, "stack": {"kind": "derived", "position": 3, "size": 3},
        })
    );
    assert_eq!(listed["pullRequests"][3]["state"], Value::Null);
    assert_eq!(listed["pullRequests"][3]["stack"], Value::Null);
    assert_eq!(
        listed["chains"],
        json!([{"kind": "derived", "numbers": [1, 2, 3]}, {"kind": "derived", "numbers": [10]}])
    );
}

#[test]
fn reports_a_native_stack_position_for_each_member() {
    let stack = json!({
        "kind": "native", "id": "stack-1", "number": 1, "url": "https://github.com/acme/widgets/stack/1", "base": "main",
        "layers": [{"number": 1, "headBranch": "a", "state": "open"}, {"number": 2, "headBranch": "b", "state": "open"}],
    });
    let mut second = link(2, None, "stack");
    second["stack"] = stack.clone();
    let mut first = link(1, None, "created");
    first["stack"] = stack;
    let result = list_thread_pull_requests(&json!({"pullRequests": [second, first]}));
    assert_eq!(result["pullRequests"][0]["stack"], json!({"kind": "native", "position": 2, "size": 2}));
    assert_eq!(result["pullRequests"][1]["stack"], json!({"kind": "native", "position": 1, "size": 2}));
    assert_eq!(result["chains"], json!([{"kind": "native", "numbers": [1, 2]}]));
}

// McpHttpServer.test.ts cases that call tools directly.

const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jRZkAAAAASUVORK5CYII=";

fn snapshot_result() -> Value {
    json!({
        "url": "http://example.test/", "title": "Example", "loading": false, "visibleText": "Example",
        "interactiveElements": [], "accessibilityTree": {}, "consoleEntries": [], "networkEntries": [], "actionTimeline": [],
        "screenshot": {"mimeType": "image/png", "data": base64::engine::general_purpose::STANDARD.encode(b"png"), "width": 10, "height": 5},
    })
}

/// Connects a host that answers every request with `respond(request)`; returns the requests.
async fn serve(broker: &PreviewAutomationBroker, respond: impl Fn(&Value) -> Value + Send + Sync + 'static) -> Arc<Mutex<Vec<Value>>> {
    let mut events = broker.connect(PreviewAutomationHost {
        client_id: "mcp-test-client".into(),
        environment_id: "environment-1".into(),
        supported_operations: None,
    });
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let broker = broker.clone();
    let (connected, wait) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let mut connected = Some(connected);
        while let Some(event) = events.next().await {
            if event["type"] == "connected" {
                let _ = connected.take().unwrap().send(());
                continue;
            }
            let request = event["request"].clone();
            recorded.lock().unwrap().push(request.clone());
            let response = respond(&request);
            broker.respond(PreviewAutomationResponse {
                client_id: "mcp-test-client".into(),
                connection_id: event["connectionId"].as_str().unwrap().into(),
                request_id: request["requestId"].as_str().unwrap().into(),
                ok: response["ok"].as_bool().unwrap(),
                result: response.get("result").cloned(),
                error: response.get("error").cloned(),
            });
        }
    });
    wait.await.unwrap();
    requests
}

async fn snapshot(harness: &Harness, arguments: Value) -> Value {
    harness.call("preview_snapshot", arguments, &[McpCapability::Preview]).await
}

#[tokio::test]
async fn returns_bounded_structural_preview_snapshot_failures() {
    for arguments in [json!({}), json!({"includeImage": false})] {
        let harness = harness();
        serve(&harness.broker, |_| {
            json!({"ok": false, "error": {"_tag": "PreviewAutomationExecutionError", "message": "sensitive renderer failure", "detail": {"consoleOutput": "sensitive browser output"}}})
        })
        .await;
        let result = snapshot(&harness, arguments).await;
        let message = "Preview automation snapshot failed on client mcp-test-client.";
        assert_eq!(
            result["content"],
            json!([{"type": "text", "text": format!("Preview snapshot failed: {message}")}])
        );
        assert_eq!(
            result["structuredContent"],
            json!({"error": {"_tag": "PreviewAutomationExecutionError", "operation": "snapshot", "failureCount": 1, "message": message}})
        );
    }
}

#[tokio::test]
async fn tells_the_agent_to_open_a_tab_when_the_snapshot_has_none() {
    for (arguments, advice) in [
        (json!({}), "No active preview tab was found for snapshot. Call preview_open first.".to_owned()),
        (
            json!({"tabId": "tab-mcp-alternate"}),
            "Preview tab tab-mcp-alternate was not found for snapshot. Omit tabId to use the current tab, or call preview_open.".to_owned(),
        ),
    ] {
        let harness = harness();
        serve(
            &harness.broker,
            |_| json!({"ok": false, "error": {"_tag": "PreviewAutomationTabNotFoundError", "message": "no tab"}}),
        )
        .await;
        let result = snapshot(&harness, arguments).await;
        assert_eq!(
            result["content"],
            json!([{"type": "text", "text": format!("Preview snapshot failed: {advice}")}])
        );
    }
}

#[tokio::test]
async fn tells_the_agent_how_to_fall_back_when_no_desktop_app_can_run_the_snapshot() {
    let harness = harness();
    let result = snapshot(&harness, json!({})).await;
    assert!(result["content"][0]["text"].as_str().unwrap().contains("use a headless browser from the shell"));
    assert_eq!(result["structuredContent"]["error"]["_tag"], "PreviewAutomationNoAvailableHostError");
}

#[tokio::test]
async fn returns_fresh_snapshots_on_repeated_calls() {
    for (input, images) in [
        (json!({}), true),
        (json!({"includeImage": true}), true),
        (json!({"includeImage": false}), false),
    ] {
        let harness = harness();
        let count = Arc::new(Mutex::new(0));
        let counter = count.clone();
        let requests = serve(&harness.broker, move |request| {
            assert_eq!(request["operation"], "snapshot");
            assert_eq!(request["tabId"], "tab-mcp-alternate");
            assert_eq!(request["threadId"], THREAD_ID);
            assert_eq!(request["input"], json!({}));
            *counter.lock().unwrap() += 1;
            let n = *counter.lock().unwrap();
            json!({"ok": true, "result": {
                "url": "http://example.test/", "title": format!("Snapshot {n}"), "loading": false, "visibleText": "Save your changes",
                "interactiveElements": [{"tag": "button", "role": "button", "name": "Save", "selector": "#save", "x": 0, "y": 0, "width": 20, "height": 10}],
                "accessibilityTree": {"role": "document", "name": "Example"}, "consoleEntries": [], "networkEntries": [], "actionTimeline": [],
                "screenshot": {"mimeType": "image/png", "width": 1, "height": 1, "data": PNG},
            }})
        })
        .await;
        for call in 1..=6 {
            let mut arguments = input.clone();
            arguments["tabId"] = json!("tab-mcp-alternate");
            let result = snapshot(&harness, arguments).await;
            assert_eq!(result["isError"], false);
            let bounded = json!({
                "url": "http://example.test/", "title": format!("Snapshot {call}"), "loading": false, "visibleText": "Save your changes",
                "interactiveElements": [{"tag": "button", "role": "button", "name": "Save", "selector": "#save", "x": 0, "y": 0, "width": 20, "height": 10}],
                "consoleEntries": [], "networkEntries": [], "actionTimeline": [],
                "screenshot": {"mimeType": "image/png", "width": 1, "height": 1},
            });
            let mut expected_structured = bounded.clone();
            expected_structured["omitted"] = json!(["accessibilityTree (use interactiveElements locators or preview_evaluate)"]);
            assert_eq!(result["structuredContent"], expected_structured);
            let content = result["content"].as_array().unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(content[0]["text"].as_str().unwrap()).unwrap(),
                json!({"url": "http://example.test/"})
            );
            assert_eq!(serde_json::from_str::<Value>(content[1]["text"].as_str().unwrap()).unwrap(), bounded);
            assert_eq!(
                content[2],
                json!({"type": "text", "text": "Snapshot text was bounded. Omitted: accessibilityTree (use interactiveElements locators or preview_evaluate)."})
            );
            if images {
                assert_eq!(content[3], json!({"type": "image", "data": PNG, "mimeType": "image/png"}));
            } else {
                assert_eq!(content.len(), 3);
            }
        }
        // Output selection belongs to this call, not the session's history.
        let next = snapshot(&harness, json!({"tabId": "tab-mcp-alternate"})).await;
        let types: Vec<&str> = next["content"].as_array().unwrap().iter().map(|c| c["type"].as_str().unwrap()).collect();
        assert_eq!(types, vec!["text", "text", "text", "image"]);
        assert_eq!(next["structuredContent"]["title"], "Snapshot 7");
        assert!(next["structuredContent"].get("accessibilityTree").is_none());
        assert_eq!(requests.lock().unwrap().len(), 7);
    }
}

#[tokio::test]
async fn rejects_non_boolean_snapshot_image_options_before_selecting_a_browser_host() {
    let harness = harness();
    for include_image in [json!("false"), json!(0), Value::Null] {
        let result = snapshot(&harness, json!({"includeImage": include_image})).await;
        assert_eq!(result["isError"], true);
        assert_eq!(result["content"], json!([{"type": "text", "text": "Preview snapshot failed: AiError."}]));
        assert_eq!(
            result["structuredContent"],
            json!({"error": {"_tag": "AiError", "operation": "snapshot", "failureCount": 1}})
        );
    }
}

#[tokio::test]
async fn saves_the_snapshot_png_on_request_and_reports_its_path() {
    let harness = harness();
    let requests = serve(&harness.broker, |_| json!({"ok": true, "result": snapshot_result()})).await;
    let artifacts = harness.toolkit.services().browser_artifacts_dir.clone();
    let saved = snapshot(&harness, json!({"save": true})).await;
    assert_eq!(saved["isError"], false);
    // The browser never receives the server-only `save` flag.
    assert_eq!(requests.lock().unwrap()[0]["input"], json!({}));
    let path = saved["structuredContent"]["screenshotPath"].as_str().unwrap().to_owned();
    assert_eq!(Path::new(&path).parent().unwrap(), artifacts);
    let name = Path::new(&path).file_name().unwrap().to_str().unwrap();
    assert!(
        regex::Regex::new(r"^browser-screenshot-example-test-[0-9a-z]+-[0-9a-f]{8}\.png$")
            .unwrap()
            .is_match(name),
        "{name}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"png");
    assert!(saved["content"][1]["text"].as_str().unwrap().contains(&path));

    let unsaved = snapshot(&harness, json!({})).await;
    assert!(unsaved["structuredContent"].get("screenshotPath").is_none());

    // A save without the image skips the page dump.
    let path_only = snapshot(&harness, json!({"save": true, "includeImage": false})).await;
    let saved = &path_only["structuredContent"];
    assert_eq!(saved["url"], "http://example.test/");
    assert_eq!(saved.as_object().unwrap().len(), 2);
    assert_eq!(std::fs::read(saved["screenshotPath"].as_str().unwrap()).unwrap(), b"png");
    let content = path_only["content"].as_array().unwrap();
    assert_eq!(content.len(), 1);
    assert_eq!(&serde_json::from_str::<Value>(content[0]["text"].as_str().unwrap()).unwrap(), saved);
}

#[tokio::test]
async fn reports_a_tagged_error_when_the_screenshot_cannot_be_saved() {
    let harness = harness();
    let artifacts = harness.toolkit.services().browser_artifacts_dir.clone();
    std::fs::create_dir_all(artifacts.parent().unwrap()).unwrap();
    // A regular file where the artifacts directory should be makes every write fail.
    std::fs::write(&artifacts, "").unwrap();
    serve(&harness.broker, |_| json!({"ok": true, "result": snapshot_result()})).await;
    let result = snapshot(&harness, json!({"save": true})).await;
    assert_eq!(
        result["content"],
        json!([{"type": "text", "text": "Preview snapshot failed: PreviewScreenshotSaveError."}])
    );
    assert_eq!(
        result["structuredContent"],
        json!({"error": {"_tag": "PreviewScreenshotSaveError", "operation": "snapshot", "failureCount": 1}})
    );
}

#[tokio::test]
async fn surfaces_a_missing_capability_as_a_tool_error() {
    let harness = harness();
    // A preview-only credential: the token predates the toolkit or was minted elsewhere.
    let denied = harness.call("list_thread_pull_requests", json!({}), &[McpCapability::Preview]).await;
    assert_eq!(
        denied["content"],
        json!([{"type": "text", "text": "MCP credential does not grant the pull-requests capability."}])
    );
}

#[tokio::test]
async fn preserves_authenticated_request_context_and_wraps_evaluate_values() {
    let harness = harness();
    let requests = serve(&harness.broker, |request| match request["operation"].as_str() {
        Some("snapshot") => json!({"ok": true, "result": snapshot_result()}),
        Some("evaluate") => json!({"ok": true, "result": ["Connect", "Continue"]}),
        Some("press") => json!({"ok": true}),
        _ => json!({"ok": true, "result": {"available": true, "visible": true, "tabId": "tab-mcp-test", "url": "http://example.test/", "title": "Example", "loading": false}}),
    })
    .await;
    let tool_icon = json!({"_tag": "website", "pageUrl": "http://example.test/"});
    let status = harness.call("preview_status", json!({}), &[McpCapability::Preview]).await;
    assert_eq!(structured(&status)["available"], true);
    assert_eq!(structured(&status)["tabId"], "tab-mcp-test");

    let malformed = harness
        .toolkit
        .call("preview_click", Some(&json!({"selector": ""})), &scope(&[McpCapability::Preview]))
        .await;
    assert_eq!(
        malformed.unwrap_err().message(),
        "Invalid parameters for tool 'preview_click': Expected a value with a length of at least 1\n  at [\"selector\"]"
    );

    let shot = harness
        .call("preview_snapshot", json!({"tabId": "tab-mcp-alternate"}), &[McpCapability::Preview])
        .await;
    assert!(shot["content"].as_array().unwrap().iter().any(|content| content["type"] == "image"));
    assert_eq!(
        shot["structuredContent"]["screenshot"],
        json!({"mimeType": "image/png", "width": 10, "height": 5})
    );
    assert_eq!(
        requests.lock().unwrap().iter().find(|request| request["operation"] == "snapshot").unwrap()["tabId"],
        "tab-mcp-alternate"
    );

    // Arrays and primitives are wrapped so structuredContent stays a JSON object.
    let evaluated = harness
        .call("preview_evaluate", json!({"expression": "buttons()"}), &[McpCapability::Preview])
        .await;
    assert_eq!(structured(&evaluated), &json!({"toolIcon": tool_icon, "value": ["Connect", "Continue"]}));
    assert_eq!(
        evaluated["content"][0]["text"].as_str().unwrap(),
        r#"{"toolIcon":{"_tag":"website","pageUrl":"http://example.test/"},"value":["Connect","Continue"]}"#
    );

    for (tool, arguments) in [
        ("preview_click", json!({"x": 10, "y": 10})),
        ("preview_type", json!({"text": "Hello"})),
        ("preview_press", json!({"key": "Enter"})),
        ("preview_scroll", json!({"deltaY": 100})),
        ("preview_wait_for", json!({"text": "Example"})),
    ] {
        let result = harness.call(tool, arguments, &[McpCapability::Preview]).await;
        assert_eq!(structured(&result), &json!({"toolIcon": tool_icon}), "{tool}");
        assert_eq!(requests.lock().unwrap().last().unwrap()["operation"], "status");
        assert_eq!(
            serde_json::from_str::<Value>(result["content"][0]["text"].as_str().unwrap()).unwrap(),
            json!({"toolIcon": tool_icon})
        );
    }
}
