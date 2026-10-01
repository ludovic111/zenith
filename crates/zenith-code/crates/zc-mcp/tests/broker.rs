//! Port of `PreviewAutomationBroker.test.ts`.
//!
//! Not ported: "rejects a routed action when its generation is evicted before delivery", which
//! suspends one fiber between route selection and delivery with a custom Effect scheduler.
//! Here routing and delivery happen in one synchronous step of the same task; the
//! re-check under the lock that the test guards is still there.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use serde_json::{json, Value};
use tokio::sync::{oneshot, Notify};
use zc_mcp::broker::{LiveTab, PreviewAutomationHost, PreviewAutomationHostFocus, PreviewAutomationResponse};
use zc_mcp::{McpCapability, McpInvocationScope, PreviewAutomationBroker, PreviewAutomationInvokeInput};

fn scope() -> McpInvocationScope {
    McpInvocationScope {
        environment_id: "environment-1".into(),
        thread_id: "thread-1".into(),
        provider_session_id: "provider-session-1".into(),
        provider_instance_id: "codex".into(),
        capabilities: HashSet::from([McpCapability::Preview]),
        issued_at: 1,
    }
}

fn session(provider_session_id: &str) -> McpInvocationScope {
    McpInvocationScope {
        provider_session_id: provider_session_id.into(),
        ..scope()
    }
}

fn host(client_id: &str) -> PreviewAutomationHost {
    PreviewAutomationHost {
        client_id: client_id.into(),
        environment_id: "environment-1".into(),
        supported_operations: None,
    }
}

fn input(operation: &str, value: Value) -> PreviewAutomationInvokeInput {
    PreviewAutomationInvokeInput::new(scope(), operation, value)
}

fn with_tab(mut input: PreviewAutomationInvokeInput, tab_id: &str) -> PreviewAutomationInvokeInput {
    input.tab_id = Some(tab_id.into());
    input
}

/// A scripted answer: `Some(response JSON {ok, result?, error?})`, or `None` for silence.
type Responder = Arc<dyn Fn(&Value) -> Option<Value> + Send + Sync>;

/// A connected host that answers with `respond`.
struct ServedHost {
    connection_id: String,
    requests: Arc<Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
}

async fn serve(broker: &PreviewAutomationBroker, host: PreviewAutomationHost, respond: Responder) -> ServedHost {
    let client_id = host.client_id.clone();
    let mut events = broker.connect(host);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (connected, connection) = oneshot::channel();
    let recorded = requests.clone();
    let broker = broker.clone();
    let task = tokio::spawn(async move {
        let mut connected = Some(connected);
        while let Some(event) = events.next().await {
            if event["type"] == "connected" {
                if let Some(connected) = connected.take() {
                    let _ = connected.send(event["connectionId"].as_str().unwrap().to_owned());
                }
                continue;
            }
            let request = event["request"].clone();
            recorded.lock().unwrap().push(request.clone());
            if let Some(response) = respond(&request) {
                broker.respond(PreviewAutomationResponse {
                    client_id: client_id.clone(),
                    connection_id: event["connectionId"].as_str().unwrap().to_owned(),
                    request_id: request["requestId"].as_str().unwrap().to_owned(),
                    ok: response["ok"].as_bool().unwrap(),
                    result: response.get("result").cloned(),
                    error: response.get("error").cloned(),
                });
            }
        }
    });
    let connection_id = connection.await.expect("the host connects");
    ServedHost { connection_id, requests, task }
}

fn ok(result: Value) -> Option<Value> {
    Some(json!({"ok": true, "result": result}))
}

fn always(result: Value) -> Responder {
    Arc::new(move |_| ok(result.clone()))
}

fn tag(error: &zc_ports::TaggedError) -> &str {
    &error.tag
}

#[tokio::test]
async fn atomically_registers_a_connected_host_and_correlates_its_response() {
    let broker = PreviewAutomationBroker::new();
    let _host = serve(&broker, host("client-1"), always(json!({"available": true}))).await;
    let outcome = broker.invoke(input("open", json!({}))).await;
    assert_eq!(outcome.result.unwrap(), json!({"available": true}));
}

