//! `serverLifecycleEvents.ts` and the `subscribeServerLifecycle` handler of `ws.ts`: the
//! startup publishes a `welcome` and then a `ready` event; subscribers get the latest of each
//! (sorted by sequence), then every later event.

use std::sync::{Arc, Mutex};

use futures::stream::{self, BoxStream, StreamExt};
use serde_json::{json, Value};
use zc_core::PubSub;

#[derive(Default)]
struct State {
    sequence: i64,
    /// The latest `welcome` and the latest `ready`, encoded.
    events: Vec<Value>,
}

/// `ServerLifecycleEvents`. Cheap to clone.
#[derive(Clone, Default)]
pub struct ServerLifecycleEvents {
    state: Arc<Mutex<State>>,
    events: PubSub<Value>,
}

impl ServerLifecycleEvents {
    pub fn new() -> Self {
        Self::default()
    }

    /// `publish`: numbers the event (`version, sequence, type, payload`, the
    /// `ServerLifecycleStreamEvent` declaration order), keeps it as the latest of its type and
    /// broadcasts it.
    pub fn publish(&self, kind: &str, payload: Value) -> Value {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.sequence += 1;
        let event = json!({ "version": 1, "sequence": state.sequence, "type": kind, "payload": payload });
        state.events.retain(|entry| entry["type"] != kind);
        state.events.insert(0, event.clone());
        // Published under the lock, so a subscriber's snapshot and stream continue each other.
        self.events.publish(event.clone());
        event
    }

    /// `welcome`: `{environment, cwd, projectName, bootstrapStatus}`.
    pub fn publish_welcome(&self, environment: &Value, cwd: &str, bootstrap_status: &str) -> Value {
        let project_name = cwd.split(['/', '\\']).rfind(|segment| !segment.is_empty()).unwrap_or("project");
        self.publish(
            "welcome",
            json!({ "environment": environment, "cwd": cwd, "projectName": project_name, "bootstrapStatus": bootstrap_status }),
        )
    }

    /// `ready`: `{at, environment}`.
    pub fn publish_ready(&self, environment: &Value) -> Value {
        self.publish("ready", json!({ "at": zc_core::now_iso(), "environment": environment }))
    }

    /// The latest sequence and events (`snapshot`).
    pub fn snapshot(&self) -> (i64, Vec<Value>) {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        (state.sequence, state.events.clone())
    }

    /// `subscribeServerLifecycle`: the snapshot's events by sequence, then live events with a
    /// higher sequence. Never completes.
    pub fn subscribe(&self) -> BoxStream<'static, Value> {
        let (sequence, mut events, live) = {
            let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            (state.sequence, state.events.clone(), self.events.subscribe())
        };
        events.sort_by_key(|event| event["sequence"].as_i64().unwrap_or(0));
        let live = live.filter(move |event| futures::future::ready(event["sequence"].as_i64().unwrap_or(0) > sequence));
        stream::iter(events).chain(live).boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn snapshot_then_live_events() {
        let lifecycle = ServerLifecycleEvents::new();
        let environment = json!({"environmentId": "environment-1"});
        lifecycle.publish_welcome(&environment, "/tmp/sample-project", "complete");
        lifecycle.publish_welcome(&environment, "/tmp/sample-project", "complete");
        let mut stream = lifecycle.subscribe();
        let first = stream.next().await.unwrap();
        assert_eq!(first["type"], "welcome");
        assert_eq!(first["sequence"], 2);
        assert_eq!(first["payload"]["projectName"], "sample-project");
        lifecycle.publish_ready(&environment);
        let ready = stream.next().await.unwrap();
        assert_eq!(ready["type"], "ready");
        assert_eq!(ready["sequence"], 3);
        assert_eq!(
            serde_json::to_string(&ready).unwrap().split("\"payload\"").next().unwrap(),
            r#"{"version":1,"sequence":3,"type":"ready","#
        );
        let (sequence, events) = lifecycle.snapshot();
        assert_eq!(sequence, 3);
        assert_eq!(events.len(), 2);
    }
}
