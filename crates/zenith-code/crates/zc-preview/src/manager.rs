//! `preview/Manager.ts`: the in-memory preview tabs.
//!
//! Sessions are keyed by `(threadId, tabId)`; a thread hosts several tabs and `open` always
//! creates a new one (the renderer owns tab lifecycle). One monotonic `revision` orders list
//! results and events; `serverEpoch` names this process so a client can tell a restart from a
//! stale answer. Events are published under the state lock, so subscribers see them in the order
//! of the state changes; a closed subscriber never fails a publisher.

use std::sync::{Arc, Mutex};

use serde_json::{json, Map, Value};
use tokio::sync::mpsc;

use crate::url::{new_preview_tab_id, normalize_preview_url, UrlNormalizationError};

/// `PreviewError`: `PreviewSessionLookupError | PreviewInvalidUrlError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewError {
    SessionLookup { thread_id: String, tab_id: String },
    InvalidUrl(UrlNormalizationError),
}

impl PreviewError {
    pub fn tag(&self) -> &'static str {
        match self {
            Self::SessionLookup { .. } => "PreviewSessionLookupError",
            Self::InvalidUrl(_) => "PreviewInvalidUrlError",
        }
    }

    /// The error's message (built here, never from the URL).
    pub fn message(&self) -> String {
        match self {
            Self::SessionLookup { thread_id, tab_id } => format!("Unknown preview session: thread={thread_id}, tab={tab_id}"),
            Self::InvalidUrl(error) => error.message(),
        }
    }

    /// The encoded tagged error.
    pub fn encoded(&self) -> Value {
        match self {
            Self::SessionLookup { thread_id, tab_id } => json!({"_tag": self.tag(), "threadId": thread_id, "tabId": tab_id}),
            Self::InvalidUrl(error) => {
                let mut fields = Map::new();
                fields.insert("_tag".into(), json!(self.tag()));
                fields.insert("inputLength".into(), json!(error.input_length));
                fields.insert("reason".into(), json!(error.reason));
                if let Some(protocol) = &error.protocol {
                    fields.insert("protocol".into(), json!(protocol));
                }
                fields.insert("cause".into(), error.encoded());
                Value::Object(fields)
            }
        }
    }
}

/// `FILL_PREVIEW_VIEWPORT`.
pub fn fill_viewport() -> Value {
    json!({"_tag": "fill"})
}

/// `PreviewOpenInput`.
#[derive(Debug, Clone, Default)]
pub struct OpenInput {
    pub thread_id: String,
    pub url: Option<String>,
    /// `PreviewViewportSetting`; fill when omitted.
    pub viewport: Option<Value>,
    pub profile_id: Option<String>,
}

/// `PreviewNavigateInput`.
#[derive(Debug, Clone, Default)]
pub struct NavigateInput {
    pub thread_id: String,
    pub tab_id: String,
    pub url: String,
    pub resolved_title: Option<String>,
}

/// `PreviewReportStatusInput`.
#[derive(Debug, Clone)]
pub struct ReportStatusInput {
    pub thread_id: String,
    pub tab_id: String,
    /// `PreviewNavStatus`.
    pub nav_status: Value,
    pub can_go_back: bool,
    pub can_go_forward: bool,
}

struct Session {
    thread_id: String,
    tab_id: String,
    snapshot: Value,
}

#[derive(Default)]
struct State {
    /// In insertion order (a `Map` in TS).
    sessions: Vec<Session>,
    revision: u64,
    subscribers: Vec<mpsc::UnboundedSender<Value>>,
}

impl State {
    fn index(&self, thread_id: &str, tab_id: &str) -> Option<usize> {
        self.sessions
            .iter()
            .position(|session| session.thread_id == thread_id && session.tab_id == tab_id)
    }

    fn publish(&mut self, event: Value) {
        self.subscribers.retain(|subscriber| subscriber.send(event.clone()).is_ok());
    }
}

struct Inner {
    server_epoch: String,
    state: Mutex<State>,
}