#[tokio::test]
async fn targets_multiple_tabs_explicitly_while_retaining_a_default_tab() {
    let broker = PreviewAutomationBroker::new();
    let opened = Arc::new(Mutex::new(vec!["tab-ios-simulator", "tab-web-app"]));
    let respond: Responder = Arc::new(move |request| {
        if request["operation"] == "open" {
            ok(json!({"available": true, "tabId": opened.lock().unwrap().pop().unwrap()}))
        } else {
            ok(json!({"url": "http://localhost:3200"}))
        }
    });
    let served = serve(&broker, host("client-1"), respond).await;
    broker.invoke(input("open", json!({"reuseExistingTab": false}))).await.result.unwrap();
    broker.invoke(input("open", json!({"reuseExistingTab": false}))).await.result.unwrap();
    broker.invoke(input("snapshot", json!({}))).await.result.unwrap();
    broker.invoke(with_tab(input("snapshot", json!({})), "tab-web-app")).await.result.unwrap();
    broker.invoke(input("snapshot", json!({}))).await.result.unwrap();
    let routed = served.requests.lock().unwrap().clone();
    assert_eq!(routed.len(), 5);
    assert!(routed[0].get("tabId").is_none());
    assert_eq!(routed[1]["tabId"], "tab-web-app");
    assert_eq!(routed[2]["tabId"], "tab-ios-simulator");
    assert_eq!(routed[2]["tabIdExplicit"], false);
    assert_eq!(routed[3]["tabId"], "tab-web-app");
    assert_eq!(routed[3]["tabIdExplicit"], true);
    assert_eq!(routed[4]["tabId"], "tab-web-app");
}

/// A host whose answer to the request matching `held` waits until a request matching
/// `releaser` was answered.
async fn serve_with_held_answer(
    broker: &PreviewAutomationBroker,
    result: impl Fn(&Value) -> Value + Send + Sync + 'static,
    held: impl Fn(&Value) -> bool + Send + Sync + 'static,
    releaser: impl Fn(&Value) -> bool + Send + Sync + 'static,
) -> Arc<Mutex<Vec<Value>>> {
    let mut events = broker.connect(host("client-1"));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let release = Arc::new(Notify::new());
    let broker_task = broker.clone();
    let (connected, connection) = oneshot::channel();
    let result = Arc::new(result);
    let held = Arc::new(held);
    let releaser = Arc::new(releaser);
    tokio::spawn(async move {
        let mut connected = Some(connected);
        while let Some(event) = events.next().await {
            if event["type"] == "connected" {
                let _ = connected.take().unwrap().send(());
                continue;
            }
            let request = event["request"].clone();
            recorded.lock().unwrap().push(request.clone());
            let broker = broker_task.clone();
            let release = release.clone();
            let (result, held, releaser) = (result.clone(), held.clone(), releaser.clone());
            let connection_id = event["connectionId"].as_str().unwrap().to_owned();
            tokio::spawn(async move {
                if held(&request) {
                    release.notified().await;
                }
                broker.respond(PreviewAutomationResponse {
                    client_id: "client-1".into(),
                    connection_id,
                    request_id: request["requestId"].as_str().unwrap().to_owned(),
                    ok: true,
                    result: Some(result(&request)),
                    error: None,
                });
                if releaser(&request) {
                    release.notify_one();
                }
            });
        }
    });
    connection.await.unwrap();
    requests
}

#[tokio::test]
async fn keeps_an_older_target_stable_while_a_newer_explicit_tab_responds() {
    for implicit in [true, false] {
        let broker = PreviewAutomationBroker::new();
        let requests = serve_with_held_answer(
            &broker,
            |_| json!({"url": "http://localhost:3200"}),
            |request| request["tabId"] == "tab-older-request" && request["operation"] == "snapshot",
            |request| request["tabId"] == "tab-newer-request",
        )
        .await;
        broker.invoke(with_tab(input("status", json!({})), "tab-older-request")).await.result.unwrap();
        let older_input = if implicit {
            input("snapshot", json!({}))
        } else {
            with_tab(input("snapshot", json!({})), "tab-older-request")
        };
        let older = tokio::spawn({
            let broker = broker.clone();
            async move { broker.invoke(older_input).await }
        });
        tokio::task::yield_now().await;
        while requests.lock().unwrap().len() < 2 {
            tokio::task::yield_now().await;
        }
        broker.invoke(with_tab(input("snapshot", json!({})), "tab-newer-request")).await.result.unwrap();
        let older = older.await.unwrap();
        older.result.unwrap();
        let mut status = with_tab(input("status", json!({})), "tab-older-request");
        status.update_current_tab = false;
        broker.invoke(status).await.result.unwrap();
        broker.invoke(input("snapshot", json!({}))).await.result.unwrap();
        assert_eq!(requests.lock().unwrap().last().unwrap()["tabId"], "tab-newer-request", "implicit: {implicit}");
        assert_eq!(older.routed_tab, Some(Some("tab-older-request".to_owned())));
    }
}

