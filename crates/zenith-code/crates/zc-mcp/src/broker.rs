//! `PreviewAutomationBroker.ts`: routes preview tool calls to a desktop browser host.
//!
//! A host (the desktop app's renderer) keeps a `previewAutomation.connect` stream open; the
//! broker pushes `{type:"request", connectionId, request}` events down it and the host answers
//! with `previewAutomation.respond`. One registration per `clientId`: a new stream replaces the
//! old one, whose pending requests fail as disconnected. A provider session is pinned to the
//! host that served its first call (a lease that lives exactly as long as that connection), so
//! a multi-step interaction never jumps between independent browser states; a live lease that
//! cannot serve an operation fails rather than moving. An unanswered request evicts its host
//! (its stream completes, buffered commands are discarded) so a responsive desktop can
//! re-register.

use std::collections::{HashMap, HashSet, VecDeque};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use futures::Stream;
use serde_json::{json, Value};
use tokio::sync::{oneshot, Notify};
use zc_ports::TaggedError;

use crate::errors::{self, RequestErrorContext};
use crate::scope::McpInvocationScope;

/// `PREVIEW_AUTOMATION_V1_OPERATIONS`: what a host that advertises nothing supports.
pub const PREVIEW_AUTOMATION_V1_OPERATIONS: [&str; 12] = [
    "status",
    "open",
    "navigate",
    "snapshot",
    "click",
    "type",
    "press",
    "scroll",
    "evaluate",
    "waitFor",
    "recordingStart",
    "recordingStop",
];

/// `PREVIEW_AUTOMATION_OPERATIONS`.
pub const PREVIEW_AUTOMATION_OPERATIONS: [&str; 14] = [
    "status",
    "open",
    "navigate",
    "snapshot",
    "click",
    "type",
    "press",
    "scroll",
    "evaluate",
    "waitFor",
    "recordingStart",
    "recordingStop",
    "resize",
    "setColorScheme",
];

/// The default wait for a host's answer.
pub const DEFAULT_TIMEOUT_MS: u64 = 15_000;

/// `PreviewAutomationHost`: a `previewAutomation.connect` payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewAutomationHost {
    pub client_id: String,
    pub environment_id: String,
    /// `None`: the V1 operation set.
    pub supported_operations: Option<Vec<String>>,
}

/// A thread's tab a host reports as live (`PreviewAutomationHostFocus.liveTabs`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveTab {
    pub thread_id: String,
    pub tab_id: String,
    pub visible: Option<bool>,
}

/// `PreviewAutomationHostFocus`: a `previewAutomation.focusHost` payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewAutomationHostFocus {
    pub client_id: String,
    pub environment_id: String,
    pub connection_id: String,
    pub focused: bool,
    pub live_tabs: Option<Vec<LiveTab>>,
}

/// `PreviewAutomationResponse`: a `previewAutomation.respond` payload.
#[derive(Debug, Clone, PartialEq)]
pub struct PreviewAutomationResponse {
    pub client_id: String,
    pub connection_id: String,
    pub request_id: String,
    pub ok: bool,
    /// `None` when the host sent no `result`.
    pub result: Option<Value>,
    /// `{_tag, message, detail?}`.
    pub error: Option<Value>,
}

/// `PreviewAutomationInvokeInput`.
#[derive(Debug, Clone)]
pub struct PreviewAutomationInvokeInput {
    pub scope: McpInvocationScope,
    pub operation: String,
    pub input: Value,
    pub tab_id: Option<String>,
    pub timeout_ms: Option<u64>,
    /// Background metadata reads must not change the agent's current tab.
    pub update_current_tab: bool,
}

impl PreviewAutomationInvokeInput {
    pub fn new(scope: McpInvocationScope, operation: impl Into<String>, input: Value) -> Self {
        Self {
            scope,
            operation: operation.into(),
            input,
            tab_id: None,
            timeout_ms: None,
            update_current_tab: true,
        }
    }
}

/// What [`PreviewAutomationBroker::invoke`] returns.
#[derive(Debug)]
pub struct InvokeOutcome {
    /// The host's result (`Null` when it sent none), or the classified error.
    pub result: Result<Value, TaggedError>,
    /// `onTargetTab`: set once the request was routed, to the tab it targeted (explicit or the
    /// session's current one).
    pub routed_tab: Option<Option<String>>,
}

#[derive(Default)]
struct QueueState {
    items: VecDeque<Value>,
    closed: bool,
}