/// The preview manager. Cheap to clone.
#[derive(Clone)]
pub struct PreviewManager {
    inner: Arc<Inner>,
}

impl Default for PreviewManager {
    fn default() -> Self {
        Self::new()
    }
}

/// A tab's navigation state.
struct Nav {
    status: Value,
    can_go_back: bool,
    can_go_forward: bool,
}

/// A `PreviewSessionSnapshot` in schema field order.
fn snapshot(thread_id: &str, tab_id: &str, nav: Nav, viewport: Value, profile_id: Option<&Value>, updated_at: &str) -> Value {
    let mut fields = Map::new();
    fields.insert("threadId".into(), json!(thread_id));
    fields.insert("tabId".into(), json!(tab_id));
    fields.insert("navStatus".into(), nav.status);
    fields.insert("canGoBack".into(), json!(nav.can_go_back));
    fields.insert("canGoForward".into(), json!(nav.can_go_forward));
    fields.insert("viewport".into(), viewport);
    if let Some(profile_id) = profile_id {
        fields.insert("profileId".into(), profile_id.clone());
    }
    fields.insert("updatedAt".into(), json!(updated_at));
    Value::Object(fields)
}

/// A `PreviewEvent`: the base fields, the type, then the type's own fields.
fn event(kind: &str, thread_id: &str, tab_id: &str, created_at: &str, server_epoch: &str, revision: u64, extra: Map<String, Value>) -> Value {
    let mut fields = Map::new();
    fields.insert("threadId".into(), json!(thread_id));
    fields.insert("tabId".into(), json!(tab_id));
    fields.insert("createdAt".into(), json!(created_at));
    fields.insert("serverEpoch".into(), json!(server_epoch));
    fields.insert("revision".into(), json!(revision));
    fields.insert("type".into(), json!(kind));
    fields.extend(extra);
    Value::Object(fields)
}

fn with_snapshot(snapshot: &Value) -> Map<String, Value> {
    let mut extra = Map::new();
    extra.insert("snapshot".into(), snapshot.clone());
    extra
}

fn field<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.get(key).filter(|value| !value.is_null())
}