#[tokio::test]
async fn tracks_the_tab_returned_by_a_targeted_recording_stop() {
    let broker = PreviewAutomationBroker::new();
    let served = serve(
        &broker,
        host("client-1"),
        Arc::new(|request: &Value| match request["operation"].as_str() {
            Some("open") => ok(json!({"available": true, "tabId": "tab-session-b"})),
            Some("recordingStop") => ok(json!({"id": "recording-1", "tabId": "tab-session-a-recording"})),
            _ => ok(json!({"url": "http://localhost:3200"})),
        }),
    )
    .await;
    broker.invoke(input("open", json!({}))).await.result.unwrap();
    broker.invoke(input("recordingStop", json!({}))).await.result.unwrap();
    broker.invoke(input("snapshot", json!({}))).await.result.unwrap();
    assert_eq!(served.requests.lock().unwrap().last().unwrap()["tabId"], "tab-session-a-recording");
}

#[tokio::test]
async fn does_not_let_a_no_tab_response_suppress_an_earlier_tab_decision() {
    let broker = PreviewAutomationBroker::new();
    let requests = serve_with_held_answer(
        &broker,
        |request| {
            if request["operation"] == "open" {
                let tab = if request["input"]["marker"] == "older" {
                    "tab-opened-late"
                } else {
                    "tab-initial"
                };
                json!({"available": true, "tabId": tab})
            } else {
                json!({"url": "http://localhost:3200"})
            }
        },
        |request| request["input"]["marker"] == "older",
        |request| request["input"]["marker"] == "newer",
    )
    .await;
    broker.invoke(input("open", json!({}))).await.result.unwrap();
    let older = tokio::spawn({
        let broker = broker.clone();
        async move { broker.invoke(input("open", json!({"marker": "older", "reuseExistingTab": false}))).await }
    });
    while requests.lock().unwrap().len() < 2 {
        tokio::task::yield_now().await;
    }
    broker.invoke(input("snapshot", json!({"marker": "newer"}))).await.result.unwrap();
    older.await.unwrap().result.unwrap();
    broker.invoke(input("snapshot", json!({}))).await.result.unwrap();
    assert_eq!(requests.lock().unwrap().last().unwrap()["tabId"], "tab-opened-late");
}

#[tokio::test]
async fn announces_a_live_replacement_stream_before_delivering_requests() {
    let broker = PreviewAutomationBroker::new();
    let mut events = broker.connect(host("client-1"));
    let first = events.next().await.unwrap();
    assert_eq!(first["type"], "connected");
    let invoke = tokio::spawn({
        let broker = broker.clone();
        async move { broker.invoke(input("status", json!({}))).await }
    });
    let second = events.next().await.unwrap();
    assert_eq!(second["type"], "request");
    assert_eq!(second["connectionId"], first["connectionId"]);
    broker.respond(PreviewAutomationResponse {
        client_id: "client-1".into(),
        connection_id: second["connectionId"].as_str().unwrap().into(),
        request_id: second["request"]["requestId"].as_str().unwrap().into(),
        ok: true,
        result: Some(json!("ready")),
        error: None,
    });
    assert_eq!(invoke.await.unwrap().result.unwrap(), json!("ready"));
}

fn failing(error: Value) -> Responder {
    Arc::new(move |_| Some(json!({"ok": false, "error": error.clone()})))
}

#[tokio::test]
async fn preserves_bounded_request_and_remote_selector_diagnostics() {
    let locator = "role=button[name='request-secret']";
    let remote_message = "Unexpected token near remote-secret.";
    let remote =
        json!({"_tag": "PreviewAutomationInvalidSelectorError", "message": remote_message, "detail": {"selector": "role=button[name='remote-secret']"}});
    let broker = PreviewAutomationBroker::new();
    let _host = serve(&broker, host("client-1"), failing(remote.clone())).await;
    let mut request = with_tab(input("click", json!({"locator": locator})), "tab-1");
    request.timeout_ms = Some(1_234);
    let error = broker.invoke(request).await.result.unwrap_err();
    assert_eq!(tag(&error), "PreviewAutomationInvalidSelectorError");
    let fields = &error.fields;
    assert_eq!(fields["operation"], "click");
    assert_eq!(fields["environmentId"], "environment-1");
    assert_eq!(fields["threadId"], "thread-1");
    assert_eq!(fields["providerSessionId"], "provider-session-1");
    assert_eq!(fields["providerInstanceId"], "codex");
    assert_eq!(fields["clientId"], "client-1");
    assert_eq!(fields["requestId"], "preview-0");
    assert_eq!(fields["tabId"], "tab-1");
    assert_eq!(fields["timeoutMs"], 1_234);
    assert_eq!(fields["selectorKind"], "locator");
    assert_eq!(fields["selectorLength"], locator.len());
    assert_eq!(fields["remoteTag"], "PreviewAutomationInvalidSelectorError");
    assert_eq!(fields["remoteMessageLength"], remote_message.len());
    assert_eq!(fields["remoteDetailKind"], "object");
    assert_eq!(fields["cause"], remote);
    assert_eq!(
        error.message,
        format!("Preview automation click received an invalid locator ({} characters).", locator.len())
    );
    assert!(!error.message.contains("secret"));
    assert!(!fields.contains_key("selector") && !fields.contains_key("remoteMessage") && !fields.contains_key("remoteDetail"));
}