/// One host stream's command queue.
#[derive(Default)]
struct ConnectionQueue {
    state: Mutex<QueueState>,
    notify: Notify,
}

impl ConnectionQueue {
    fn offer(&self, event: Value) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return false;
        }
        state.items.push_back(event);
        drop(state);
        self.notify.notify_one();
        true
    }

    /// Discards what is buffered and ends the stream (`Queue.clear` + `Queue.end` on eviction,
    /// `Queue.shutdown` on replacement: either way the host sees its stream finish).
    fn close(&self) {
        let mut state = self.state.lock().unwrap();
        state.items.clear();
        state.closed = true;
        drop(state);
        self.notify.notify_one();
    }
}

struct Connection {
    client_id: String,
    connection_id: String,
    environment_id: String,
    supported_operations: HashSet<String>,
    focused: bool,
    live_tabs: Vec<LiveTab>,
    focus_order: u64,
    queue: Arc<ConnectionQueue>,
}

impl Connection {
    fn supports(&self, operation: &str) -> bool {
        self.supported_operations.contains(operation)
    }
}

/// A lease pinning one provider session to one connection.
#[derive(Clone)]
struct Assignment {
    client_id: String,
    connection_id: String,
    tab_id: Option<String>,
    tab_sequence: Option<u64>,
}

struct Pending {
    connection_id: String,
    sender: oneshot::Sender<Result<Value, TaggedError>>,
    context: RequestErrorContext,
}

#[derive(Default)]
struct BrokerState {
    clients: HashMap<String, Connection>,
    assignments: HashMap<String, Assignment>,
    pending: HashMap<String, Pending>,
    request_sequence: u64,
    focus_sequence: u64,
}

impl BrokerState {
    fn is_current(&self, client_id: &str, connection_id: &str) -> bool {
        self.clients.get(client_id).is_some_and(|connection| connection.connection_id == connection_id)
    }

    /// `removeConnectionFromState`: the connection, its leases and its pending requests.
    fn remove_connection(&mut self, client_id: &str, connection_id: &str) -> Vec<Pending> {
        if self.is_current(client_id, connection_id) {
            self.clients.remove(client_id);
        }
        self.assignments.retain(|_, assignment| assignment.connection_id != connection_id);
        let ids: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, pending)| pending.connection_id == connection_id)
            .map(|(id, _)| id.clone())
            .collect();
        ids.into_iter().filter_map(|id| self.pending.remove(&id)).collect()
    }
}

#[derive(Default)]
struct Inner {
    state: Mutex<BrokerState>,
}

/// The broker. Cheap to clone.
#[derive(Clone, Default)]
pub struct PreviewAutomationBroker {
    inner: Arc<Inner>,
}

/// `closeConnection`: ends the stream and fails its pending requests as disconnected.
fn close_connection(queue: &ConnectionQueue, disconnected: Vec<Pending>) {
    queue.close();
    for pending in disconnected {
        let error = errors::client_disconnected(&pending.context);
        let _ = pending.sender.send(Err(error));
    }
}

fn host_assignment_key(scope: &McpInvocationScope) -> String {
    format!("{}\u{0}{}", scope.environment_id, scope.provider_session_id)
}

/// `selectorDiagnosticsFromInput`.
fn selector_diagnostics(input: &Value) -> (Option<String>, Option<u64>) {
    for kind in ["locator", "selector"] {
        if let Some(text) = input.get(kind).and_then(Value::as_str) {
            return (Some(kind.to_owned()), Some(text.encode_utf16().count() as u64));
        }
    }
    (None, None)
}

/// `isPreviewTabId` on the type side: a non-empty string of at most 128 UTF-16 units.
fn is_preview_tab_id(value: &str) -> bool {
    let length = value.encode_utf16().count();
    length > 0 && length <= 128
}

/// `readResultTabId`: `Some(Some(id))` for a tab id, `Some(None)` for `null`, `None` when the
/// result names no tab.
fn read_result_tab_id(result: &Value) -> Option<Option<String>> {
    let object = result.as_object()?;
    match object.get("tabId")? {
        Value::Null => Some(None),
        Value::String(tab_id) if is_preview_tab_id(tab_id) => Some(Some(tab_id.clone())),
        _ => None,
    }
}

