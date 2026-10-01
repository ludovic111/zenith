//! Helpers shared by the integration tests.
//!
//! The TS tests build read models, commands and events as object literals; the ports build
//! them as `serde_json::json!` values decoded into the generated contract types, and assert on
//! the wire JSON of what comes out. That keeps them close to the TS source.
#![allow(dead_code)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde_json::{json, Value};
use zc_contracts::{OrchestrationCommand, OrchestrationEvent, OrchestrationReadModel, OrchestrationThreadActivity};
use zc_orchestration::decider::{decide_orchestration_command, DeciderEnv};
use zc_orchestration::errors::CommandRejection;
use zc_orchestration::projector::{create_empty_read_model, project_event};

/// A decider clock pinned to a time (settable), with event ids `event-1`, `event-2`, …
pub struct TestEnv {
    now: Mutex<String>,
    next_id: AtomicU64,
}

impl TestEnv {
    pub fn at(now: &str) -> Self {
        Self {
            now: Mutex::new(now.to_owned()),
            next_id: AtomicU64::new(1),
        }
    }

    /// The real clock (like the TS tests that run on `NodeServices.layer`).
    pub fn now() -> Self {
        Self::at(&zc_core::time::now_iso())
    }

    /// `TestClock.setTime`.
    pub fn set_now(&self, now: &str) {
        *self.now.lock().unwrap() = now.to_owned();
    }
}

impl DeciderEnv for TestEnv {
    fn now_iso(&self) -> String {
        self.now.lock().unwrap().clone()
    }
    fn new_event_id(&self) -> String {
        format!("event-{}", self.next_id.fetch_add(1, Ordering::SeqCst))
    }
}

/// Decodes a value into a contract type, with a readable panic.
pub fn decode<T: serde::de::DeserializeOwned>(value: Value) -> T {
    let text = value.to_string();
    serde_json::from_value(value).unwrap_or_else(|error| panic!("cannot decode {text}: {error}"))
}

pub fn read_model(value: Value) -> OrchestrationReadModel {
    decode(value)
}

pub fn command(value: Value) -> OrchestrationCommand {
    decode(value)
}

pub fn activity(value: Value) -> OrchestrationThreadActivity {
    decode(value)
}

/// `createEmptyReadModel(now)`.
pub fn empty_model(now: &str) -> OrchestrationReadModel {
    create_empty_read_model(now)
}

/// `decideOrchestrationCommand`: the planned events as wire JSON (with `"sequence": 0`), or the
/// rejection.
pub fn decide(env: &TestEnv, command_value: Value, model: &OrchestrationReadModel) -> Result<Vec<Value>, CommandRejection> {
    decide_with(env, command_value, model, None)
}

/// [`decide`] with the request activity the engine reads for question answers.
pub fn decide_with(
    env: &TestEnv,
    command_value: Value,
    model: &OrchestrationReadModel,
    user_input_activity: Option<&OrchestrationThreadActivity>,
) -> Result<Vec<Value>, CommandRejection> {
    let command = command(command_value);
    decide_orchestration_command(&command, model, user_input_activity, env).map(|events| {
        events
            .into_iter()
            .map(|event| serde_json::to_value(event.into_event(0)).expect("encode the event"))
            .collect()
    })
}

/// A full event envelope around a payload (`makeEvent` in the TS tests). The aggregate kind
/// follows the event type.
pub fn make_event(sequence: i64, event_type: &str, occurred_at: &str, aggregate_id: &str, payload: Value) -> Value {
    json!({
        "sequence": sequence,
        "eventId": format!("event-{sequence}"),
        "aggregateKind": if event_type.starts_with("project.") { "project" } else { "thread" },
        "aggregateId": aggregate_id,
        "occurredAt": occurred_at,
        "commandId": format!("cmd-{sequence}"),
        "causationEventId": null,
        "correlationId": null,
        "metadata": {},
        "type": event_type,
        "payload": payload,
    })
}

/// `projectEvent(model, event)`, with the event given as wire JSON.
pub fn apply(model: &mut OrchestrationReadModel, event: Value) {
    let event: OrchestrationEvent = decode(event);
    project_event(model, &event);
}