#[tokio::test]
async fn classifies_a_remote_non_editable_target_without_collapsing_it_to_execution() {
    let broker = PreviewAutomationBroker::new();
    let remote = json!({"_tag": "PreviewAutomationTargetNotEditableError", "message": "remote target details", "detail": {"selectorKind": "focused-element"}});
    let _host = serve(&broker, host("client-1"), failing(remote)).await;
    let error = broker
        .invoke(with_tab(input("type", json!({"text": "hello"})), "tab-1"))
        .await
        .result
        .unwrap_err();
    assert_eq!(tag(&error), "PreviewAutomationTargetNotEditableError");
    assert_eq!(error.fields["operation"], "type");
    assert_eq!(error.fields["tabId"], "tab-1");
    assert_eq!(error.fields["selectorKind"], "focused-element");
    assert_eq!(error.fields["remoteTag"], "PreviewAutomationTargetNotEditableError");
    assert_eq!(error.message, "Preview automation type requires an editable focused element.");
}

#[tokio::test]
async fn preserves_recording_failures() {
    for tag_name in [
        "PreviewAutomationRecordingTransferError",
        "PreviewAutomationRecordingDesktopUpdateRequiredError",
        "PreviewAutomationRecordingTooLargeError",
        "PreviewAutomationRecordingDeadlineExpiredError",
    ] {
        let broker = PreviewAutomationBroker::new();
        let remote = json!({"_tag": tag_name, "message": "remote recording details", "detail": {"reason": "untrusted-reason", "threadId": "untrusted-thread"}});
        let _host = serve(&broker, host("client-1"), failing(remote.clone())).await;
        let error = broker.invoke(input("recordingStop", json!({}))).await.result.unwrap_err();
        assert_eq!(error.tag, tag_name);
        assert_eq!(error.fields["threadId"], "thread-1");
        assert_eq!(error.fields["cause"], remote);
        assert!(error.message.contains("remains on the desktop"), "{tag_name}");
        assert!(!error.message.contains("remote recording details"));
    }
}

#[tokio::test]
async fn distinguishes_malformed_remote_failures() {
    let broker = PreviewAutomationBroker::new();
    let _host = serve(&broker, host("client-1"), Arc::new(|_| Some(json!({"ok": false})))).await;
    let mut request = input("status", json!({}));
    request.timeout_ms = Some(2_000);
    let error = broker.invoke(request).await.result.unwrap_err();
    assert_eq!(tag(&error), "PreviewAutomationMalformedResponseError");
    assert_eq!(error.fields["operation"], "status");
    assert_eq!(error.fields["clientId"], "client-1");
    assert_eq!(error.fields["requestId"], "preview-0");
    assert_eq!(error.fields["timeoutMs"], 2_000);
}

#[tokio::test]
async fn rejects_calls_when_no_connected_host_exists() {
    let broker = PreviewAutomationBroker::new();
    let outcome = broker.invoke(input("status", json!({}))).await;
    let error = outcome.result.unwrap_err();
    assert_eq!(tag(&error), "PreviewAutomationNoAvailableHostError");
    assert_eq!(error.fields["operation"], "status");
    assert_eq!(error.fields["environmentId"], "environment-1");
    assert_eq!(error.fields["threadId"], "thread-1");
    assert_eq!(error.fields["providerSessionId"], "provider-session-1");
    assert_eq!(error.fields["providerInstanceId"], "codex");
    assert_eq!(outcome.routed_tab, None);
}

#[tokio::test]
async fn does_not_create_host_state_from_focus_updates_without_a_live_stream() {
    let broker = PreviewAutomationBroker::new();
    broker.focus_host(PreviewAutomationHostFocus {
        client_id: "client-1".into(),
        environment_id: "environment-1".into(),
        connection_id: "connection-missing".into(),
        focused: true,
        live_tabs: None,
    });
    assert_eq!(
        tag(&broker.invoke(input("status", json!({}))).await.result.unwrap_err()),
        "PreviewAutomationNoAvailableHostError"
    );
}