/// The `previewAutomation.connect` stream: events until the connection is replaced, evicted
/// or the stream is dropped (which releases the registration). Like the TypeScript
/// `acquireRelease`, the host registers when the stream is first polled.
pub struct HostStream {
    broker: PreviewAutomationBroker,
    host: Option<PreviewAutomationHost>,
    events: Option<Pin<Box<dyn Stream<Item = Value> + Send>>>,
    release: Option<(String, String)>,
}

fn queue_events(queue: Arc<ConnectionQueue>) -> Pin<Box<dyn Stream<Item = Value> + Send>> {
    Box::pin(futures::stream::unfold(queue, |queue| async move {
        loop {
            let notified = queue.notify.notified();
            let next = {
                let mut state = queue.state.lock().unwrap();
                match state.items.pop_front() {
                    Some(item) => Some(Some(item)),
                    None if state.closed => Some(None),
                    None => None,
                }
            };
            match next {
                Some(item) => {
                    drop(notified);
                    return item.map(|item| (item, queue));
                }
                None => notified.await,
            }
        }
    }))
}

impl Stream for HostStream {
    type Item = Value;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Value>> {
        if let Some(host) = self.host.take() {
            let client_id = host.client_id.clone();
            let (connection_id, queue) = self.broker.register(host);
            self.events = Some(queue_events(queue));
            self.release = Some((client_id, connection_id));
        }
        match self.events.as_mut() {
            Some(events) => events.as_mut().poll_next(cx),
            None => Poll::Ready(None),
        }
    }
}

impl Drop for HostStream {
    fn drop(&mut self) {
        if let Some((client_id, connection_id)) = self.release.take() {
            self.broker.disconnect(&client_id, &connection_id);
        }
    }
}

impl PreviewAutomationBroker {
    pub fn new() -> Self {
        Self::default()
    }

    /// `disconnect`: drops a still-current registration (a retired one was already closed by
    /// its replacement or eviction).
    fn disconnect(&self, client_id: &str, connection_id: &str) {
        let removed = {
            let mut state = self.inner.state.lock().unwrap();
            if !state.is_current(client_id, connection_id) {
                return;
            }
            let queue = state.clients.get(client_id).map(|connection| connection.queue.clone());
            let disconnected = state.remove_connection(client_id, connection_id);
            queue.map(|queue| (queue, disconnected))
        };
        if let Some((queue, disconnected)) = removed {
            close_connection(&queue, disconnected);
        }
    }

    /// `connect`: the host's event stream. Polling it registers `host` (replacing its
    /// previous stream); it starts with `{type:"connected", connectionId}`.
    pub fn connect(&self, host: PreviewAutomationHost) -> HostStream {
        HostStream {
            broker: self.clone(),
            host: Some(host),
            events: None,
            release: None,
        }
    }

    /// `acquireConnection`: registers a new connection for the host.
    fn register(&self, host: PreviewAutomationHost) -> (String, Arc<ConnectionQueue>) {
        let connection_id = zc_core::ids::uuid_v4();
        let queue = Arc::new(ConnectionQueue::default());
        queue.offer(json!({"type": "connected", "connectionId": connection_id}));
        let supported_operations: HashSet<String> = match &host.supported_operations {
            Some(operations) => operations.iter().cloned().collect(),
            None => PREVIEW_AUTOMATION_V1_OPERATIONS.iter().map(|op| (*op).to_owned()).collect(),
        };
        let previous = {
            let mut state = self.inner.state.lock().unwrap();
            let previous = state
                .clients
                .get(&host.client_id)
                .map(|connection| (connection.connection_id.clone(), connection.queue.clone()));
            let disconnected = previous
                .as_ref()
                .map(|(previous_id, _)| state.remove_connection(&host.client_id, previous_id))
                .unwrap_or_default();
            state.focus_sequence += 1;
            let focus_order = state.focus_sequence;
            state.clients.insert(
                host.client_id.clone(),
                Connection {
                    client_id: host.client_id.clone(),
                    connection_id: connection_id.clone(),
                    environment_id: host.environment_id.clone(),
                    supported_operations,
                    focused: false,
                    live_tabs: Vec::new(),
                    focus_order,
                    queue: queue.clone(),
                },
            );
            previous.map(|(_, queue)| (queue, disconnected))
        };
        if let Some((previous_queue, disconnected)) = previous {
            // Replaced registrations must not reconnect and displace their successor.
            close_connection(&previous_queue, disconnected);
        }
        (connection_id, queue)
    }

