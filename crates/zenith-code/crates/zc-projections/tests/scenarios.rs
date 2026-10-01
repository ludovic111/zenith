//! The projection and query tests, ported as differential scenarios: the event sequences of
//! `ProjectionPipeline.test.ts` and `ProjectionSnapshotQuery.test.ts` (and more), each run
//! through the TS pipeline from source and through the Rust one, both by bootstrap replay and
//! event by event as the engine does. Every projection table, the attachment files left on
//! disk, and the answers to every snapshot query must be the same.
//!
//! Needs `node` and `code/apps/server/node_modules` (symlink the main checkout's); skipped
//! otherwise. All names are made up.

mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};
use zc_contracts::RepositoryIdentity;
use zc_db::repos::event_store::{self, NewEvent};
use zc_db::{Db, DbOptions};
use zc_projections::{FixedRepositoryIdentities, NoThreadLiveState, ProjectionPipeline, ProjectionSnapshotQuery};

use common::*;

const MODEL: &str = r#"{"instanceId":"codex","model":"gpt-5-codex"}"#;

/// An event log under construction.
#[derive(Default)]
struct Log {
    events: Vec<Value>,
    clock: i64,
}

fn at(ms: i64) -> String {
    let seconds = ms / 1000;
    format!(
        "2026-03-01T{:02}:{:02}:{:02}.{:03}Z",
        seconds / 3600,
        (seconds / 60) % 60,
        seconds % 60,
        ms % 1000
    )
}

impl Log {
    fn now(&mut self) -> String {
        self.clock += 1000;
        at(self.clock)
    }

    fn push_full(&mut self, event_type: &str, kind: &str, id: &str, command_id: Option<&str>, metadata: Value, payload: Value) -> &mut Self {
        let n = self.events.len() + 1;
        let occurred_at = at(self.clock);
        let command = command_id.map(str::to_owned).unwrap_or_else(|| format!("cmd-{n}"));
        self.events.push(json!({
            "type": event_type,
            "eventId": format!("evt-{n}"),
            "aggregateKind": kind,
            "aggregateId": id,
            "occurredAt": occurred_at,
            "commandId": command,
            "causationEventId": null,
            "correlationId": command,
            "metadata": metadata,
            "payload": payload,
        }));
        self
    }

    fn thread(&mut self, event_type: &str, thread: &str, payload: Value) -> &mut Self {
        self.push_full(event_type, "thread", thread, None, json!({}), payload)
    }

    fn project_created(&mut self, project: &str, root: &str) -> &mut Self {
        let now = self.now();
        self.push_full(
            "project.created",
            "project",
            project,
            None,
            json!({}),
            json!({
                "projectId": project,
                "title": format!("Project {project}"),
                "workspaceRoot": root,
                "defaultModelSelection": null,
                "scripts": [],
                "createdAt": now,
                "updatedAt": now,
            }),
        )
    }