#[tokio::test]
async fn removes_host_availability_when_the_authoritative_request_stream_disconnects() {
    let broker = PreviewAutomationBroker::new();
    let mut events = broker.connect(host("client-1"));
    // The registration starts with the stream.
    assert_eq!(
        tag(&broker.invoke(input("status", json!({}))).await.result.unwrap_err()),
        "PreviewAutomationNoAvailableHostError"
    );
    events.next().await.unwrap();
    assert_eq!(broker.hosts().len(), 1);
    drop(events);
    assert_eq!(
        tag(&broker.invoke(input("status", json!({}))).await.result.unwrap_err()),
        "PreviewAutomationNoAvailableHostError"
    );
}

#[tokio::test]
async fn routes_requests_for_background_threads_through_an_environment_level_host() {
    let broker = PreviewAutomationBroker::new();
    let served = serve(&broker, host("client-1"), always(json!("background"))).await;
    let background = McpInvocationScope {
        thread_id: "thread-background".into(),
        provider_session_id: "provider-session-background".into(),
        ..scope()
    };
    let result = broker
        .invoke(PreviewAutomationInvokeInput::new(background, "status", json!({})))
        .await
        .result
        .unwrap();
    assert_eq!(result, json!("background"));
    assert_eq!(served.requests.lock().unwrap()[0]["threadId"], "thread-background");
}

#[tokio::test]
async fn never_routes_a_provider_session_to_a_host_from_another_environment() {
    let broker = PreviewAutomationBroker::new();
    let _matching = serve(&broker, host("client-matching"), always(json!("matching"))).await;
    let foreign_host = PreviewAutomationHost {
        environment_id: "environment-foreign".into(),
        ..host("client-foreign")
    };
    let _foreign = serve(&broker, foreign_host, always(json!("foreign"))).await;
    assert_eq!(broker.invoke(input("status", json!({}))).await.result.unwrap(), json!("matching"));
}

fn focus(client_id: &str, connection_id: &str, focused: bool, live_tabs: Option<Vec<LiveTab>>) -> PreviewAutomationHostFocus {
    PreviewAutomationHostFocus {
        client_id: client_id.into(),
        environment_id: "environment-1".into(),
        connection_id: connection_id.into(),
        focused,
        live_tabs,
    }
}

fn live_tab(thread_id: &str, tab_id: &str, visible: Option<bool>) -> LiveTab {
    LiveTab {
        thread_id: thread_id.into(),
        tab_id: tab_id.into(),
        visible,
    }
}

#[tokio::test]
async fn pins_a_provider_session_to_its_initial_host_despite_later_focus_changes() {
    let broker = PreviewAutomationBroker::new();
    let first = serve(&broker, host("client-first"), always(json!("first"))).await;
    let second = serve(&broker, host("client-second"), always(json!("second"))).await;
    broker.focus_host(focus(
        "client-first",
        "connection-stale",
        true,
        Some(vec![live_tab("thread-1", "stale-tab", None)]),
    ));
    assert_eq!(broker.invoke(input("status", json!({}))).await.result.unwrap(), json!("second"));
    broker.focus_host(focus("client-first", &first.connection_id, true, None));
    let pinned = |provider_session_id: &str| PreviewAutomationInvokeInput::new(session(provider_session_id), "status", json!({}));
    assert_eq!(broker.invoke(pinned("provider-session-first-pinned")).await.result.unwrap(), json!("first"));
    broker.focus_host(focus("client-second", &second.connection_id, true, None));
    assert_eq!(broker.invoke(pinned("provider-session-first-pinned")).await.result.unwrap(), json!("first"));
    assert_eq!(broker.invoke(pinned("provider-session-second-pinned")).await.result.unwrap(), json!("second"));
}