impl PreviewManager {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                server_epoch: zc_core::ids::uuid_v4(),
                state: Mutex::new(State::default()),
            }),
        }
    }

    pub fn server_epoch(&self) -> &str {
        &self.inner.server_epoch
    }

    /// A subscription to every later event (`subscribeEvents`).
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<Value> {
        let (sender, receiver) = mpsc::unbounded_channel();
        self.inner.state.lock().unwrap().subscribers.push(sender);
        receiver
    }

    /// `open`: a new tab, loading its URL or idle.
    pub fn open(&self, input: OpenInput) -> Result<Value, PreviewError> {
        let tab_id = new_preview_tab_id();
        let updated_at = zc_core::time::now_iso();
        // Clients with a configured default send the viewport so the tab is born at its size.
        let viewport = input.viewport.unwrap_or_else(fill_viewport);
        let profile_id = input.profile_id.map(Value::String);
        let nav_status = match &input.url {
            Some(url) => {
                let url = normalize_preview_url(url).map_err(PreviewError::InvalidUrl)?;
                json!({"_tag": "Loading", "url": url, "title": ""})
            }
            None => json!({"_tag": "Idle"}),
        };
        let nav = Nav {
            status: nav_status,
            can_go_back: false,
            can_go_forward: false,
        };
        let snapshot = snapshot(&input.thread_id, &tab_id, nav, viewport, profile_id.as_ref(), &updated_at);
        let mut state = self.inner.state.lock().unwrap();
        state.revision += 1;
        let revision = state.revision;
        state.sessions.push(Session {
            thread_id: input.thread_id.clone(),
            tab_id: tab_id.clone(),
            snapshot: snapshot.clone(),
        });
        let event = event(
            "opened",
            &input.thread_id,
            &tab_id,
            &updated_at,
            &self.inner.server_epoch,
            revision,
            with_snapshot(&snapshot),
        );
        state.publish(event);
        Ok(snapshot)
    }

    /// `mutateExistingSession`: replaces the session's snapshot and publishes its event (if
    /// any) under one lock.
    fn mutate(
        &self,
        thread_id: &str,
        tab_id: &str,
        mutator: impl FnOnce(&Session) -> (Value, Option<(&'static str, Map<String, Value>)>),
    ) -> Result<Value, PreviewError> {
        let mut state = self.inner.state.lock().unwrap();
        let Some(index) = state.index(thread_id, tab_id) else {
            return Err(PreviewError::SessionLookup {
                thread_id: thread_id.to_owned(),
                tab_id: tab_id.to_owned(),
            });
        };
        let (next, emit) = mutator(&state.sessions[index]);
        if let Some((kind, extra)) = emit {
            state.revision += 1;
            let revision = state.revision;
            let session = &state.sessions[index];
            let created_at = next.get("updatedAt").and_then(Value::as_str).unwrap_or("").to_owned();
            let event = event(
                kind,
                &session.thread_id,
                &session.tab_id,
                &created_at,
                &self.inner.server_epoch,
                revision,
                extra,
            );
            state.publish(event);
        }
        state.sessions[index].snapshot = next.clone();
        Ok(next)
    }

    /// `navigate`: the tab now shows `url` (normalized), titled `resolvedTitle` or as before.
    pub fn navigate(&self, input: NavigateInput) -> Result<Value, PreviewError> {
        let url = normalize_preview_url(&input.url).map_err(PreviewError::InvalidUrl)?;
        self.mutate(&input.thread_id, &input.tab_id, |session| {
            let updated_at = zc_core::time::now_iso();
            let current = &session.snapshot;
            let previous_title = if current.pointer("/navStatus/_tag") == Some(&json!("Idle")) {
                String::new()
            } else {
                current.pointer("/navStatus/title").and_then(Value::as_str).unwrap_or("").to_owned()
            };
            let title = input.resolved_title.clone().unwrap_or(previous_title);
            let next = snapshot(
                &session.thread_id,
                &session.tab_id,
                Nav {
                    status: json!({"_tag": "Success", "url": url, "title": title}),
                    can_go_back: current["canGoBack"].as_bool().unwrap_or(false),
                    can_go_forward: current["canGoForward"].as_bool().unwrap_or(false),
                },
                field(current, "viewport").cloned().unwrap_or_else(fill_viewport),
                field(current, "profileId"),
                &updated_at,
            );
            let extra = with_snapshot(&next);
            (next, Some(("navigated", extra)))
        })
    }

    /// `reportStatus`: the desktop's navigation state; a load failure is published as `failed`.
    pub fn report_status(&self, input: ReportStatusInput) -> Result<(), PreviewError> {
        self.mutate(&input.thread_id, &input.tab_id, |session| {
            let updated_at = zc_core::time::now_iso();
            let current = &session.snapshot;
            let next = snapshot(
                &session.thread_id,
                &session.tab_id,
                Nav {
                    status: input.nav_status.clone(),
                    can_go_back: input.can_go_back,
                    can_go_forward: input.can_go_forward,
                },
                field(current, "viewport").cloned().unwrap_or_else(fill_viewport),
                field(current, "profileId"),
                &updated_at,
            );
            let emit = if input.nav_status["_tag"] == "LoadFailed" {
                let mut extra = Map::new();
                for key in ["url", "title", "code", "description"] {
                    extra.insert(key.into(), input.nav_status.get(key).cloned().unwrap_or(Value::Null));
                }
                ("failed", extra)
            } else {
                ("navigated", with_snapshot(&next))
            };
            (next, Some(emit))
        })
        .map(|_| ())
    }

    /// `resize`: the tab's viewport setting.
    pub fn resize(&self, thread_id: &str, tab_id: &str, viewport: Value) -> Result<Value, PreviewError> {
        self.mutate(thread_id, tab_id, |session| {
            let mut next = session.snapshot.clone();
            next["viewport"] = viewport.clone();
            next["updatedAt"] = json!(zc_core::time::now_iso());
            let extra = with_snapshot(&next);
            (next, Some(("resized", extra)))
        })
    }

    /// `refresh`: checks the tab exists; the desktop reloads and reports back. No event.
    pub fn refresh(&self, thread_id: &str, tab_id: &str) -> Result<(), PreviewError> {
        self.mutate(thread_id, tab_id, |session| (session.snapshot.clone(), None)).map(|_| ())
    }

    /// `close`: one tab, or every tab of the thread; each closed tab gets its own revision.
    pub fn close(&self, thread_id: &str, tab_id: Option<&str>) {
        let created_at = zc_core::time::now_iso();
        let mut state = self.inner.state.lock().unwrap();
        let targets: Vec<usize> = state
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, session)| session.thread_id == thread_id && tab_id.is_none_or(|tab_id| session.tab_id == tab_id))
            .map(|(index, _)| index)
            .collect();
        let mut events = Vec::new();
        for index in &targets {
            state.revision += 1;
            let session = &state.sessions[*index];
            events.push(event(
                "closed",
                &session.thread_id,
                &session.tab_id,
                &created_at,
                &self.inner.server_epoch,
                state.revision,
                Map::new(),
            ));
        }
        for index in targets.into_iter().rev() {
            state.sessions.remove(index);
        }
        for event in events {
            state.publish(event);
        }
    }

    /// `list`: the thread's snapshots by `updatedAt`, with the epoch and revision.
    pub fn list(&self, thread_id: &str) -> Value {
        let state = self.inner.state.lock().unwrap();
        let mut sessions: Vec<Value> = state
            .sessions
            .iter()
            .filter(|session| session.thread_id == thread_id)
            .map(|session| session.snapshot.clone())
            .collect();
        sessions.sort_by(|left, right| left["updatedAt"].as_str().unwrap_or("").cmp(right["updatedAt"].as_str().unwrap_or("")));
        json!({"sessions": sessions, "serverEpoch": self.inner.server_epoch, "revision": state.revision})
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_THREAD: AtomicU64 = AtomicU64::new(0);

    fn fresh_thread() -> String {
        format!("thread-{}", NEXT_THREAD.fetch_add(1, Ordering::SeqCst) + 1)
    }

    fn drain(receiver: &mut mpsc::UnboundedReceiver<Value>) -> Vec<Value> {
        let mut events = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            events.push(event);
        }
        events
    }

    fn open(manager: &PreviewManager, thread_id: &str, url: Option<&str>) -> Value {
        manager
            .open(OpenInput {
                thread_id: thread_id.into(),
                url: url.map(str::to_owned),
                ..OpenInput::default()
            })
            .unwrap()
    }

    fn types(events: &[Value]) -> Vec<&str> {
        events.iter().map(|event| event["type"].as_str().unwrap()).collect()
    }

    // Manager.test.ts
    #[test]
    fn opens_a_session_and_emits_opened_with_normalized_url() {
        let manager = PreviewManager::new();
        let mut events = manager.subscribe();
        let snapshot = open(&manager, &fresh_thread(), Some("localhost:5173"));
        assert!(snapshot["tabId"].as_str().unwrap().starts_with("tab_"));
        assert_eq!(snapshot["navStatus"], json!({"_tag": "Loading", "url": "http://localhost:5173/", "title": ""}));
        let events = drain(&mut events);
        assert_eq!(types(&events), vec!["opened"]);
        assert_eq!(events[0]["tabId"], snapshot["tabId"]);
        assert_eq!(events[0]["snapshot"], snapshot);
    }

    #[test]
    fn keeps_the_tabs_profile_across_navigation_and_status_reports() {
        let manager = PreviewManager::new();
        let thread = fresh_thread();
        let opened = manager
            .open(OpenInput {
                thread_id: thread.clone(),
                profile_id: Some("work".into()),
                ..OpenInput::default()
            })
            .unwrap();
        assert_eq!(opened["profileId"], "work");
        let tab = opened["tabId"].as_str().unwrap().to_owned();
        let navigated = manager
            .navigate(NavigateInput {
                thread_id: thread.clone(),
                tab_id: tab.clone(),
                url: "localhost:5173".into(),
                resolved_title: None,
            })
            .unwrap();
        assert_eq!(navigated["profileId"], "work");
        manager
            .report_status(ReportStatusInput {
                thread_id: thread.clone(),
                tab_id: tab.clone(),
                nav_status: json!({"_tag": "Success", "url": "http://localhost:5173/", "title": "Dev"}),
                can_go_back: true,
                can_go_forward: false,
            })
            .unwrap();
        let listed = manager.list(&thread);
        assert_eq!(listed["sessions"][0]["profileId"], "work");
    }

    #[test]
    fn opens_an_idle_tab_when_no_url_is_supplied() {
        let manager = PreviewManager::new();
        assert_eq!(open(&manager, &fresh_thread(), None)["navStatus"], json!({"_tag": "Idle"}));
    }

    #[test]
    fn orders_list_snapshots_and_events_with_one_monotonic_revision() {
        let manager = PreviewManager::new();
        let thread = fresh_thread();
        let mut receiver = manager.subscribe();
        let before = manager.list(&thread);
        let opened = open(&manager, &thread, Some("http://localhost:5173"));
        manager
            .navigate(NavigateInput {
                thread_id: thread.clone(),
                tab_id: opened["tabId"].as_str().unwrap().into(),
                url: "http://localhost:5173/ready".into(),
                resolved_title: None,
            })
            .unwrap();
        let events = drain(&mut receiver);
        let listed = manager.list(&thread);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["serverEpoch"], listed["serverEpoch"]);
        assert_eq!(events[1]["serverEpoch"], listed["serverEpoch"]);
        assert!(events[0]["revision"].as_u64() > before["revision"].as_u64());
        assert!(events[1]["revision"].as_u64() > events[0]["revision"].as_u64());
        assert_eq!(listed["revision"], events[1]["revision"]);
        assert_eq!(listed["sessions"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn treats_bare_hosts_as_https() {
        let manager = PreviewManager::new();
        assert_eq!(open(&manager, &fresh_thread(), Some("example.com"))["navStatus"]["url"], "https://example.com/");
    }

    #[test]
    fn rejects_empty_url_with_preview_invalid_url_error() {
        let manager = PreviewManager::new();
        let error = manager
            .open(OpenInput {
                thread_id: fresh_thread(),
                url: Some("   ".into()),
                ..OpenInput::default()
            })
            .unwrap_err();
        assert_eq!(error.tag(), "PreviewInvalidUrlError");
        let encoded = error.encoded();
        assert_eq!(encoded["inputLength"], 3);
        assert_eq!(encoded["reason"], "empty");
        assert!(encoded.get("rawUrl").is_none());
        assert_eq!(encoded["cause"]["_tag"], "PreviewUrlNormalizationError");
        assert_eq!(encoded["cause"]["reason"], "empty");
    }

    #[test]
    fn preserves_url_parser_failures_as_the_invalid_url_cause_chain() {
        let manager = PreviewManager::new();
        let raw = "https://user:password@example.com:bad/path?access_token=secret#fragment";
        let error = manager
            .open(OpenInput {
                thread_id: fresh_thread(),
                url: Some(raw.into()),
                ..OpenInput::default()
            })
            .unwrap_err();
        let encoded = error.encoded();
        assert_eq!(encoded["inputLength"], raw.len());
        assert_eq!(encoded["reason"], "parse");
        assert_eq!(encoded["protocol"], "https:");
        assert!(encoded["cause"]["cause"]["message"].is_string());
        let message = error.message();
        for secret in ["user", "password", "access_token", "secret", "fragment"] {
            assert!(!message.contains(secret), "{message}");
        }
    }

    #[test]
    fn navigate_updates_snapshot_and_emits_navigated() {
        let manager = PreviewManager::new();
        let thread = fresh_thread();
        let mut receiver = manager.subscribe();
        let opened = open(&manager, &thread, Some("http://localhost:5173"));
        let snapshot = manager
            .navigate(NavigateInput {
                thread_id: thread,
                tab_id: opened["tabId"].as_str().unwrap().into(),
                url: "http://localhost:5173/about".into(),
                resolved_title: Some("About".into()),
            })
            .unwrap();
        assert_eq!(
            snapshot["navStatus"],
            json!({"_tag": "Success", "url": "http://localhost:5173/about", "title": "About"})
        );
        assert_eq!(types(&drain(&mut receiver)), vec!["opened", "navigated"]);
    }

    #[test]
    fn navigate_fails_for_unknown_tab() {
        let manager = PreviewManager::new();
        let error = manager
            .navigate(NavigateInput {
                thread_id: fresh_thread(),
                tab_id: "tab_missing".into(),
                url: "http://localhost:5173".into(),
                resolved_title: None,
            })
            .unwrap_err();
        assert_eq!(error.tag(), "PreviewSessionLookupError");
        assert_eq!(error.encoded()["tabId"], "tab_missing");
    }

    #[test]
    fn resizes_a_tab_and_preserves_its_viewport_across_navigation_reports() {
        let manager = PreviewManager::new();
        let thread = fresh_thread();
        let mut receiver = manager.subscribe();
        let opened = open(&manager, &thread, Some("http://localhost:5173"));
        let tab = opened["tabId"].as_str().unwrap().to_owned();
        let viewport = json!({"_tag": "freeform", "width": 1024, "height": 768});
        let resized = manager.resize(&thread, &tab, viewport.clone()).unwrap();
        assert_eq!(resized["viewport"], viewport);
        let navigated = manager
            .navigate(NavigateInput {
                thread_id: thread.clone(),
                tab_id: tab.clone(),
                url: "http://localhost:5173/resized".into(),
                resolved_title: None,
            })
            .unwrap();
        assert_eq!(navigated["viewport"], viewport);
        manager
            .report_status(ReportStatusInput {
                thread_id: thread.clone(),
                tab_id: tab,
                nav_status: json!({"_tag": "Success", "url": "http://localhost:5173/resized", "title": "Resized"}),
                can_go_back: true,
                can_go_forward: false,
            })
            .unwrap();
        assert_eq!(manager.list(&thread)["sessions"][0]["viewport"], viewport);
        assert_eq!(types(&drain(&mut receiver)), vec!["opened", "resized", "navigated", "navigated"]);
    }

    #[test]
    fn rejects_resize_for_an_unknown_tab() {
        let manager = PreviewManager::new();
        assert_eq!(
            manager.resize(&fresh_thread(), "tab_missing", fill_viewport()).unwrap_err().tag(),
            "PreviewSessionLookupError"
        );
    }

    #[test]
    fn report_status_emits_failed_for_load_failed_nav() {
        let manager = PreviewManager::new();
        let thread = fresh_thread();
        let mut receiver = manager.subscribe();
        let opened = open(&manager, &thread, Some("http://localhost:5173"));
        manager
            .report_status(ReportStatusInput {
                thread_id: thread,
                tab_id: opened["tabId"].as_str().unwrap().into(),
                nav_status: json!({"_tag": "LoadFailed", "url": "http://localhost:5173", "title": "", "code": -105, "description": "ERR_NAME_NOT_RESOLVED"}),
                can_go_back: false,
                can_go_forward: false,
            })
            .unwrap();
        let events = drain(&mut receiver);
        let failed = events.iter().find(|event| event["type"] == "failed").unwrap();
        assert_eq!(failed["code"], -105);
        assert_eq!(failed["description"], "ERR_NAME_NOT_RESOLVED");
    }

    #[test]
    fn close_removes_the_session_and_emits_closed() {
        let manager = PreviewManager::new();
        let thread = fresh_thread();
        let mut receiver = manager.subscribe();
        open(&manager, &thread, Some("http://localhost:5173"));
        manager.close(&thread, None);
        assert!(manager.list(&thread)["sessions"].as_array().unwrap().is_empty());
        assert!(drain(&mut receiver).iter().any(|event| event["type"] == "closed"));
    }

    #[test]
    fn gives_every_tab_in_a_batch_close_its_own_monotonic_revision() {
        let manager = PreviewManager::new();
        let thread = fresh_thread();
        open(&manager, &thread, Some("http://localhost:5173"));
        open(&manager, &thread, Some("http://localhost:3000"));
        let mut receiver = manager.subscribe();
        manager.close(&thread, None);
        let events = drain(&mut receiver);
        let listed = manager.list(&thread);
        assert_eq!(types(&events), vec!["closed", "closed"]);
        assert!(events[1]["revision"].as_u64() > events[0]["revision"].as_u64());
        assert_eq!(listed["revision"], events[1]["revision"]);
        assert!(listed["sessions"].as_array().unwrap().is_empty());
    }

    #[test]
    fn close_is_idempotent_for_unknown_threads() {
        let manager = PreviewManager::new();
        let thread = fresh_thread();
        manager.close(&thread, None);
        assert!(manager.list(&thread)["sessions"].as_array().unwrap().is_empty());
    }

    #[test]
    fn list_returns_every_snapshot_for_the_thread_sorted_by_updated_at() {
        let manager = PreviewManager::new();
        let thread = fresh_thread();
        let first = open(&manager, &thread, Some("http://localhost:5173"));
        let second = open(&manager, &thread, Some("http://localhost:3000"));
        let ids: Vec<Value> = manager.list(&thread)["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["tabId"].clone())
            .collect();
        assert_eq!(ids, vec![first["tabId"].clone(), second["tabId"].clone()]);
    }

    #[test]
    fn open_creates_an_independent_tab_on_every_call() {
        let manager = PreviewManager::new();
        let thread = fresh_thread();
        let mut receiver = manager.subscribe();
        let a = open(&manager, &thread, Some("http://localhost:5173"));
        let b = open(&manager, &thread, Some("http://localhost:3000/path"));
        assert_ne!(a["tabId"], b["tabId"]);
        assert_eq!(manager.list(&thread)["sessions"].as_array().unwrap().len(), 2);
        assert_eq!(types(&drain(&mut receiver)), vec!["opened", "opened"]);
    }

    #[test]
    fn close_with_mismatching_tab_id_is_a_no_op() {
        let manager = PreviewManager::new();
        let thread = fresh_thread();
        open(&manager, &thread, Some("http://localhost:5173"));
        manager.close(&thread, Some("tab_missing"));
        assert_eq!(manager.list(&thread)["sessions"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn close_with_explicit_tab_id_removes_only_that_tab() {
        let manager = PreviewManager::new();
        let thread = fresh_thread();
        let a = open(&manager, &thread, Some("http://localhost:5173"));
        let b = open(&manager, &thread, Some("http://localhost:3000"));
        manager.close(&thread, a["tabId"].as_str());
        let ids: Vec<Value> = manager.list(&thread)["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["tabId"].clone())
            .collect();
        assert_eq!(ids, vec![b["tabId"].clone()]);
    }

    #[test]
    fn multiple_subscribers_receive_every_event_independently() {
        let manager = PreviewManager::new();
        let thread = fresh_thread();
        let mut a = manager.subscribe();
        let mut b = manager.subscribe();
        open(&manager, &thread, Some("http://localhost:5173"));
        open(&manager, &thread, Some("http://localhost:3000"));
        assert_eq!(types(&drain(&mut a)), vec!["opened", "opened"]);
        assert_eq!(types(&drain(&mut b)), vec!["opened", "opened"]);
        // A closed subscriber never fails a publisher.
        drop(a);
        open(&manager, &thread, None);
        assert_eq!(types(&drain(&mut b)), vec!["opened"]);
    }
}