    /// `focusHost`: focus and live tabs of the current connection; stale updates are ignored.
    pub fn focus_host(&self, focus: PreviewAutomationHostFocus) {
        let mut state = self.inner.state.lock().unwrap();
        let focus_sequence = if focus.focused { state.focus_sequence + 1 } else { state.focus_sequence };
        let Some(connection) = state.clients.get_mut(&focus.client_id) else {
            return;
        };
        if connection.environment_id != focus.environment_id || connection.connection_id != focus.connection_id {
            return;
        }
        connection.focused = focus.focused;
        if let Some(live_tabs) = focus.live_tabs {
            connection.live_tabs = live_tabs;
        }
        if focus.focused {
            connection.focus_order = focus_sequence;
        }
        state.focus_sequence = focus_sequence;
    }

    /// `respond`: settles the pending request, if this host's connection received it.
    pub fn respond(&self, response: PreviewAutomationResponse) {
        let pending = {
            let mut state = self.inner.state.lock().unwrap();
            match state.pending.get(&response.request_id) {
                Some(entry) if entry.context.client_id == response.client_id && entry.context.connection_id == response.connection_id => {
                    state.pending.remove(&response.request_id)
                }
                _ => None,
            }
        };
        let Some(pending) = pending else {
            return;
        };
        let outcome = if response.ok {
            Ok(response.result.unwrap_or(Value::Null))
        } else {
            Err(match &response.error {
                Some(error) => errors::classify_response_error(&pending.context, error),
                None => errors::malformed_response(&pending.context),
            })
        };
        let _ = pending.sender.send(outcome);
    }

    /// The connected hosts, as `(clientId, connectionId)`.
    pub fn hosts(&self) -> Vec<(String, String)> {
        let state = self.inner.state.lock().unwrap();
        let mut hosts: Vec<(String, String)> = state.clients.values().map(|c| (c.client_id.clone(), c.connection_id.clone())).collect();
        hosts.sort();
        hosts
    }