#[tokio::test]
async fn prefers_the_live_tab_owner_for_new_sessions_without_moving_existing_leases() {
    let broker = PreviewAutomationBroker::new();
    let owner = serve(&broker, host("owner"), always(json!("owner"))).await;
    let other = serve(&broker, host("other"), always(json!("other"))).await;
    broker.focus_host(focus(
        "owner",
        &owner.connection_id,
        false,
        Some(vec![live_tab("thread-1", "signed-in", Some(true))]),
    ));
    broker.focus_host(focus(
        "other",
        &other.connection_id,
        true,
        Some(vec![
            live_tab("thread-1", "signed-in", Some(false)),
            live_tab("another-thread", "different-tab", Some(true)),
        ]),
    ));
    assert_eq!(broker.invoke(input("evaluate", json!({}))).await.result.unwrap(), json!("owner"));
    let explicit = |provider_session_id: &str, operation: &str, tab_id: &str| {
        let mut input = PreviewAutomationInvokeInput::new(session(provider_session_id), operation, json!({}));
        input.tab_id = Some(tab_id.into());
        input
    };
    assert_eq!(
        broker.invoke(explicit("explicit-owner", "snapshot", "signed-in")).await.result.unwrap(),
        json!("owner")
    );
    assert_eq!(
        broker.invoke(explicit("other-tab", "evaluate", "different-tab")).await.result.unwrap(),
        json!("other")
    );
    broker.focus_host(focus("owner", &owner.connection_id, false, Some(Vec::new())));
    assert_eq!(broker.invoke(input("evaluate", json!({}))).await.result.unwrap(), json!("owner"));
    let after = PreviewAutomationInvokeInput::new(session("after-tab-closed"), "evaluate", json!({}));
    assert_eq!(broker.invoke(after).await.result.unwrap(), json!("other"));
}

fn host_with(client_id: &str, operations: &[&str]) -> PreviewAutomationHost {
    PreviewAutomationHost {
        supported_operations: Some(operations.iter().map(|op| (*op).to_owned()).collect()),
        ..host(client_id)
    }
}

#[tokio::test]
async fn prefers_a_focused_host_over_unrelated_extra_capabilities_for_a_new_session() {
    let broker = PreviewAutomationBroker::new();
    let focused = serve(&broker, host_with("focused", &["status"]), always(json!("focused"))).await;
    let _background = serve(&broker, host_with("background", &["status", "resize"]), always(json!("background"))).await;
    broker.focus_host(focus("focused", &focused.connection_id, true, None));
    assert_eq!(broker.invoke(input("status", json!({}))).await.result.unwrap(), json!("focused"));
}

#[tokio::test]
async fn does_not_route_new_operations_to_legacy_hosts_that_did_not_advertise_support() {
    let broker = PreviewAutomationBroker::new();
    let _legacy = serve(&broker, host("client-1"), Arc::new(|_| None)).await;
    let error = broker.invoke(input("resize", json!({"mode": "fill"}))).await.result.unwrap_err();
    assert_eq!(tag(&error), "PreviewAutomationNoAvailableHostError");
    assert_eq!(error.fields["operation"], "resize");
}

#[tokio::test]
async fn routes_resize_to_a_capable_host_instead_of_a_newer_legacy_connection() {
    let broker = PreviewAutomationBroker::new();
    let _capable = serve(&broker, host_with("client-capable", &["resize"]), always(json!("capable"))).await;
    let _legacy = serve(&broker, host("client-legacy"), always(json!("legacy"))).await;
    assert_eq!(broker.invoke(input("resize", json!({"mode": "fill"}))).await.result.unwrap(), json!("capable"));
}

#[tokio::test]
async fn does_not_move_a_live_legacy_assignment_to_another_runtime_for_resize() {
    let broker = PreviewAutomationBroker::new();
    let _legacy = serve(&broker, host("client-legacy"), always(json!("legacy"))).await;
    assert_eq!(broker.invoke(input("status", json!({}))).await.result.unwrap(), json!("legacy"));
    let _capable = serve(&broker, host_with("client-capable", &["resize"]), always(json!("capable"))).await;
    assert_eq!(
        tag(&broker.invoke(input("resize", json!({"mode": "fill"}))).await.result.unwrap_err()),
        "PreviewAutomationNoAvailableHostError"
    );
    assert_eq!(broker.invoke(input("status", json!({}))).await.result.unwrap(), json!("legacy"));
}

#[tokio::test]
async fn ignores_stale_focus_updates_for_a_different_environment() {
    let broker = PreviewAutomationBroker::new();
    let first = serve(&broker, host("client-first"), always(json!("first"))).await;
    let _second = serve(&broker, host("client-second"), always(json!("second"))).await;
    broker.focus_host(PreviewAutomationHostFocus {
        environment_id: "environment-stale".into(),
        ..focus("client-first", &first.connection_id, true, None)
    });
    assert_eq!(broker.invoke(input("status", json!({}))).await.result.unwrap(), json!("second"));
}