    fn thread_created(&mut self, thread: &str, project: &str) -> &mut Self {
        let now = self.now();
        self.thread(
            "thread.created",
            thread,
            json!({
                "threadId": thread,
                "projectId": project,
                "title": format!("Thread {thread}"),
                "modelSelection": serde_json::from_str::<Value>(MODEL).unwrap(),
                "runtimeMode": "full-access",
                "branch": null,
                "worktreePath": null,
                "createdAt": now,
                "updatedAt": now,
            }),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn message(&mut self, thread: &str, message: &str, role: &str, text: &str, turn: Option<&str>, streaming: bool, extra: Value) -> &mut Self {
        let now = self.now();
        let mut payload = json!({
            "threadId": thread,
            "messageId": message,
            "role": role,
            "text": text,
            "turnId": turn,
            "streaming": streaming,
            "createdAt": now,
            "updatedAt": now,
        });
        if let Value::Object(extra) = extra {
            for (key, value) in extra {
                payload[key] = value;
            }
        }
        self.thread("thread.message-sent", thread, payload)
    }

    fn user(&mut self, thread: &str, message: &str, text: &str) -> &mut Self {
        self.message(thread, message, "user", text, None, false, json!({}))
    }

    fn assistant(&mut self, thread: &str, message: &str, turn: &str, text: &str, streaming: bool) -> &mut Self {
        self.message(thread, message, "assistant", text, Some(turn), streaming, json!({}))
    }

    fn turn_start(&mut self, thread: &str, message: &str) -> &mut Self {
        let now = self.now();
        self.thread(
            "thread.turn-start-requested",
            thread,
            json!({"threadId": thread, "messageId": message, "createdAt": now}),
        )
    }

    fn session(&mut self, thread: &str, status: &str, active_turn: Option<&str>) -> &mut Self {
        self.session_with(thread, status, active_turn, None)
    }

    fn session_with(&mut self, thread: &str, status: &str, active_turn: Option<&str>, command: Option<&str>) -> &mut Self {
        let now = self.now();
        self.push_full(
            "thread.session-set",
            "thread",
            thread,
            command,
            json!({}),
            json!({
                "threadId": thread,
                "session": {
                    "threadId": thread,
                    "status": status,
                    "providerName": "codex",
                    "providerInstanceId": "codex",
                    "runtimeMode": "full-access",
                    "activeTurnId": active_turn,
                    "lastError": if status == "error" { json!("boom") } else { Value::Null },
                    "updatedAt": now,
                },
            }),
        )
    }

    fn activity(&mut self, thread: &str, id: &str, kind: &str, turn: Option<&str>, payload: Value) -> &mut Self {
        self.activity_with(thread, id, kind, turn, payload, json!({}))
    }

    fn activity_with(&mut self, thread: &str, id: &str, kind: &str, turn: Option<&str>, payload: Value, metadata: Value) -> &mut Self {
        let now = self.now();
        let tone = if kind.starts_with("approval") {
            "approval"
        } else if kind.starts_with("tool") {
            "tool"
        } else {
            "info"
        };
        let sequence = self.events.len() as i64;
        self.push_full(
            "thread.activity-appended",
            "thread",
            thread,
            None,
            metadata,
            json!({
                "threadId": thread,
                "activity": {
                    "id": id,
                    "tone": tone,
                    "kind": kind,
                    "summary": format!("Summary of {kind}"),
                    "payload": payload,
                    "turnId": turn,
                    "sequence": sequence,
                    "createdAt": now,
                },
            }),
        )
    }

    fn diff(&mut self, thread: &str, turn: &str, count: i64, status: &str, assistant: Option<&str>) -> &mut Self {
        let now = self.now();
        self.thread(
            "thread.turn-diff-completed",
            thread,
            json!({
                "threadId": thread,
                "turnId": turn,
                "checkpointTurnCount": count,
                "checkpointRef": format!("refs/t3/checkpoints/x/turn/{count}"),
                "status": status,
                "files": [{"path": "src/app.ts", "kind": "modified", "additions": count, "deletions": 1}],
                "assistantMessageId": assistant,
                "completedAt": now,
            }),
        )
    }

    fn simple(&mut self, event_type: &str, thread: &str, extra: Value) -> &mut Self {
        let now = self.now();
        let mut payload = json!({"threadId": thread, "updatedAt": now});
        if let Value::Object(extra) = extra {
            for (key, value) in extra {
                payload[key] = if value == json!("$now") { Value::String(now.clone()) } else { value };
            }
        }
        self.thread(event_type, thread, payload)
    }

    /// A full turn: user message, start request, running session, assistant reply, checkpoint,
    /// ready session.
    fn turn(&mut self, thread: &str, n: usize) -> &mut Self {
        let user = format!("{thread}-user-{n}");
        let turn = format!("{thread}-turn-{n}");
        self.user(thread, &user, &format!("Question number {n} about the widget"))
            .turn_start(thread, &user)
            .session(thread, "running", Some(&turn))
            .activity(thread, &format!("{thread}-act-{n}"), "tool.completed", Some(&turn), json!({"itemType": "command_execution", "title": "Ran tests", "data": {"item": {"command": "npm test", "aggregatedOutput": "line one\nline two"}}}))
            .assistant(thread, &format!("{thread}-reply-{n}"), &turn, &format!("Answer number {n}"), false)
            .diff(thread, &turn, n as i64, "ready", Some(&format!("{thread}-reply-{n}")))
            .session(thread, "ready", None)
    }
}

struct Scenario {
    name: &'static str,
    attachments: Vec<String>,
    log: Log,
}

fn attachment(thread_segment: &str, n: u32, kind: &str) -> (Value, String) {
    let id = format!("{thread_segment}-0f0e0d0c-0b0a-4908-8706-0504030201{n:02}");
    match kind {
        "image" => (
            json!({"type": "image", "id": id, "name": "shot.png", "mimeType": "image/png", "sizeBytes": 12}),
            format!("{id}.png"),
        ),
        _ => (
            json!({"type": "file", "id": id, "name": "notes.txt", "mimeType": "text/plain", "sizeBytes": 12}),
            format!("{id}.txt"),
        ),
    }
}

fn scenarios() -> Vec<Scenario> {
    let mut out = Vec::new();

    // "bootstraps all projection states and writes projection rows", streaming deltas,
    // "keeps accumulated assistant text when completion payload text is empty", imported
    // user messages are not queued work.
    let mut log = Log::default();
    log.project_created("p1", "/work/alpha")
        .thread_created("t1", "p1")
        .assistant("t1", "m1", "t1-turn", "hello", false)
        .message("t1", "m2", "assistant", "Hel", Some("t1-turn"), true, json!({}))
        .message("t1", "m2", "assistant", "lo wor", Some("t1-turn"), true, json!({}))
        .message("t1", "m2", "assistant", "ld", Some("t1-turn"), true, json!({}))
        .message("t1", "m2", "assistant", "", Some("t1-turn"), false, json!({}))
        .message("t1", "import:codex:session-1:0", "user", "imported question", None, false, json!({}))
        .user("t1", "m3", "A real question")
        .message("t1", "m4", "system", "system note", None, false, json!({}))
        .message("t1", "m5", "reasoning", "thinking about it", Some("t1-turn"), false, json!({}));
    out.push(Scenario {
        name: "messages",
        attachments: vec![],
        log,
    });

    // Turn lifecycle: interim assistant messages keep the turn running until the session
    // ends; a new active turn supersedes a running one; interrupts; checkpoints and the
    // "missing placeholder must not clobber a ready checkpoint" rule; error sessions.
    let mut log = Log::default();
    log.project_created("p1", "/work/alpha").thread_created("t1", "p1");
    log.user("t1", "u1", "first")
        .turn_start("t1", "u1")
        .session("t1", "starting", None)
        .session("t1", "running", Some("turn-1"))
        .assistant("t1", "a1", "turn-1", "interim", false)
        .diff("t1", "turn-1", 1, "missing", None)
        .assistant("t1", "a2", "turn-1", "final", false)
        .diff("t1", "turn-1", 1, "ready", Some("a2"))
        .diff("t1", "turn-1", 1, "missing", None)
        .session("t1", "ready", None);
    log.user("t1", "u2", "second")
        .turn_start("t1", "u2")
        .session("t1", "running", Some("turn-2"))
        .user("t1", "u3", "steer")
        .turn_start("t1", "u3")
        .session("t1", "running", Some("turn-3"))
        .simple("thread.turn-interrupt-requested", "t1", json!({"turnId": "turn-3", "createdAt": "$now"}))
        .session("t1", "interrupted", None);
    log.simple("thread.turn-interrupt-requested", "t1", json!({"turnId": "turn-x", "createdAt": "$now"}))
        .simple("thread.turn-interrupt-requested", "t1", json!({"createdAt": "$now"}))
        .user("t1", "u4", "fourth")
        .turn_start("t1", "u4")
        .session("t1", "running", Some("turn-4"))
        .assistant("t1", "a4", "turn-4", "partial", true)
        .session("t1", "error", None)
        .diff("t1", "turn-5", 2, "error", None)
        .diff("t1", "turn-6", 2, "ready", None)
        .assistant("t1", "a7", "turn-7", "orphan reply", false);
    out.push(Scenario {
        name: "turns",
        attachments: vec![],
        log,
    });

    // Pending turn starts: terminal sessions clear them, a provider "ready" clears them,
    // only the compact request that produced a compaction is cleared, a failed start clears.
    let mut log = Log::default();
    log.project_created("p1", "/work/alpha").thread_created("t1", "p1");
    log.user("t1", "c1", " /COMPACT ")
        .turn_start("t1", "c1")
        .user("t1", "u1", "after compact")
        .turn_start("t1", "u1")
        .activity("t1", "cc-other", "context-compaction", None, json!({"requestId": "u-other"}))
        .activity("t1", "cc", "context-compaction", None, json!({"requestId": "c1"}))
        .user("t1", "u2", "next")
        .turn_start("t1", "u2")
        .activity("t1", "fail", "provider.turn.start.failed", None, json!({"requestId": "u2", "detail": "nope"}))
        .user("t1", "u3", "again")
        .turn_start("t1", "u3")
        .session_with("t1", "ready", None, Some("server:provider-session-set:abc"))
        .user("t1", "u4", "more")
        .turn_start("t1", "u4")
        .session("t1", "stopped", None)
        .user("t1", "u5", "plan follow-up");
    let now = log.now();
    log.thread(
        "thread.turn-start-requested",
        "t1",
        json!({"threadId": "t1", "messageId": "u5", "sourceProposedPlan": {"threadId": "t0", "planId": "plan-a"}, "createdAt": now}),
    )
    .session("t1", "running", Some("turn-plan"))
    .session("t1", "idle", None);
    out.push(Scenario {
        name: "pending-turn-starts",
        attachments: vec![],
        log,
    });

    // Approvals: requested, response requested, resolved with decision, failed reply restores
    // a request, stale failures resolve, the metadata request id fallback, unrelated kinds.
    let mut log = Log::default();
    log.project_created("p1", "/work/alpha").thread_created("t1", "p1").turn("t1", 1);
    log.activity(
        "t1",
        "ap1",
        "approval.requested",
        Some("t1-turn-1"),
        json!({"requestId": "r1", "detail": "run rm"}),
    )
    .activity("t1", "ap2", "approval.requested", Some("t1-turn-1"), json!({"requestId": "r2"}))
    .activity("t1", "ap3", "approval.requested", Some("t1-turn-1"), json!({"requestId": "r3"}))
    .activity_with(
        "t1",
        "ap4",
        "approval.requested",
        None,
        json!({"detail": "metadata id"}),
        json!({"requestId": "r4"}),
    );
    let now = log.now();
    log.thread(
        "thread.approval-response-requested",
        "t1",
        json!({"threadId": "t1", "requestId": "r1", "decision": "accept", "createdAt": now}),
    );
    log.activity(
        "t1",
        "ap1-failed",
        "provider.approval.respond.failed",
        None,
        json!({"requestId": "r1", "detail": "transport closed"}),
    )
    .activity(
        "t1",
        "ap2-resolved",
        "approval.resolved",
        None,
        json!({"requestId": "r2", "decision": "acceptForSession"}),
    )
    .activity(
        "t1",
        "ap2-failed",
        "provider.approval.respond.failed",
        None,
        json!({"requestId": "r2", "detail": "transport closed"}),
    )
    .activity(
        "t1",
        "ap3-stale",
        "provider.approval.respond.failed",
        None,
        json!({"requestId": "r3", "detail": "Stale pending approval request"}),
    )
    .activity("t1", "ap5", "approval.resolved", None, json!({"requestId": "r5", "decision": "weird"}))
    .activity("t1", "ui-req", "user-input.requested", None, json!({"requestId": "r6"}));
    out.push(Scenario {
        name: "approvals",
        attachments: vec![],
        log,
    });

    // User input lifecycle and the shell's pending-user-input count.
    let mut log = Log::default();
    log.project_created("p1", "/work/alpha").thread_created("t1", "p1").thread_created("t2", "p1");
    log.activity(
        "t1",
        "q1",
        "user-input.requested",
        None,
        json!({"requestId": "q-1", "questions": [{"id": "a", "question": "Pick?"}]}),
    )
    .activity("t1", "q2", "user-input.requested", None, json!({"requestId": "q-2"}))
    .activity("t1", "q3", "user-input.requested", None, json!({"requestId": "q-3"}))
    .activity("t1", "q1-done", "user-input.resolved", None, json!({"requestId": "q-1"}))
    .activity(
        "t1",
        "q2-stale",
        "provider.user-input.respond.failed",
        None,
        json!({"requestId": "q-2", "detail": "Unknown pending user-input request"}),
    )
    .activity(
        "t1",
        "q3-other",
        "provider.user-input.respond.failed",
        None,
        json!({"requestId": "q-3", "detail": "socket closed"}),
    )
    .activity("t2", "q4", "user-input.requested", None, json!({"requestId": "q-4"}));
    let now = log.now();
    log.thread(
        "thread.user-input-response-requested",
        "t2",
        json!({"threadId": "t2", "requestId": "q-4", "answers": {"a": "yes"}, "createdAt": now}),
    );
    out.push(Scenario {
        name: "user-input",
        attachments: vec![],
        log,
    });

    // Proposed plans and hasActionableProposedPlan (latest turn's plans win).
    let mut log = Log::default();
    log.project_created("p1", "/work/alpha").thread_created("t1", "p1").turn("t1", 1);
    for (id, turn, implemented) in [
        ("plan-a", Some("t1-turn-1"), false),
        ("plan-b", Some("t1-turn-1"), true),
        ("plan-c", None, false),
    ] {
        let now = log.now();
        log.thread(
            "thread.proposed-plan-upserted",
            "t1",
            json!({"threadId": "t1", "proposedPlan": {
                "id": id, "turnId": turn, "planMarkdown": format!("  # {id}\n- step  "),
                "implementedAt": if implemented { json!(now.clone()) } else { Value::Null },
                "implementationThreadId": if implemented { json!("t9") } else { Value::Null },
                "createdAt": now, "updatedAt": now,
            }}),
        );
    }
    log.turn("t1", 2);
    out.push(Scenario {
        name: "proposed-plans",
        attachments: vec![],
        log,
    });

    // Reverts: messages, activities, plans and turns past the kept turn count go, system and
    // imported messages stay, fallback user/assistant retention, and "does not fallback-retain
    // messages whose turnId is removed by revert"; pruned attachment files.
    let mut log = Log::default();
    let (image, image_file) = attachment("t1", 1, "image");
    let (late, late_file) = attachment("t1", 2, "file");
    log.project_created("p1", "/work/alpha").thread_created("t1", "p1");
    log.message("t1", "u1", "user", "with image", None, false, json!({"attachments": [image]}))
        .turn_start("t1", "u1")
        .session("t1", "running", Some("t1-turn-1"))
        .assistant("t1", "a1", "t1-turn-1", "one", false)
        .diff("t1", "t1-turn-1", 1, "ready", Some("a1"))
        .session("t1", "ready", None)
        .message("t1", "u2", "user", "with file", None, false, json!({"attachments": [late]}))
        .turn_start("t1", "u2")
        .session("t1", "running", Some("t1-turn-2"))
        .assistant("t1", "a2", "t1-turn-2", "two", false)
        .activity("t1", "act-2", "tool.completed", Some("t1-turn-2"), json!({"title": "edit"}))
        .diff("t1", "t1-turn-2", 2, "ready", Some("a2"))
        .session("t1", "ready", None)
        .message("t1", "sys", "system", "note", Some("t1-turn-2"), false, json!({}))
        .message("t1", "import:codex:s:1", "user", "imported", Some("t1-turn-2"), false, json!({}))
        .user("t1", "u3", "no turn yet")
        .assistant("t1", "a3", "t1-turn-9", "unknown turn", false);
    let now = log.now();
    log.thread("thread.proposed-plan-upserted", "t1", json!({"threadId": "t1", "proposedPlan": {"id": "plan-late", "turnId": "t1-turn-2", "planMarkdown": "later", "implementedAt": null, "implementationThreadId": null, "createdAt": now, "updatedAt": now}}));
    log.thread("thread.reverted", "t1", json!({"threadId": "t1", "turnCount": 1}));
    log.thread("thread.reverted", "t1", json!({"threadId": "t1", "turnCount": 0}));
    out.push(Scenario {
        name: "revert",
        attachments: vec![
            image_file,
            late_file,
            "t1-0f0e0d0c-0b0a-4908-8706-050403020199.png".into(),
            "other-0f0e0d0c-0b0a-4908-8706-050403020101.png".into(),
        ],
        log,
    });

    // Thread lifecycle fields and the legacy single-link replay.
    let mut log = Log::default();
    log.project_created("p1", "/work/alpha").thread_created("t1", "p1").thread_created("t2", "p1");
    log.simple("thread.archived", "t2", json!({"archivedAt": "$now"}))
        .simple("thread.settled", "t1", json!({"settledAt": "$now"}))
        .simple("thread.unsettled", "t1", json!({"reason": "user"}))
        .simple("thread.unsettled", "t1", json!({"reason": "activity"}))
        .simple("thread.settled", "t1", json!({"settledAt": "$now"}))
        .simple("thread.unsettled", "t1", json!({"reason": "activity"}))
        .simple("thread.snoozed", "t1", json!({"snoozedUntil": "2026-04-01T00:00:00.000Z", "snoozedAt": "$now"}))
        .simple("thread.pinned", "t1", json!({"pinnedAt": "$now", "pinOrderKey": "a0"}))
        .simple("thread.pinned", "t1", json!({"pinnedAt": "$now"}))
        .simple("thread.pin-reordered", "t1", json!({"orderKey": "a5"}))
        .simple("thread.auto-settle-set", "t1", json!({"autoSettleDisabledAt": "$now"}))
        .simple("thread.runtime-mode-set", "t1", json!({"runtimeMode": "approval-required"}))
        .simple("thread.interaction-mode-set", "t1", json!({"interactionMode": "plan"}))
        .simple("thread.meta-updated", "t1", json!({"title": "Renamed", "activeOrderKey": "k1", "titleState": {"source": "manual", "version": "cmd-x", "needsRefinement": false}, "titleRegeneration": {"requestId": "cmd-r", "startedAt": "2026-03-01T00:00:00.000Z"}, "modelSelection": {"instanceId": "claude", "model": "claude-sonnet"}, "branch": "feature/x", "worktreePath": "/work/alpha-wt", "branchPullRequest": {"projectId": "p1", "repository": "acme/widgets", "number": 7, "url": "https://github.com/acme/widgets/pull/7"}}))
        .simple("thread.meta-updated", "t1", json!({"linkedPullRequest": {"projectId": "p1", "repository": "Acme/Widgets", "number": 9, "url": "https://github.com/acme/widgets/pull/9"}}))
        .simple("thread.meta-updated", "t1", json!({"titleRegeneration": null, "activeOrderKey": null, "branch": null}))
        .simple("thread.meta-updated", "t2", json!({"linkedPullRequest": {"projectId": "p1", "repository": "proj", "number": 3, "url": "https://dev.azure.com/org/proj/_git/repo/pullrequest/3"}}))
        .simple("thread.meta-updated", "t2", json!({"linkedPullRequest": null}))
        .simple("thread.unsnoozed", "t1", json!({"reason": "user"}))
        .simple("thread.unpinned", "t1", json!({}))
        .simple("thread.unarchived", "t2", json!({}))
        .simple("thread.archived", "t2", json!({"archivedAt": "$now"}))
        .simple("thread.unsettled", "t9", json!({"reason": "user"}));
    out.push(Scenario {
        name: "thread-lifecycle",
        attachments: vec![],
        log,
    });

    // Pull request links: link, sync, a stale sync, unlink, Forgejo authorities, native
    // stacks, and deletion dropping links.
    let mut log = Log::default();
    log.project_created("p1", "/work/widgets").thread_created("t1", "p1").thread_created("t2", "p1");
    let link = |number: i64, url: &str, host: &str, repository: &str, source: &str, linked_at: &str| {
        json!({
            "host": host, "repository": repository, "number": number, "url": url, "source": source,
            "linkedAt": linked_at, "snapshot": null, "stack": null,
        })
    };
    let snapshot = |state: &str, head: &str, base: &str| {
        json!({
            "state": state, "title": "Add the widget", "headBranch": head, "baseBranch": base,
            "isDraft": false, "updatedAt": "2026-03-02T00:00:00.000Z", "syncedAt": "2026-03-02T00:00:00.000Z",
        })
    };
    let t = log.now();
    log.thread(
        "thread.pull-request-linked",
        "t1",
        json!({"threadId": "t1", "link": link(11, "https://github.com/acme/widgets/pull/11", "github.com", "acme/widgets", "created", &t), "updatedAt": t}),
    );
    let t = log.now();
    log.thread(
        "thread.pull-request-linked",
        "t1",
        json!({"threadId": "t1", "link": link(12, "https://github.com/acme/widgets/pull/12", "GitHub.com", "Acme/Widgets", "agent", &t), "updatedAt": t}),
    );
    let t = log.now();
    log.thread(
        "thread.pull-request-linked",
        "t1",
        json!({"threadId": "t1", "link": link(5, "http://forge.example:3000/team/app/pulls/5", "forge.example", "team/app", "manual", &t), "updatedAt": t}),
    );
    let t = log.now();
    log.thread("thread.pull-request-synced", "t1", json!({"threadId": "t1", "host": "github.com", "repository": "acme/widgets", "number": 11, "snapshot": snapshot("open", "feature-1", "main"), "stack": null, "updatedAt": t}));
    let t = log.now();
    log.thread("thread.pull-request-synced", "t1", json!({"threadId": "t1", "host": "github.com", "repository": "ACME/widgets", "number": 12, "snapshot": snapshot("open", "feature-2", "feature-1"), "stack": {"kind": "native", "id": "stk", "number": 12, "url": "https://github.com/acme/widgets/pull/12", "base": "main", "layers": [{"number": 11, "headBranch": "feature-1", "state": "open"}, {"number": 12, "headBranch": "feature-2", "state": "open"}]}, "updatedAt": t}));
    let t = log.now();
    log.thread("thread.pull-request-synced", "t1", json!({"threadId": "t1", "host": "forge.example:3000", "repository": "team/app", "number": 5, "snapshot": snapshot("merged", "fix", "main"), "stack": null, "updatedAt": t}));
    let t = log.now();
    log.thread("thread.pull-request-synced", "t1", json!({"threadId": "t1", "host": "github.com", "repository": "acme/widgets", "number": 99, "snapshot": snapshot("open", "x", "main"), "stack": null, "updatedAt": t}));
    let t = log.now();
    log.thread(
        "thread.pull-request-linked",
        "t2",
        json!({"threadId": "t2", "link": link(21, "https://github.com/acme/widgets/pull/21", "github.com", "acme/widgets", "manual", &t), "updatedAt": t}),
    );
    let t = log.now();
    log.thread("thread.pull-request-linked", "t2", json!({"threadId": "t2", "link": link(22, "https://github.com/acme/widgets/pull/22", "github.com", "acme/widgets", "stack-dismissed", &t), "updatedAt": t}));
    let t = log.now();
    log.thread(
        "thread.pull-request-unlinked",
        "t2",
        json!({"threadId": "t2", "host": "GITHUB.com", "repository": "acme/widgets", "number": 21, "updatedAt": t}),
    );
    log.thread_created("t3", "p1");
    let t = log.now();
    log.thread(
        "thread.pull-request-linked",
        "t3",
        json!({"threadId": "t3", "link": link(31, "https://github.com/acme/widgets/pull/31", "github.com", "acme/widgets", "manual", &t), "updatedAt": t}),
    );
    let t = log.now();
    log.thread("thread.deleted", "t3", json!({"threadId": "t3", "deletedAt": t}));
    out.push(Scenario {
        name: "pull-requests",
        attachments: vec![],
        log,
    });

    // Deletion and re-creation of a thread id (draft retry): "re-creating a deleted thread id
    // starts from an empty projection", "replaying a superseded thread.deleted keeps the
    // re-created thread's files", unsafe thread ids, deleted worktrees.
    let mut log = Log::default();
    let (first, first_file) = attachment("t1", 3, "image");
    let (second, second_file) = attachment("t1", 4, "image");
    log.project_created("p1", "/work/alpha").thread_created("t1", "p1");
    log.simple(
        "thread.meta-updated",
        "t1",
        json!({"branch": "feature/gone", "worktreePath": "/work/alpha-gone"}),
    )
    .message("t1", "u1", "user", "old", None, false, json!({"attachments": [first]}))
    .turn("t1", 1)
    .activity("t1", "ap-old", "approval.requested", None, json!({"requestId": "r-old"}));
    let t = log.now();
    log.thread("thread.deleted", "t1", json!({"threadId": "t1", "deletedAt": t}));
    log.thread_created("t1", "p1")
        .message("t1", "u2", "user", "new", None, false, json!({"attachments": [second]}));
    log.thread_created("---", "p1");
    let t = log.now();
    log.thread("thread.deleted", "---", json!({"threadId": "---", "deletedAt": t}));
    log.thread_created("t5", "p1").simple(
        "thread.meta-updated",
        "t5",
        json!({"branch": "feature/five", "worktreePath": "/work/alpha-five"}),
    );
    let t = log.now();
    log.thread("thread.deleted", "t5", json!({"threadId": "t5", "deletedAt": t}));
    out.push(Scenario {
        name: "recreate-and-delete",
        attachments: vec![first_file, second_file, "t5-0f0e0d0c-0b0a-4908-8706-050403020105.png".into()],
        log,
    });

    // Projects: meta updates (scripts, icon, env mode, auto-pull, favicon), deletion.
    let mut log = Log::default();
    log.project_created("p1", "/work/alpha")
        .project_created("p2", "/work/beta")
        .project_created("p3", "/work/gamma");
    let t = log.now();
    log.push_full("project.meta-updated", "project", "p1", None, json!({}), json!({"projectId": "p1", "title": "Alpha Renamed", "scripts": [{"id": "test", "name": "Test", "command": "npm test", "icon": "test", "runOnWorktreeCreate": false}], "projectIcon": {"kind": "emoji", "emoji": "🚀"}, "defaultThreadEnvMode": "worktree", "autoPull": true, "faviconPath": "public/icon.png", "defaultModelSelection": {"instanceId": "codex", "model": "gpt-5-codex"}, "updatedAt": t}));
    let t = log.now();
    log.push_full(
        "project.meta-updated",
        "project",
        "p1",
        None,
        json!({}),
        json!({"projectId": "p1", "projectIcon": null, "faviconPath": null, "updatedAt": t}),
    );
    let t = log.now();
    log.push_full("project.deleted", "project", "p2", None, json!({}), json!({"projectId": "p2", "deletedAt": t}));
    let t = log.now();
    log.push_full(
        "project.meta-updated",
        "project",
        "p9",
        None,
        json!({}),
        json!({"projectId": "p9", "title": "Ghost", "updatedAt": t}),
    );
    log.thread_created("t1", "p1").thread_created("t2", "p3").turn("t2", 1);
    out.push(Scenario {
        name: "projects",
        attachments: vec![],
        log,
    });

    // Windowed thread details: many turns, subagent fan-out turns without a user message,
    // turnless activities and messages between turns.
    let mut log = Log::default();
    log.project_created("p1", "/work/alpha").thread_created("t1", "p1");
    log.message("t1", "import:codex:s:0", "user", "imported history", None, false, json!({}));
    for n in 1..=7 {
        log.turn("t1", n);
        if n % 3 == 0 {
            log.activity("t1", &format!("ctx-{n}"), "context-window.updated", None, json!({"usedTokens": n * 100}))
                .session("t1", "running", Some(&format!("t1-sub-{n}")))
                .assistant("t1", &format!("t1-sub-reply-{n}"), &format!("t1-sub-{n}"), "subagent", false)
                .session("t1", "ready", None);
        }
    }
    log.user("t1", "t1-tail", "a message with no turn yet");
    log.thread_created("t2", "p1");
    out.push(Scenario {
        name: "windows",
        attachments: vec![],
        log,
    });

    // Activity payload projection: MCP calls with results, commands, file changes, preview
    // tools, question tools, context-window rows, superseded tool updates.
    let mut log = Log::default();
    log.project_created("p1", "/work/alpha").thread_created("t1", "p1").turn("t1", 1);
    let turn = Some("t1-turn-1");
    log.activity("t1", "mcp1", "tool.completed", turn, json!({"itemType": "mcp_tool_call", "status": "completed", "title": "Search", "data": {"toolCallId": "call-1", "item": {"type": "mcpToolCall", "id": "i1", "tool": "search", "server": "docs", "status": "failed", "arguments": {"q": "x"}, "result": {"content": [{"type": "text", "text": "first line\nsecond line"}]}, "extra": "dropped"}, "kind": "mcp"}}))
        .activity("t1", "mcp2", "tool.completed", turn, json!({"itemType": "mcp_tool_call", "data": {"toolName": "mcp__t3-code__preview_open", "input": {"url": "x"}, "result": {"content": [{"type": "text", "text": "{\"url\": \" https://localhost:5173/app \"}"}]}}}))
        .activity("t1", "cmd1", "tool.completed", turn, json!({"itemType": "command_execution", "data": {"command": ["bash", "-lc", "ls"], "rawOutput": {"stdout": "\n\n```\nalpha beta\n"}, "result": {"content": "x"}}}))
        .activity("t1", "cmd2", "tool.completed", turn, json!({"itemType": "command_execution", "data": {"item": {"input": {"command": "pwd"}, "result": {"command": "pwd", "content": "a\nb\nc"}}, "result": {"content": [{"text": "one"}, {"text": "two"}]}}}))
        .activity("t1", "files1", "tool.completed", turn, json!({"itemType": "file_change", "data": {"changes": [{"path": " src/a.ts "}, {"filePath": "src/b.ts"}, {"path": "src/a.ts"}], "toolCallId": "call-f", "rawOutput": {"totalFiles": 3, "truncated": true}}}))
        .activity("t1", "read1", "tool.completed", turn, json!({"itemType": "dynamic_tool_call", "data": {"toolName": "Read", "input": {"file_path": "/work/alpha/logo.PNG?raw"}, "content": [{"type": "content", "content": {"type": "text", "text": "  image bytes  "}}]}}))
        .activity("t1", "ask1", "tool.completed", turn, json!({"itemType": "dynamic_tool_call", "title": "AskUserQuestion", "data": {"input": {"questions": [{"question": " Which one? "}, {"prompt": "Or this?"}, {"question_text": "And?"}]}}}))
        .activity("t1", "up1", "tool.updated", turn, json!({"itemType": "file_change", "title": "Editing app.ts", "data": {"toolCallId": "call-e"}}))
        .activity("t1", "up2", "tool.updated", turn, json!({"itemType": "file_change", "title": "Editing app.ts", "data": {"toolCallId": "call-e"}}))
        .activity("t1", "done-e", "tool.completed", turn, json!({"itemType": "file_change", "title": "Editing app.ts complete", "data": {"toolCallId": "call-e"}}))
        .activity("t1", "up3", "tool.updated", turn, json!({"itemType": "file_change", "title": "Editing app.ts", "data": {"toolCallId": "call-e"}}))
        .activity("t1", "up-anon", "tool.updated", turn, json!({"itemType": "web_search", "title": "Searching"}))
        .activity("t1", "done-anon", "tool.completed", turn, json!({"itemType": "web_search", "title": "Searching completed"}))
        .activity("t1", "ctx1", "context-window.updated", turn, json!({"usedTokens": 10}))
        .activity("t1", "ctx2", "context-window.updated", turn, json!({"usedTokens": 20}))
        .activity("t1", "ctx-bad", "context-window.updated", turn, json!({"usedTokens": -1}))
        .activity("t1", "plain", "task.progress", turn, json!("not an object"))
        .activity("t1", "nodata", "task.progress", turn, json!({"data": [1, 2]}))
        .message("t1", "r1", "reasoning", "deep thought", turn, false, json!({}));
    out.push(Scenario {
        name: "activity-payloads",
        attachments: vec![],
        log,
    });

    // Search: roles, escaping, long snippets, deleted and archived threads.
    let mut log = Log::default();
    log.project_created("p1", "/work/alpha")
        .project_created("p2", "/work/beta")
        .thread_created("t1", "p1")
        .thread_created("t2", "p1")
        .thread_created("t3", "p2")
        .thread_created("t4", "p1");
    let long = format!("{} needle in the middle {}", "lorem ipsum ".repeat(30), "dolor sit amet ".repeat(30));
    log.user("t1", "s1", "Find 100% of the_widgets! please")
        .user("t1", "s2", &long)
        .turn("t2", 1)
        .user("t3", "s3", "needle in a deleted project")
        .user("t4", "s4", "needle in an archived thread")
        .message("t2", "s5", "reasoning", "needle in a thought", Some("t2-turn-1"), false, json!({}));
    let t = log.now();
    log.push_full("project.deleted", "project", "p2", None, json!({}), json!({"projectId": "p2", "deletedAt": t}));
    log.simple("thread.archived", "t4", json!({"archivedAt": "$now"}));
    out.push(Scenario {
        name: "search",
        attachments: vec![],
        log,
    });

    out
}

fn identities() -> HashMap<String, Option<RepositoryIdentity>> {
    let identity = |canonical: &str, remote: &str, display: &str, provider: &str, owner: &str, name: &str| -> RepositoryIdentity {
        serde_json::from_value(json!({
            "canonicalKey": canonical,
            "locator": {"source": "git-remote", "remoteName": "origin", "remoteUrl": remote},
            "rootPath": "/work",
            "displayName": display,
            "provider": provider,
            "owner": owner,
            "name": name,
        }))
        .unwrap()
    };
    HashMap::from([
        (
            "/work/widgets".to_string(),
            Some(identity(
                "github.com/acme/widgets",
                "git@github.com:acme/widgets.git",
                "acme/widgets",
                "github",
                "acme",
                "widgets",
            )),
        ),
        (
            "/work/alpha".to_string(),
            Some(identity(
                "forge.example/team/app",
                "http://forge.example:3000/team/app.git",
                "team/app",
                "forgejo",
                "team",
                "app",
            )),
        ),
        ("/work/beta".to_string(), None),
    ])
}

/// Builds a Rust database for one scenario and mode, with its attachment files.
fn run_rust(dir: &Path, scenario: &Scenario, mode: &str) -> (PathBuf, Vec<String>) {
    let db_path = dir.join(format!("{}-{mode}-rust.sqlite", scenario.name));
    let attachments = dir.join(format!("{}-{mode}-rust-attachments", scenario.name));
    std::fs::create_dir_all(&attachments).unwrap();
    for file in &scenario.attachments {
        std::fs::write(attachments.join(file), file).unwrap();
    }
    let db = Db::open_with(&db_path, DbOptions { migrate: true, readers: 0 }).unwrap();
    let pipeline = ProjectionPipeline::new(&attachments);
    let events = scenario.log.events.clone();
    let live = mode == "live";
    db.call_blocking(move |conn| {
        for event in &events {
            let new_event: NewEvent = serde_json::from_value(event.clone()).unwrap();
            let stored = event_store::append(conn, &new_event)?;
            if live {
                // As the engine does: project inside the append transaction, clean up after.
                let cleanup = conn.transaction(|conn| pipeline.project_persisted_deferred(conn, &stored))?;
                cleanup.run(conn);
            }
        }
        if !live {
            pipeline.bootstrap(conn)?;
        }
        Ok(())
    })
    .unwrap();
    let mut files: Vec<String> = std::fs::read_dir(&attachments)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    files.sort();
    (db_path, files)
}

/// The query requests for one scenario, with page cursors discovered on the Rust side.
async fn requests_for(query: &ProjectionSnapshotQuery, db_path: &Path) -> Vec<Value> {
    let strings = |sql: &str| -> Vec<String> {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        let mut statement = conn.prepare(sql).unwrap();
        statement.query_map([], |row| row.get::<_, String>(0)).unwrap().map(Result::unwrap).collect()
    };
    let threads = strings("SELECT thread_id FROM projection_threads ORDER BY created_at");
    let projects = strings("SELECT project_id FROM projection_projects ORDER BY created_at");
    let roots = strings("SELECT workspace_root FROM projection_projects ORDER BY created_at");
    let kinds = strings("SELECT DISTINCT kind FROM projection_thread_activities ORDER BY kind");
    let requests_ids = strings("SELECT DISTINCT json_extract(payload_json, '$.requestId') FROM projection_thread_activities WHERE json_extract(payload_json, '$.requestId') IS NOT NULL");
    let messages = strings("SELECT thread_id || char(31) || message_id FROM projection_thread_messages ORDER BY created_at");
    let mut requests = vec![
        json!({"method": "getShellSnapshot"}),
        json!({"method": "getShellSnapshot", "args": [{"unsettledOnly": true}]}),
        json!({"method": "getArchivedShellSnapshot"}),
        json!({"method": "getCommandReadModel"}),
        json!({"method": "getSnapshot"}),
        json!({"method": "getSnapshotSequence"}),
        json!({"method": "getCounts"}),
        json!({"method": "listThreadsWithPullRequests"}),
        json!({"method": "getDeletedWorktreeThreads"}),
        json!({"method": "getProjectShells"}),
        json!({"method": "getProjectShells", "args": [["p1", "p2", "nope"]]}),
        json!({"method": "getEventReplayStats", "args": [{"fromSequenceExclusive": 2, "toSequenceInclusive": 9}]}),
        json!({"method": "getActiveProjectByWorkspaceRoot", "args": ["/nowhere"]}),
    ];
    for query_text in ["needle", "NEEDLE", "100%", "the_widgets", "!", "question", "Answer", "thought", "imported"] {
        requests.push(json!({"method": "searchThreads", "args": [{"query": query_text}]}));
    }
    requests.push(json!({"method": "searchThreads", "args": [{"query": "e", "limit": 2}]}));
    for kind in &kinds {
        requests.push(json!({"method": "listActivitiesByKind", "args": [kind]}));
    }
    for project in &projects {
        requests.push(json!({"method": "getProjectShellById", "args": [project]}));
        requests.push(json!({"method": "getFirstActiveThreadIdByProjectId", "args": [project]}));
        requests.push(json!({"method": "getImportedAgentSessionSources", "args": [project]}));
    }
    for root in &roots {
        requests.push(json!({"method": "getActiveProjectByWorkspaceRoot", "args": [root]}));
    }
    for pair in &messages {
        let (thread, message) = pair.split_once('\u{1f}').unwrap();
        requests.push(json!({"method": "getTurnStartMessage", "args": [{"threadId": thread, "messageId": message}]}));
    }
    let mut unique_threads = threads.clone();
    unique_threads.dedup();
    for thread in &unique_threads {
        for request_id in &requests_ids {
            requests.push(json!({"method": "getUserInputActivity", "args": [{"threadId": thread, "requestId": request_id}]}));
        }
        requests.push(json!({"method": "getThreadShellById", "args": [thread]}));
        requests.push(json!({"method": "getThreadRuntimeContext", "args": [thread]}));
        requests.push(json!({"method": "getThreadCheckpointContext", "args": [thread]}));
        for count in 0..3 {
            requests.push(json!({"method": "getFullThreadDiffContext", "args": [thread, count]}));
        }
        requests.push(json!({"method": "getThreadDetailById", "args": [thread]}));
        requests.push(
            json!({"method": "getThreadDetailById", "args": [thread, {"activityKinds": ["tool.completed", "approval.requested", "context-window.updated"]}]}),
        );
        requests.push(json!({"method": "getThreadDetailById", "args": [thread, {"activityKinds": []}]}));
        requests.push(json!({"method": "getThreadDetailSnapshot", "args": [thread]}));
        requests.push(json!({"method": "getThreadDetailSnapshotProjected", "args": [thread, null, false]}));
        requests.push(json!({"method": "getThreadDetailSnapshotProjected", "args": [thread, null, true]}));
        for turn_limit in [1, 2, 3] {
            let mut window = json!({"turnLimit": turn_limit});
            for _ in 0..12 {
                requests.push(json!({"method": "getThreadDetailSnapshot", "args": [thread, window.clone()]}));
                requests.push(json!({"method": "getThreadDetailSnapshotProjected", "args": [thread, window.clone(), true]}));
                let answer = rust_answer(query, "getThreadDetailSnapshot", &[json!(thread), window.clone()]).await;
                match answer["ok"]["page"]["beforeCursor"].as_str() {
                    Some(cursor) => window = json!({"turnLimit": turn_limit, "beforeCursor": cursor}),
                    None => break,
                }
            }
        }
    }
    requests
}

#[tokio::test(flavor = "multi_thread")]
async fn scenarios_project_and_answer_like_typescript() {
    if !node_available() {
        eprintln!("skipped: node or code/apps/server/node_modules missing");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let scenarios = scenarios();
    let identity_map = identities();

    let mut oracle_input = Vec::new();
    let mut rust_results = Vec::new();
    for scenario in &scenarios {
        let bootstrap = tokio::task::block_in_place(|| run_rust(dir.path(), scenario, "bootstrap"));
        let live = tokio::task::block_in_place(|| run_rust(dir.path(), scenario, "live"));
        let query = ProjectionSnapshotQuery::new(
            Db::open_with(&bootstrap.0, DbOptions { migrate: false, readers: 0 }).unwrap(),
            Arc::new(FixedRepositoryIdentities(identity_map.clone())),
            Arc::new(NoThreadLiveState),
        );
        let requests = requests_for(&query, &bootstrap.0).await;
        oracle_input.push(json!({
            "name": scenario.name,
            "attachments": scenario.attachments,
            "events": scenario.log.events,
            "requests": requests,
            "identities": identity_map,
        }));
        rust_results.push((bootstrap, live, query, requests));
    }
    let input_file = dir.path().join("scenarios.json");
    std::fs::write(&input_file, serde_json::to_string(&oracle_input).unwrap()).unwrap();
    let out_dir = dir.path().join("ts");
    std::fs::create_dir_all(&out_dir).unwrap();
    run_oracle(&["scenarios", "--file", input_file.to_str().unwrap(), "--out-dir", out_dir.to_str().unwrap()]);

    let mut failures = Vec::new();
    let mut compared_queries = 0;
    let mut compared_rows = 0;
    for (scenario, ((bootstrap, bootstrap_files), (live, live_files), query, requests)) in scenarios.iter().zip(&rust_results) {
        for (mode, rust_db, rust_files) in [("bootstrap", bootstrap, bootstrap_files), ("live", live, live_files)] {
            let ts_db = out_dir.join(format!("{}-{mode}.sqlite", scenario.name));
            for (table, key) in PROJECTION_TABLES {
                let rust_rows = table_rows(rust_db, table, key);
                let ts_rows = table_rows(&ts_db, table, key);
                compared_rows += rust_rows.len();
                let diff = diff_rows(&rust_rows, &ts_rows);
                for (key, columns) in diff.iter().take(5) {
                    failures.push(format!("{} {mode} {table} {key}: {columns:?}", scenario.name));
                }
            }
            let ts_files: Vec<String> =
                serde_json::from_str(&std::fs::read_to_string(out_dir.join(format!("{}-{mode}-files.json", scenario.name))).unwrap()).unwrap();
            if !scenario.attachments.is_empty() {
                eprintln!(
                    "   {} {mode}: {} of {} attachment files left",
                    scenario.name,
                    rust_files.len(),
                    scenario.attachments.len()
                );
            }
            if &ts_files != rust_files {
                failures.push(format!("{} {mode} attachments: rust {rust_files:?} ts {ts_files:?}", scenario.name));
            }
        }
        let ts_answers: Vec<Value> = std::fs::read_to_string(out_dir.join(format!("{}-answers.jsonl", scenario.name)))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(ts_answers.len(), requests.len());
        for (request, ts) in requests.iter().zip(&ts_answers) {
            let method = request["method"].as_str().unwrap();
            let args: Vec<Value> = request["args"].as_array().cloned().unwrap_or_default();
            let rust = rust_answer(query, method, &args).await;
            compared_queries += 1;
            let mut diffs = Vec::new();
            json_diff("", &rust, ts, &mut diffs);
            if !diffs.is_empty() {
                failures.push(format!(
                    "{} {method} {}: {}",
                    scenario.name,
                    serde_json::to_string(&args).unwrap().chars().take(120).collect::<String>(),
                    diffs.iter().take(4).cloned().collect::<Vec<_>>().join("; ")
                ));
            }
        }
    }
    eprintln!(
        "{} scenarios, {compared_rows} projection rows and {compared_queries} query answers compared",
        scenarios.len()
    );
    for failure in &failures {
        eprintln!("   {failure}");
    }
    assert!(failures.is_empty(), "{} differences", failures.len());
}