/// Applies the events of a decision to a model, numbering them after its sequence.
pub fn apply_decided(model: &mut OrchestrationReadModel, events: &[Value]) {
    for event in events {
        let mut event = event.clone();
        event["sequence"] = json!(model.snapshot_sequence + 1);
        apply(model, event);
    }
}

/// The wire JSON of a model.
pub fn to_json<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("encode")
}

/// A thread in wire form with every required field, `overrides` merged on top (keys set to
/// `null` stay `null`).
pub fn thread_json(id: &str, project_id: &str, now: &str, overrides: Value) -> Value {
    let mut thread = json!({
        "id": id,
        "projectId": project_id,
        "title": "Thread",
        "modelSelection": {"instanceId": "codex", "model": "gpt-5.4"},
        "runtimeMode": "full-access",
        "interactionMode": "default",
        "branch": null,
        "worktreePath": null,
        "pullRequests": [],
        "latestTurn": null,
        "createdAt": now,
        "updatedAt": now,
        "archivedAt": null,
        "settledOverride": null,
        "settledAt": null,
        "deletedAt": null,
        "messages": [],
        "proposedPlans": [],
        "activities": [],
        "checkpoints": [],
        "session": null,
    });
    merge(&mut thread, overrides);
    thread
}

/// A project in wire form, `overrides` merged on top.
pub fn project_json(id: &str, workspace_root: &str, now: &str, overrides: Value) -> Value {
    let mut project = json!({
        "id": id,
        "title": "Project",
        "workspaceRoot": workspace_root,
        "defaultModelSelection": null,
        "scripts": [],
        "createdAt": now,
        "updatedAt": now,
        "deletedAt": null,
    });
    merge(&mut project, overrides);
    project
}

/// Shallow-merges the keys of `overrides` (an object) into `target`.
pub fn merge(target: &mut Value, overrides: Value) {
    if let (Some(target), Value::Object(overrides)) = (target.as_object_mut(), overrides) {
        for (key, value) in overrides {
            target.insert(key, value);
        }
    }
}

/// The `_tag` of a rejection.
pub fn tag(rejection: &CommandRejection) -> &'static str {
    rejection.tag()
}

/// The paths where two JSON values differ (object key order ignored, numbers by value), at most
/// `limit` of them.
pub fn json_diff(expected: &Value, actual: &Value, limit: usize) -> Vec<String> {
    let mut out = Vec::new();
    diff_at("$", expected, actual, &mut out, limit);
    out
}

fn short(value: &Value) -> String {
    let text = value.to_string();
    if text.chars().count() > 160 {
        format!("{}…", text.chars().take(160).collect::<String>())
    } else {
        text
    }
}

fn diff_at(path: &str, expected: &Value, actual: &Value, out: &mut Vec<String>, limit: usize) {
    if out.len() >= limit {
        return;
    }
    match (expected, actual) {
        (Value::Object(left), Value::Object(right)) => {
            for (key, value) in left {
                match right.get(key) {
                    Some(other) => diff_at(&format!("{path}.{key}"), value, other, out, limit),
                    None => out.push(format!("{path}.{key}: missing in actual (expected {})", short(value))),
                }
            }
            for (key, value) in right {
                if !left.contains_key(key) {
                    out.push(format!("{path}.{key}: unexpected in actual ({})", short(value)));
                }
            }
        }
        (Value::Array(left), Value::Array(right)) => {
            if left.len() != right.len() {
                out.push(format!("{path}: length {} != {}", left.len(), right.len()));
            }
            for (index, (left, right)) in left.iter().zip(right.iter()).enumerate() {
                diff_at(&format!("{path}[{index}]"), left, right, out, limit);
            }
        }
        (Value::Number(left), Value::Number(right)) => {
            if left.as_f64() != right.as_f64() {
                out.push(format!("{path}: {left} != {right}"));
            }
        }
        (left, right) => {
            if left != right {
                out.push(format!("{path}: expected {} got {}", short(left), short(right)));
            }
        }
    }
}