#[tokio::test]
async fn fails_over_a_pinned_provider_session_only_after_its_host_disconnects() {
    let broker = PreviewAutomationBroker::new();
    let first = serve(
        &broker,
        host("client-first"),
        Arc::new(|request: &Value| {
            if request["operation"] == "open" {
                ok(json!({"host": "first", "tabId": "tab-on-first-host"}))
            } else {
                ok(json!("first"))
            }
        }),
    )
    .await;
    let second = serve(&broker, host("client-second"), always(json!("second"))).await;
    broker.focus_host(focus(
        "client-first",
        &first.connection_id,
        true,
        Some(vec![live_tab("thread-1", "tab-on-first-host", None)]),
    ));
    assert_eq!(
        broker.invoke(input("open", json!({}))).await.result.unwrap(),
        json!({"host": "first", "tabId": "tab-on-first-host"})
    );
    first.task.abort();
    let _ = first.task.await;
    assert_eq!(broker.invoke(input("status", json!({}))).await.result.unwrap(), json!("second"));
    assert!(second.requests.lock().unwrap()[0].get("tabId").is_none());
}

#[tokio::test]
async fn lets_the_browser_host_resolve_an_active_tab_locally() {
    let broker = PreviewAutomationBroker::new();
    let served = serve(&broker, host("client-1"), Arc::new(|_| Some(json!({"ok": true})))).await;
    let result = broker.invoke(input("click", json!({"x": 10, "y": 10}))).await.result.unwrap();
    assert_eq!(result, Value::Null);
    assert!(served.requests.lock().unwrap()[0].get("tabId").is_none());
}

#[tokio::test]
async fn keeps_a_replacement_stream_authoritative_when_the_old_stream_finalizes() {
    let broker = PreviewAutomationBroker::new();
    let first = serve(&broker, host("client-1"), Arc::new(|_| None)).await;
    let replacement = serve(&broker, host("client-1"), always(json!("replacement"))).await;
    assert_ne!(replacement.connection_id, first.connection_id);
    // The replaced stream ends; dropping it must not unregister its successor.
    let _ = first.task.await;
    assert_eq!(broker.invoke(input("status", json!({}))).await.result.unwrap(), json!("replacement"));
}

#[tokio::test]
async fn does_not_carry_a_tab_id_across_a_replacement_automation_stream() {
    let broker = PreviewAutomationBroker::new();
    let _first = serve(
        &broker,
        host("client-1"),
        Arc::new(|request: &Value| {
            if request["operation"] == "open" {
                ok(json!({"host": "first", "tabId": "tab-first-webcontents"}))
            } else {
                ok(json!({"host": "first"}))
            }
        }),
    )
    .await;
    assert_eq!(
        broker.invoke(input("open", json!({}))).await.result.unwrap(),
        json!({"host": "first", "tabId": "tab-first-webcontents"})
    );
    let replacement = serve(&broker, host("client-1"), always(json!("replacement"))).await;
    assert_eq!(broker.invoke(input("status", json!({}))).await.result.unwrap(), json!("replacement"));
    assert!(replacement.requests.lock().unwrap().last().unwrap().get("tabId").is_none());
}

#[tokio::test]
async fn fails_requests_assigned_to_the_stream_that_is_replaced() {
    let broker = PreviewAutomationBroker::new();
    let first = serve(&broker, host("client-1"), Arc::new(|_| None)).await;
    let pending = tokio::spawn({
        let broker = broker.clone();
        async move { broker.invoke(input("status", json!({}))).await }
    });
    while first.requests.lock().unwrap().is_empty() {
        tokio::task::yield_now().await;
    }
    let _replacement = serve(&broker, host("client-1"), Arc::new(|_| None)).await;
    let error = pending.await.unwrap().result.unwrap_err();
    assert_eq!(tag(&error), "PreviewAutomationClientDisconnectedError");
    assert_eq!(error.fields["operation"], "status");
    assert_eq!(error.fields["clientId"], "client-1");
    assert_eq!(error.fields["requestId"], "preview-0");
    assert_eq!(error.fields["timeoutMs"], 15_000);
    assert_eq!(error.message, "Preview automation client client-1 disconnected during status.");
}

#[tokio::test]
async fn accepts_responses_only_from_the_host_that_received_the_request() {
    let broker = PreviewAutomationBroker::new();
    let mut events = broker.connect(host("client-1"));
    events.next().await.unwrap();
    let invoke = tokio::spawn({
        let broker = broker.clone();
        async move { broker.invoke(input("status", json!({}))).await }
    });
    let event = events.next().await.unwrap();
    let connection_id = event["connectionId"].as_str().unwrap().to_owned();
    let request_id = event["request"]["requestId"].as_str().unwrap().to_owned();
    let respond = |client_id: &str, connection_id: &str, result: &str| {
        broker.respond(PreviewAutomationResponse {
            client_id: client_id.into(),
            connection_id: connection_id.into(),
            request_id: request_id.clone(),
            ok: true,
            result: Some(json!(result)),
            error: None,
        })
    };
    respond("client-foreign", &connection_id, "foreign");
    respond("client-1", "connection-stale", "stale");
    respond("client-1", &connection_id, "owner");
    assert_eq!(invoke.await.unwrap().result.unwrap(), json!("owner"));
}