    /// `invoke`: routes one operation to a host and waits for its answer.
    pub async fn invoke(&self, input: PreviewAutomationInvokeInput) -> InvokeOutcome {
        let timeout_ms = input.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        let (sender, mut receiver) = oneshot::channel();
        let assignment_key = host_assignment_key(&input.scope);
        let route = {
            let mut state = self.inner.state.lock().unwrap();
            let state = &mut *state;
            let clients = &state.clients;
            state.assignments.retain(|_, assignment| {
                clients
                    .get(&assignment.client_id)
                    .is_some_and(|connection| connection.connection_id == assignment.connection_id)
            });
            let assigned = state.assignments.get(&assignment_key).cloned();
            let assigned_connection = assigned.as_ref().and_then(|assignment| state.clients.get(&assignment.client_id));
            let has_live_assignment = assigned_connection.is_some_and(|connection| connection.environment_id == input.scope.environment_id);
            let owns_target_tab = |host: &Connection, visible_only: bool| {
                host.live_tabs.iter().any(|tab| {
                    tab.thread_id == input.scope.thread_id
                        && (!visible_only || tab.visible == Some(true))
                        && input.tab_id.as_ref().is_none_or(|tab_id| &tab.tab_id == tab_id)
                })
            };
            let chosen: Option<&Connection> = if has_live_assignment {
                assigned_connection.filter(|connection| connection.supports(&input.operation))
            } else {
                let mut candidates: Vec<&Connection> = state
                    .clients
                    .values()
                    .filter(|host| host.environment_id == input.scope.environment_id && host.supports(&input.operation))
                    .collect();
                candidates.sort_by(|left, right| {
                    owns_target_tab(right, true)
                        .cmp(&owns_target_tab(left, true))
                        .then(owns_target_tab(right, false).cmp(&owns_target_tab(left, false)))
                        .then(right.focused.cmp(&left.focused))
                        .then(right.focus_order.cmp(&left.focus_order))
                });
                candidates.into_iter().next()
            };
            match chosen {
                None => {
                    if !has_live_assignment {
                        state.assignments.remove(&assignment_key);
                    }
                    None
                }
                Some(connection) => {
                    let client_id = connection.client_id.clone();
                    let connection_id = connection.connection_id.clone();
                    let queue = connection.queue.clone();
                    let can_reuse = assigned.as_ref().is_some_and(|assignment| assignment.connection_id == connection_id);
                    let reused = assigned.filter(|_| can_reuse);
                    state.assignments.insert(
                        assignment_key.clone(),
                        Assignment {
                            client_id: client_id.clone(),
                            connection_id: connection_id.clone(),
                            tab_id: reused.as_ref().and_then(|assignment| assignment.tab_id.clone()),
                            tab_sequence: reused.as_ref().and_then(|assignment| assignment.tab_sequence),
                        },
                    );
                    let request_sequence = state.request_sequence;
                    state.request_sequence += 1;
                    let request_id = format!("preview-{request_sequence}");
                    let tab_id = input.tab_id.clone().or_else(|| reused.and_then(|assignment| assignment.tab_id));
                    let (selector_kind, selector_length) = selector_diagnostics(&input.input);
                    let context = RequestErrorContext {
                        operation: input.operation.clone(),
                        environment_id: input.scope.environment_id.clone(),
                        thread_id: input.scope.thread_id.clone(),
                        provider_session_id: input.scope.provider_session_id.clone(),
                        provider_instance_id: input.scope.provider_instance_id.clone(),
                        client_id: client_id.clone(),
                        connection_id: connection_id.clone(),
                        request_id: request_id.clone(),
                        tab_id,
                        timeout_ms,
                        selector_kind,
                        selector_length,
                    };
                    state.pending.insert(
                        request_id.clone(),
                        Pending {
                            connection_id: connection_id.clone(),
                            sender,
                            context: context.clone(),
                        },
                    );
                    Some((client_id, connection_id, queue, request_id, context, request_sequence))
                }
            }
        };
        let Some((client_id, connection_id, queue, request_id, context, request_sequence)) = route else {
            return InvokeOutcome {
                result: Err(errors::no_available_host(&input.operation, &input.scope)),
                routed_tab: None,
            };
        };
        let routed_tab = Some(context.tab_id.clone());

        // Removes the pending entry however the wait ends.
        struct RemovePending<'a>(&'a Inner, String);
        impl Drop for RemovePending<'_> {
            fn drop(&mut self) {
                self.0.state.lock().unwrap().pending.remove(&self.1);
            }
        }
        let _remove = RemovePending(&self.inner, request_id.clone());

        let offered = {
            let state = self.inner.state.lock().unwrap();
            // A route can outlive its generation while another request evicts it.
            if !state.is_current(&client_id, &connection_id) || !state.pending.contains_key(&request_id) {
                false
            } else {
                let mut request = serde_json::Map::new();
                request.insert("requestId".into(), json!(request_id));
                request.insert("threadId".into(), json!(input.scope.thread_id));
                if let Some(tab_id) = &context.tab_id {
                    request.insert("tabId".into(), json!(tab_id));
                }
                request.insert("tabIdExplicit".into(), json!(input.tab_id.is_some()));
                request.insert("operation".into(), json!(input.operation));
                request.insert("input".into(), input.input.clone());
                request.insert("timeoutMs".into(), json!(timeout_ms));
                queue.offer(json!({"type": "request", "connectionId": connection_id, "request": request}))
            }
        };
        let result = if !offered {
            match receiver.try_recv() {
                Ok(outcome) => outcome,
                Err(_) => Err(errors::request_queue_closed(&context)),
            }
        } else {
            match tokio::time::timeout(Duration::from_millis(timeout_ms), &mut receiver).await {
                Ok(Ok(outcome)) => outcome,
                Ok(Err(_)) => Err(errors::client_disconnected(&context)),
                Err(_) => {
                    // An unanswered request invalidates this connection. Do not replay
                    // actions: the client may have applied them before becoming unreachable.
                    self.disconnect(&client_id, &connection_id);
                    Err(errors::timeout(&context, None))
                }
            }
        };
        drop(_remove);
        let value = match result {
            Ok(value) => value,
            Err(error) => {
                return InvokeOutcome {
                    result: Err(error),
                    routed_tab,
                }
            }
        };
        if input.update_current_tab {
            let result_tab = read_result_tab_id(&value).or_else(|| input.tab_id.clone().map(Some));
            if let Some(result_tab) = result_tab {
                let mut state = self.inner.state.lock().unwrap();
                if let Some(assignment) = state.assignments.get_mut(&assignment_key) {
                    let stale = assignment.tab_sequence.is_some_and(|sequence| sequence > request_sequence);
                    if assignment.connection_id == connection_id && !stale {
                        assignment.tab_id = result_tab;
                        assignment.tab_sequence = Some(request_sequence);
                    }
                }
            }
        }
        InvokeOutcome { result: Ok(value), routed_tab }
    }
}