#[tokio::test(start_paused = true)]
async fn evicts_an_unanswered_host_and_lets_later_calls_use_a_healthy_runtime() {
    let broker = PreviewAutomationBroker::new();
    let frozen = serve(
        &broker,
        host("client-1"),
        Arc::new(|request: &Value| (request["operation"] == "open").then(|| json!({"ok": true, "result": {"tabId": "tab-on-frozen-host"}}))),
    )
    .await;
    broker.invoke(input("open", json!({}))).await.result.unwrap();
    let healthy = serve(&broker, host("healthy"), always(json!("healthy"))).await;

    let timed_out = tokio::spawn({
        let broker = broker.clone();
        let mut request = input("snapshot", json!({}));
        request.timeout_ms = Some(1_000);
        async move { broker.invoke(request).await }
    });
    while frozen.requests.lock().unwrap().len() < 2 {
        tokio::task::yield_now().await;
    }
    let late_request_id = frozen.requests.lock().unwrap()[1]["requestId"].as_str().unwrap().to_owned();
    let other = tokio::spawn({
        let broker = broker.clone();
        let mut request = input("evaluate", json!({}));
        request.timeout_ms = Some(10_000);
        async move { broker.invoke(request).await }
    });
    while frozen.requests.lock().unwrap().len() < 3 {
        tokio::task::yield_now().await;
    }
    assert_eq!(tag(&timed_out.await.unwrap().result.unwrap_err()), "PreviewAutomationTimeoutError");
    assert_eq!(tag(&other.await.unwrap().result.unwrap_err()), "PreviewAutomationClientDisconnectedError");
    // The evicted stream completes.
    frozen.task.await.unwrap();

    // Late traffic from the evicted connection cannot restore its assignment.
    broker.respond(PreviewAutomationResponse {
        client_id: "client-1".into(),
        connection_id: frozen.connection_id.clone(),
        request_id: late_request_id,
        ok: true,
        result: Some(json!({"tabId": "tab-on-frozen-host"})),
        error: None,
    });
    broker.focus_host(focus("client-1", &frozen.connection_id, true, None));
    assert_eq!(broker.invoke(input("status", json!({}))).await.result.unwrap(), json!("healthy"));
    let healthy_requests = healthy.requests.lock().unwrap().clone();
    assert_eq!(healthy_requests.len(), 1);
    assert!(healthy_requests[0].get("tabId").is_none());
}

#[tokio::test(start_paused = true)]
async fn discards_buffered_actions_before_completing_an_evicted_host_stream() {
    let broker = PreviewAutomationBroker::new();
    let mut events = broker.connect(host("client-1"));
    events.next().await.unwrap();
    let timed_out = tokio::spawn({
        let broker = broker.clone();
        let mut request = input("snapshot", json!({}));
        request.timeout_ms = Some(1_000);
        async move { broker.invoke(request).await }
    });
    let snapshot = events.next().await.unwrap();
    assert_eq!(snapshot["request"]["operation"], "snapshot");
    // The consumer is busy: the click is buffered behind it.
    let buffered = tokio::spawn({
        let broker = broker.clone();
        let mut request = input("click", json!({}));
        request.timeout_ms = Some(10_000);
        async move { broker.invoke(request).await }
    });
    assert_eq!(tag(&timed_out.await.unwrap().result.unwrap_err()), "PreviewAutomationTimeoutError");
    assert_eq!(tag(&buffered.await.unwrap().result.unwrap_err()), "PreviewAutomationClientDisconnectedError");
    // Nothing else is delivered: the stream ends.
    assert_eq!(events.next().await, None);
}

#[tokio::test]
async fn keeps_a_host_that_responds_with_an_operation_timeout() {
    let broker = PreviewAutomationBroker::new();
    let _host = serve(
        &broker,
        host("client-1"),
        Arc::new(|request: &Value| {
            if request["operation"] == "waitFor" {
                Some(json!({"ok": false, "error": {"_tag": "PreviewAutomationTimeoutError", "message": "Selector timed out"}}))
            } else {
                ok(json!("responsive"))
            }
        }),
    )
    .await;
    assert_eq!(
        tag(&broker.invoke(input("waitFor", json!({}))).await.result.unwrap_err()),
        "PreviewAutomationTimeoutError"
    );
    assert_eq!(broker.invoke(input("status", json!({}))).await.result.unwrap(), json!("responsive"));
}
