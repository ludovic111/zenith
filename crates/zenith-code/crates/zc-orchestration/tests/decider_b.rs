//! Ports of the decider unit tests: `decider.pullRequests.test.ts`,
//! `decider.questionAttachments.test.ts`, `decider.settled.test.ts`, `decider.snoozed.test.ts`,
//! `decider.titleRegeneration.test.ts`, `decider.turnDiffComplete.test.ts`,
//! `decider.userInputDismiss.test.ts`, `decider.userMessageAppend.test.ts` and
//! `messageContext.test.ts`, one module per file.
//!
//! The TS tests run under `it.effect`, whose decider clock is the Effect `TestClock` pinned to
//! the epoch: the ports pin [`TestEnv`] to [`EPOCH`] the same way.

mod common;

use common::*;
use serde_json::{json, Value};

/// The `TestClock` start time the TS tests decide at.
const EPOCH: &str = "1970-01-01T00:00:00.000Z";

fn env() -> TestEnv {
    TestEnv::at(EPOCH)
}

/// The event types of a decision.
fn types(events: &[Value]) -> Vec<&str> {
    events.iter().map(|event| event["type"].as_str().unwrap()).collect()
}

/// `expectSingleEvent`: the first event, which must have the given type.
fn first_of(events: &[Value], event_type: &str) -> Value {
    let event = events.first().unwrap_or_else(|| panic!("expected {event_type}, got no event"));
    assert_eq!(event["type"], event_type, "events: {events:#?}");
    event.clone()
}

/// The wire JSON of the model's first thread.
fn first_thread(model: &zc_contracts::OrchestrationReadModel) -> Value {
    to_json(&model.threads[0])
}

/// vitest `toMatchObject`: every key of `expected` is in `actual` with a matching value
/// (recursively for objects; arrays match element-wise with the same length).
#[track_caller]
fn assert_match(actual: &Value, expected: &Value) {
    let mut out = Vec::new();
    match_at("$", actual, expected, &mut out);
    assert!(out.is_empty(), "toMatchObject failed:\n{}\nactual: {actual:#}", out.join("\n"));
}

fn match_at(path: &str, actual: &Value, expected: &Value, out: &mut Vec<String>) {
    match (expected, actual) {
        (Value::Object(expected), Value::Object(actual)) => {
            for (key, value) in expected {
                match actual.get(key) {
                    Some(other) => match_at(&format!("{path}.{key}"), other, value, out),
                    None => out.push(format!("{path}.{key}: missing (expected {value})")),
                }
            }
        }
        (Value::Array(expected), Value::Array(actual)) => {
            if expected.len() != actual.len() {
                out.push(format!("{path}: length {} != {}", actual.len(), expected.len()));
            }
            for (index, (expected, actual)) in expected.iter().zip(actual.iter()).enumerate() {
                match_at(&format!("{path}[{index}]"), actual, expected, out);
            }
        }
        (Value::Number(expected), Value::Number(actual)) => {
            if expected.as_f64() != actual.as_f64() {
                out.push(format!("{path}: expected {expected} got {actual}"));
            }
        }
        (expected, actual) => {
            if expected != actual {
                out.push(format!("{path}: expected {expected} got {actual}"));
            }
        }
    }
}

/// `toEqual` on JSON (object key order ignored).
#[track_caller]
fn assert_json_eq(actual: &Value, expected: &Value) {
    let diff = json_diff(expected, actual, 20);
    assert!(diff.is_empty(), "values differ:\n{}\nactual: {actual:#}", diff.join("\n"));
}

/// A read model holding one thread (wire JSON).
fn model_with_thread(thread: Value, projects: Value, now: &str) -> zc_contracts::OrchestrationReadModel {
    read_model(json!({
        "snapshotSequence": 0,
        "projects": projects,
        "threads": [thread],
        "updatedAt": now,
    }))
}

// ---------------------------------------------------------------------------------------------
mod pull_requests {
    //! `decider.pullRequests.test.ts` ("pull request link decider").

    use super::*;

    const NOW: &str = "2026-01-01T00:00:00.000Z";
    const THREAD_ID: &str = "thread-1";

    fn make_link(overrides: Value) -> Value {
        let mut link = json!({
            "host": "github.com",
            "repository": "acme/widgets",
            "number": 42,
            "url": "https://github.com/acme/widgets/pull/42",
            "source": "manual",
            "linkedAt": NOW,
            "snapshot": null,
            "stack": null,
        });
        merge(&mut link, overrides);
        link
    }

    fn project(repository_identity: Value) -> Value {
        project_json("project-1", "/repo", NOW, json!({"repositoryIdentity": repository_identity}))
    }

    fn github_identity() -> Value {
        json!({
            "canonicalKey": "github.com/acme/widgets",
            "provider": "github",
            "displayName": "acme/widgets",
            "locator": {
                "source": "git-remote",
                "remoteName": "origin",
                "remoteUrl": "https://github.com/acme/widgets.git",
            },
        })
    }

    fn make_read_model(pull_requests: Value) -> zc_contracts::OrchestrationReadModel {
        make_read_model_with_identity(pull_requests, github_identity())
    }

    fn make_read_model_with_identity(pull_requests: Value, identity: Value) -> zc_contracts::OrchestrationReadModel {
        model_with_thread(
            thread_json(THREAD_ID, "project-1", NOW, json!({"pullRequests": pull_requests})),
            json!([project(identity)]),
            NOW,
        )
    }

    fn snapshot() -> Value {
        json!({
            "state": "open",
            "title": "Add links",
            "headBranch": "feat/links",
            "baseBranch": "main",
            "isDraft": false,
            "updatedAt": NOW,
            "syncedAt": NOW,
        })
    }

    fn merged_snapshot() -> Value {
        let mut value = snapshot();
        value["state"] = json!("merged");
        value
    }

    fn with_source(link: &Value, source: &str) -> Value {
        let mut link = link.clone();
        link["source"] = json!(source);
        link
    }

    #[test]
    fn links_the_same_forgejo_number_on_two_ports_and_unlinks_an_older_portless_record() {
        let env = env();
        let existing = make_link(json!({
            "host": "forge.example",
            "url": "http://forge.example:3000/acme/widgets/pulls/42",
        }));
        let mut model = make_read_model(json!([existing]));
        let events = decide(
            &env,
            json!({
                "type": "thread.pull-request.link",
                "commandId": "link-other-port",
                "threadId": THREAD_ID,
                "host": "forge.example",
                "repository": "acme/widgets",
                "number": 42,
                "url": "http://forge.example:4000/acme/widgets/pulls/42",
                "source": "manual",
            }),
            &model,
        )
        .unwrap();
        let linked = first_of(&events, "thread.pull-request-linked");
        assert_eq!(linked["payload"]["link"]["host"], "forge.example:4000");
        apply_decided(&mut model, std::slice::from_ref(&linked));
        assert_eq!(model.threads[0].pull_requests.len(), 2);

        let events = decide(
            &env,
            json!({
                "type": "thread.pull-request.unlink",
                "commandId": "unlink-old-port",
                "threadId": THREAD_ID,
                "host": "forge.example:3000",
                "repository": "acme/widgets",
                "number": 42,
            }),
            &model,
        )
        .unwrap();
        let unlinked = first_of(&events, "thread.pull-request-unlinked");
        apply_decided(&mut model, std::slice::from_ref(&unlinked));
        let urls: Vec<Value> = first_thread(&model)["pullRequests"]
            .as_array()
            .unwrap()
            .iter()
            .map(|link| link["url"].clone())
            .collect();
        assert_eq!(urls, vec![linked["payload"]["link"]["url"].clone()]);
    }

    #[test]
    fn legacy_unlink_cannot_remove_a_newer_cross_host_link() {
        let env = env();
        let own = make_link(json!({}));
        let foreign = make_link(json!({
            "host": "github.enterprise.test",
            "url": "https://github.enterprise.test/acme/widgets/pull/42",
            "linkedAt": "2026-01-02T00:00:00Z",
        }));
        let events = decide(
            &env,
            json!({
                "type": "thread.meta.update",
                "commandId": "unlink",
                "threadId": THREAD_ID,
                "linkedPullRequest": null,
            }),
            &make_read_model(json!([own, foreign])),
        )
        .unwrap();
        let event = first_of(&events, "thread.pull-request-unlinked");
        assert_eq!(event["payload"]["host"], "github.com");
    }

    #[test]
    fn legacy_replacement_preserves_unrelated_manual_links() {
        let env = env();
        let other = make_link(json!({"number": 7, "snapshot": merged_snapshot()}));
        let current = make_link(json!({"linkedAt": "2026-01-02T00:00:00Z"}));
        let mut model = make_read_model(json!([other, current]));
        let events = decide(
            &env,
            json!({
                "type": "thread.meta.update",
                "commandId": "replace",
                "threadId": THREAD_ID,
                "linkedPullRequest": {
                    "projectId": "project-1",
                    "repository": "acme/widgets",
                    "number": 99,
                    "url": "https://github.com/acme/widgets/pull/99",
                },
            }),
            &model,
        )
        .unwrap();
        assert_eq!(types(&events), ["thread.pull-request-unlinked", "thread.pull-request-linked"]);
        apply_decided(&mut model, &events);
        let numbers: Vec<u64> = model.threads[0]
            .pull_requests
            .iter()
            .map(|link| to_json(&link.number).as_u64().unwrap())
            .collect();
        assert_eq!(numbers, [7, 99]);
    }

    #[test]
    fn round_trips_an_azure_legacy_link_and_unlinks_only_its_organization() {
        let env = env();
        let foreign = make_link(json!({
            "host": "dev.azure.com",
            "repository": "org-b/project/_git/web",
            "number": 7,
            "url": "https://dev.azure.com/org-b/project/_git/web/pullrequest/7",
        }));
        let mut identity = github_identity();
        merge(
            &mut identity,
            json!({
                "provider": "azure-devops",
                "canonicalKey": "ssh.dev.azure.com/v3/org-a/project/web",
                "displayName": "v3/org-a/project/web",
                "name": "web",
            }),
        );
        let mut model = make_read_model_with_identity(json!([foreign]), identity);
        let legacy = json!({
            "projectId": "project-1",
            "repository": "web",
            "number": 7,
            "url": "https://dev.azure.com/org-a/project/_git/web/pullrequest/7",
        });
        for linked_pull_request in [legacy.clone(), Value::Null] {
            let events = decide(
                &env,
                json!({
                    "type": "thread.meta.update",
                    "commandId": if linked_pull_request.is_null() { "unlink-azure" } else { "link-azure" },
                    "threadId": THREAD_ID,
                    "linkedPullRequest": linked_pull_request,
                }),
                &model,
            )
            .unwrap();
            apply_decided(&mut model, &events);
            if !linked_pull_request.is_null() {
                let thread = first_thread(&model);
                assert_json_eq(&thread["linkedPullRequest"], &legacy);
                let repositories: Vec<&Value> = thread["pullRequests"].as_array().unwrap().iter().map(|link| &link["repository"]).collect();
                assert_eq!(repositories, [&json!("org-b/project/_git/web"), &json!("org-a/project/_git/web")]);
            }
        }
        let thread = first_thread(&model);
        assert_json_eq(&thread["pullRequests"], &json!([foreign]));
        assert_eq!(thread.get("linkedPullRequest"), Some(&Value::Null), "thread: {thread:#}");
    }

    #[test]
    fn legacy_unlink_alone_does_not_emit_an_empty_metadata_event() {
        let env = env();
        let events = decide(
            &env,
            json!({
                "type": "thread.meta.update",
                "commandId": "unlink",
                "threadId": THREAD_ID,
                "linkedPullRequest": null,
            }),
            &make_read_model(json!([make_link(json!({}))])),
        )
        .unwrap();
        assert_eq!(types(&events), ["thread.pull-request-unlinked"]);
    }

    /// `legacy unlink removes the visible ${source} link and preserves other requests`.
    ///
    /// The TS test also checks `isThreadDetailEvent(decoded) === false` (from `ws.ts`, the
    /// older WebSocket detail-event union); that predicate lives outside this crate and is not
    /// ported. The encode/decode round trip is kept: `apply` decodes the wire JSON.
    fn legacy_unlink_removes_the_visible_link(source: &str) {
        let env = env();
        let other = make_link(json!({"number": 7, "snapshot": merged_snapshot()}));
        let current = make_link(json!({"source": source, "linkedAt": "2026-01-02T00:00:00.000Z"}));
        let mut model = make_read_model(json!([other, current]));
        // This is the pre-array command shape sent by older clients.
        let events = decide(
            &env,
            json!({
                "type": "thread.meta.update",
                "commandId": "legacy-unlink",
                "threadId": THREAD_ID,
                "linkedPullRequest": null,
                "title": "Renamed by old client",
            }),
            &model,
        )
        .unwrap();
        for planned in &events {
            let mut event = planned.clone();
            event["sequence"] = json!(model.snapshot_sequence + 1);
            let encoded = to_json(&decode::<zc_contracts::OrchestrationEvent>(event));
            apply(&mut model, encoded);
        }
        let thread = first_thread(&model);
        assert_eq!(thread["title"], "Renamed by old client");
        let expected = if source == "stack" {
            json!([other, with_source(&current, "stack-dismissed")])
        } else {
            json!([other])
        };
        assert_json_eq(&thread["pullRequests"], &expected);
        // The old single-link field continues to track the remaining visible request.
        assert_eq!(thread["linkedPullRequest"]["number"], 7, "thread: {thread:#}");
    }

    #[test]
    fn legacy_unlink_removes_the_visible_manual_link_and_preserves_other_requests() {
        legacy_unlink_removes_the_visible_link("manual");
    }

    #[test]
    fn legacy_unlink_removes_the_visible_agent_link_and_preserves_other_requests() {
        legacy_unlink_removes_the_visible_link("agent");
    }

    #[test]
    fn legacy_unlink_removes_the_visible_created_link_and_preserves_other_requests() {
        legacy_unlink_removes_the_visible_link("created");
    }

    #[test]
    fn legacy_unlink_removes_the_visible_stack_link_and_preserves_other_requests() {
        legacy_unlink_removes_the_visible_link("stack");
    }

    #[test]
    fn links_a_pull_request_with_a_normalized_key_and_empty_host_state() {
        let env = env();
        let events = decide(
            &env,
            json!({
                "type": "thread.pull-request.link",
                "commandId": "cmd-link",
                "threadId": THREAD_ID,
                "host": " GitHub.com ",
                "repository": "Acme/Widgets",
                "number": 42,
                "url": "https://github.com/acme/widgets/pull/42",
                "source": "manual",
            }),
            &make_read_model(json!([])),
        )
        .unwrap();
        assert_eq!(events.len(), 1);
        let event = first_of(&events, "thread.pull-request-linked");
        assert_json_eq(
            &event["payload"]["link"],
            &json!({
                "host": "github.com",
                "repository": "acme/widgets",
                "number": 42,
                "url": "https://github.com/acme/widgets/pull/42",
                "source": "manual",
                "linkedAt": event["payload"]["updatedAt"],
                "snapshot": null,
                "stack": null,
            }),
        );
        assert_ne!(event["payload"]["updatedAt"], NOW);
    }

    #[test]
    fn rejects_linking_a_pull_request_that_is_already_linked() {
        let env = env();
        let error = decide(
            &env,
            json!({
                "type": "thread.pull-request.link",
                "commandId": "cmd-link-dup",
                "threadId": THREAD_ID,
                "host": "GITHUB.COM",
                "repository": "acme/widgets",
                "number": 42,
                "url": "https://github.com/acme/widgets/pull/42",
                "source": "agent",
            }),
            &make_read_model(json!([make_link(json!({}))])),
        )
        .unwrap_err();
        assert_eq!(tag(&error), "OrchestrationCommandInvariantError");
    }

    #[test]
    fn re_linking_a_dismissed_stack_member_un_dismisses_it() {
        let env = env();
        let dismissed = make_link(json!({
            "source": "stack-dismissed",
            "snapshot": snapshot(),
            "stack": {
                "kind": "native",
                "id": "stack-1",
                "number": 1,
                "url": "https://github.com/acme/widgets/stack/1",
                "base": "main",
                "layers": [{"number": 42, "headBranch": "feat/links", "state": "open"}],
            },
        }));
        let events = decide(
            &env,
            json!({
                "type": "thread.pull-request.link",
                "commandId": "cmd-relink",
                "threadId": THREAD_ID,
                "host": "github.com",
                "repository": "acme/widgets",
                "number": 42,
                "url": "https://github.com/acme/widgets/pull/42",
                "source": "manual",
            }),
            &make_read_model(json!([dismissed])),
        )
        .unwrap();
        let event = first_of(&events, "thread.pull-request-linked");
        // Host state survives the flip; only the source changes.
        assert_json_eq(&event["payload"]["link"], &with_source(&dismissed, "manual"));
    }

    #[test]
    fn rejects_a_stack_sync_re_adding_a_dismissed_stack_member() {
        let env = env();
        let error = decide(
            &env,
            json!({
                "type": "thread.pull-request.link",
                "commandId": "cmd-stack-readd",
                "threadId": THREAD_ID,
                "host": "github.com",
                "repository": "acme/widgets",
                "number": 42,
                "url": "https://github.com/acme/widgets/pull/42",
                "source": "stack",
            }),
            &make_read_model(json!([make_link(json!({"source": "stack-dismissed"}))])),
        )
        .unwrap_err();
        assert_eq!(tag(&error), "OrchestrationCommandInvariantError");
    }

    #[test]
    fn unlinks_a_manual_pull_request() {
        let env = env();
        let events = decide(
            &env,
            json!({
                "type": "thread.pull-request.unlink",
                "commandId": "cmd-unlink",
                "threadId": THREAD_ID,
                "host": "GitHub.com",
                "repository": "acme/widgets",
                "number": 42,
            }),
            &make_read_model(json!([make_link(json!({}))])),
        )
        .unwrap();
        let event = first_of(&events, "thread.pull-request-unlinked");
        assert_match(
            &event["payload"],
            &json!({
                "threadId": THREAD_ID,
                "host": "github.com",
                "repository": "acme/widgets",
                "number": 42,
            }),
        );
    }

    #[test]
    fn unlinking_a_stack_member_leaves_a_stack_dismissed_tombstone() {
        let env = env();
        let member = make_link(json!({"source": "stack", "snapshot": snapshot()}));
        let events = decide(
            &env,
            json!({
                "type": "thread.pull-request.unlink",
                "commandId": "cmd-unlink-stack",
                "threadId": THREAD_ID,
                "host": "github.com",
                "repository": "acme/widgets",
                "number": 42,
            }),
            &make_read_model(json!([member])),
        )
        .unwrap();
        let event = first_of(&events, "thread.pull-request-linked");
        assert_json_eq(&event["payload"]["link"], &with_source(&member, "stack-dismissed"));
    }

    /// `unlinking a ${source} member prevents its sibling rediscovering it`.
    fn unlinking_a_member_prevents_its_sibling_rediscovering_it(source: &str) {
        let env = env();
        let member = make_link(json!({"source": source}));
        let sibling = make_link(json!({
            "number": 43,
            "source": "stack",
            "stack": {
                "kind": "native",
                "id": "stack-1",
                "number": 1,
                "url": "https://github.com/acme/widgets/stack/1",
                "base": "main",
                "layers": [
                    {"number": 42, "headBranch": "first", "state": "open"},
                    {"number": 43, "headBranch": "second", "state": "open"},
                ],
            },
        }));
        let mut model = make_read_model(json!([member, sibling]));
        let events = decide(
            &env,
            json!({
                "type": "thread.pull-request.unlink",
                "commandId": "remove",
                "threadId": THREAD_ID,
                "host": member["host"],
                "repository": member["repository"],
                "number": member["number"],
            }),
            &model,
        )
        .unwrap();
        let event = first_of(&events, "thread.pull-request-linked");
        apply_decided(&mut model, std::slice::from_ref(&event));
        assert_json_eq(
            &first_thread(&model)["pullRequests"],
            &json!([with_source(&member, "stack-dismissed"), sibling]),
        );
        let rediscovered = decide(
            &env,
            json!({
                "type": "thread.pull-request.link",
                "commandId": "rediscovered",
                "threadId": THREAD_ID,
                "host": member["host"],
                "repository": member["repository"],
                "number": member["number"],
                "url": member["url"],
                "source": "stack",
            }),
            &model,
        );
        assert!(rediscovered.is_err(), "expected a failure, got {rediscovered:?}");
    }

    #[test]
    fn unlinking_a_manual_member_prevents_its_sibling_rediscovering_it() {
        unlinking_a_member_prevents_its_sibling_rediscovering_it("manual");
    }

    #[test]
    fn unlinking_an_agent_member_prevents_its_sibling_rediscovering_it() {
        unlinking_a_member_prevents_its_sibling_rediscovering_it("agent");
    }

    #[test]
    fn unlinking_a_created_member_prevents_its_sibling_rediscovering_it() {
        unlinking_a_member_prevents_its_sibling_rediscovering_it("created");
    }

    #[test]
    fn rejects_unlinking_a_pull_request_that_is_not_linked() {
        let env = env();
        let error = decide(
            &env,
            json!({
                "type": "thread.pull-request.unlink",
                "commandId": "cmd-unlink-missing",
                "threadId": THREAD_ID,
                "host": "github.com",
                "repository": "acme/widgets",
                "number": 7,
            }),
            &make_read_model(json!([make_link(json!({}))])),
        )
        .unwrap_err();
        assert_eq!(tag(&error), "OrchestrationCommandInvariantError");
    }

    #[test]
    fn rejects_syncing_a_pull_request_that_is_not_linked() {
        let env = env();
        let error = decide(
            &env,
            json!({
                "type": "thread.pull-request-link.sync",
                "commandId": "cmd-sync-missing",
                "threadId": THREAD_ID,
                "host": "github.com",
                "repository": "acme/widgets",
                "number": 42,
                "snapshot": snapshot(),
                "stack": null,
            }),
            &make_read_model(json!([])),
        )
        .unwrap_err();
        assert_eq!(tag(&error), "OrchestrationCommandInvariantError");
    }

    #[test]
    fn sync_emits_the_host_snapshot_for_a_linked_pull_request() {
        let env = env();
        let events = decide(
            &env,
            json!({
                "type": "thread.pull-request-link.sync",
                "commandId": "cmd-sync",
                "threadId": THREAD_ID,
                "host": "GitHub.com",
                "repository": "acme/widgets",
                "number": 42,
                "snapshot": snapshot(),
                "stack": null,
            }),
            &make_read_model(json!([make_link(json!({}))])),
        )
        .unwrap();
        let event = first_of(&events, "thread.pull-request-synced");
        assert_match(
            &event["payload"],
            &json!({
                "threadId": THREAD_ID,
                "host": "github.com",
                "repository": "acme/widgets",
                "number": 42,
                "snapshot": snapshot(),
                "stack": null,
            }),
        );
    }
}

// ---------------------------------------------------------------------------------------------
mod question_attachments {
    //! `decider.questionAttachments.test.ts` ("question attachment answers").

    use super::*;

    const UPDATED_AT: &str = "2026-01-01T00:00:00.000Z";

    fn model() -> zc_contracts::OrchestrationReadModel {
        model_with_thread(
            thread_json(
                "thread-1",
                "project-1",
                UPDATED_AT,
                json!({"title": "Manual title", "snoozedUntil": null, "snoozedAt": null}),
            ),
            json!([]),
            UPDATED_AT,
        )
    }

    fn attachments_by_question_id() -> Value {
        json!({
            "q": [{
                "type": "file",
                "id": "thread-1-00000000-0000-4000-8000-0000000000aa-txt",
                "name": "spec.txt",
                "mimeType": "text/plain",
                "sizeBytes": 4,
            }],
        })
    }

    fn command_json() -> Value {
        json!({
            "type": "thread.user-input.respond",
            "commandId": "answer",
            "threadId": "thread-1",
            "requestId": "question-request",
            "answers": {"q": ""},
            "createdAt": UPDATED_AT,
            "attachmentsByQuestionId": attachments_by_question_id(),
        })
    }

    fn question() -> Value {
        json!({"id": "q", "header": "Spec", "question": "Provide a spec", "options": [], "allowCustomAnswer": true})
    }

    fn request(questions: Value) -> Value {
        json!({
            "id": "question",
            "kind": "user-input.requested",
            "summary": "Question",
            "tone": "info",
            "turnId": null,
            "createdAt": UPDATED_AT,
            "payload": {"requestId": "question-request", "questions": questions},
        })
    }

    #[test]
    fn persists_the_original_answer_with_its_attachment_and_emits_a_provider_response() {
        let env = env();
        let request = activity(request(json!([question()])));
        let events = decide_with(&env, command_json(), &model(), Some(&request)).unwrap();
        assert_eq!(types(&events), ["thread.activity-appended", "thread.user-input-response-requested"]);
        assert_match(
            &events[0]["payload"],
            &json!({
                "activity": {
                    "kind": "user-input.answer-submitted",
                    "payload": {"answers": {"q": ""}, "attachmentsByQuestionId": attachments_by_question_id()},
                },
            }),
        );
        assert_match(
            &events[1]["payload"],
            &json!({"answers": {"q": ""}, "attachmentsByQuestionId": attachments_by_question_id()}),
        );
    }

    #[test]
    fn rejects_attachments_for_a_resolved_or_unknown_request() {
        let env = env();
        let result = decide(&env, command_json(), &model());
        assert!(result.is_err(), "expected a failure, got {result:?}");
    }

    #[test]
    fn rejects_unknown_question_ids_and_predefined_choice_only_protocols() {
        let env = env();
        let mut other_id = question();
        other_id["id"] = json!("other");
        let mut choice_only = question();
        choice_only["allowCustomAnswer"] = json!(false);
        for question in [other_id, choice_only] {
            let request = activity(request(json!([question])));
            let result = decide_with(&env, command_json(), &model(), Some(&request));
            assert!(result.is_err(), "expected a failure for {question}, got {result:?}");
        }
    }
}

// ---------------------------------------------------------------------------------------------
mod settled {
    //! `decider.settled.test.ts` ("settled thread decider").

    use super::*;
    use zc_orchestration::errors::CommandRejection;

    const NOW: &str = "2026-01-01T00:00:00.000Z";
    const SETTLED_AT: &str = "2025-12-30T00:00:00.000Z";
    const SETTLE_BLOCKED_MESSAGE: &str = "This thread still needs attention. Resolve or interrupt it first, then try again.";

    /// `makeReadModel(settledOverride, archivedAt, session, activities, messages, lifecycle)`.
    /// `lifecycle` holds `pinnedAt` / `snoozedUntil` / `snoozedAt`.
    fn make_read_model(
        settled_override: Option<&str>,
        archived_at: Option<&str>,
        session: Value,
        activities: Value,
        messages: Value,
        lifecycle: Value,
    ) -> zc_contracts::OrchestrationReadModel {
        let snoozed_until = lifecycle.get("snoozedUntil").cloned().unwrap_or(Value::Null);
        let snoozed_at = match lifecycle.get("snoozedAt") {
            Some(value) if !value.is_null() => value.clone(),
            _ if !snoozed_until.is_null() => json!(SETTLED_AT),
            _ => Value::Null,
        };
        model_with_thread(
            thread_json(
                "thread-1",
                "project-1",
                NOW,
                json!({
                    "archivedAt": archived_at,
                    "settledOverride": settled_override,
                    "settledAt": if settled_override == Some("settled") { Some(SETTLED_AT) } else { None },
                    "snoozedUntil": snoozed_until,
                    "snoozedAt": snoozed_at,
                    "pinnedAt": lifecycle.get("pinnedAt").cloned().unwrap_or(Value::Null),
                    "messages": messages,
                    "activities": activities,
                    "session": session,
                }),
            ),
            json!([]),
            NOW,
        )
    }

    fn simple_model(settled_override: Option<&str>) -> zc_contracts::OrchestrationReadModel {
        make_read_model(settled_override, None, Value::Null, json!([]), json!([]), json!({}))
    }

    fn make_session(status: &str) -> Value {
        json!({
            "threadId": "thread-1",
            "status": status,
            "providerName": "Codex",
            "runtimeMode": "full-access",
            "activeTurnId": null,
            "lastError": null,
            "updatedAt": NOW,
        })
    }

    fn settle(command_id: &str) -> Value {
        json!({"type": "thread.settle", "commandId": command_id, "threadId": "thread-1"})
    }

    fn find<'a>(events: &'a [Value], event_type: &str) -> &'a Value {
        events
            .iter()
            .find(|event| event["type"] == event_type)
            .unwrap_or_else(|| panic!("no {event_type} in {events:#?}"))
    }

    #[track_caller]
    fn assert_settle_blocked(error: &CommandRejection) {
        assert_eq!(tag(error), "OrchestrationThreadSettleBlockedError", "{error:?}");
        match error {
            CommandRejection::SettleBlocked { thread_id } => assert_eq!(thread_id.as_str(), "thread-1"),
            other => panic!("expected a settle-blocked rejection, got {other:?}"),
        }
        assert_eq!(error.to_string(), SETTLE_BLOCKED_MESSAGE);
    }

    #[test]
    fn preserves_the_activity_stamp_when_automatically_settling() {
        let env = env();
        let events = decide(
            &env,
            json!({
                "type": "thread.auto-settle",
                "commandId": "cmd-auto-settle-inactive",
                "threadId": "thread-1",
                "snapshotSequence": 0,
                "settledAt": SETTLED_AT,
            }),
            &simple_model(None),
        )
        .unwrap();
        let settled = find(&events, "thread.settled");
        assert_eq!(settled["payload"]["settledAt"], SETTLED_AT);
        // updatedAt stays the command time so the row still moves on settle.
        assert_eq!(settled["payload"]["updatedAt"], settled["occurredAt"]);
        assert_ne!(settled["payload"]["updatedAt"], SETTLED_AT);
    }

    #[test]
    fn rejects_an_automatic_settle_when_the_thread_is_pinned_active() {
        let env = env();
        let error = decide(
            &env,
            json!({
                "type": "thread.auto-settle",
                "commandId": "cmd-auto-settle",
                "threadId": "thread-1",
                "snapshotSequence": 0,
                "settledAt": SETTLED_AT,
            }),
            &simple_model(Some("active")),
        )
        .unwrap_err();
        assert_eq!(tag(&error), "OrchestrationCommandInvariantError");
    }

    #[test]
    fn settles_awake_threads_without_a_redundant_wake_and_re_emits_idempotently() {
        let env = env();
        let events = decide(&env, settle("cmd-settle"), &simple_model(None)).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "thread.settled");
        assert_eq!(events[0]["payload"]["settledAt"], events[0]["payload"]["updatedAt"]);

        // Already settled: the engine rejects zero-event commands, so idempotency
        // is by re-emission — preserving the original settledAt.
        let re_emit = decide(&env, settle("cmd-settle-again"), &simple_model(Some("settled"))).unwrap();
        assert_eq!(re_emit.len(), 1);
        assert_eq!(re_emit[0]["type"], "thread.settled");
        assert_eq!(re_emit[0]["payload"]["settledAt"], SETTLED_AT);
        // updatedAt must NOT rewind to the historical settledAt: sorting and
        // relative-time labels key on it.
        assert_ne!(re_emit[0]["payload"]["updatedAt"], SETTLED_AT);
    }

    #[test]
    fn settling_a_snoozed_thread_also_wakes_it() {
        let env = env();
        let events = decide(
            &env,
            settle("cmd-settle-snoozed"),
            &make_read_model(
                None,
                None,
                Value::Null,
                json!([]),
                json!([]),
                json!({"snoozedUntil": "1970-01-02T09:00:00.000Z"}),
            ),
        )
        .unwrap();
        assert_eq!(types(&events), ["thread.settled", "thread.unsnoozed"]);
        let settled = find(&events, "thread.settled");
        let unsnoozed = find(&events, "thread.unsnoozed");
        assert_eq!(unsnoozed["payload"]["reason"], "user");
        assert_eq!(unsnoozed["payload"]["updatedAt"], settled["payload"]["updatedAt"]);
    }

    #[test]
    fn repeated_settle_repairs_legacy_settled_and_snoozed_state() {
        let env = env();
        let events = decide(
            &env,
            settle("cmd-settle-snoozed-again"),
            &make_read_model(
                Some("settled"),
                None,
                Value::Null,
                json!([]),
                json!([]),
                json!({"snoozedUntil": "1970-01-02T09:00:00.000Z"}),
            ),
        )
        .unwrap();
        assert_eq!(types(&events), ["thread.settled", "thread.unsnoozed"]);
        let settled = find(&events, "thread.settled");
        let unsnoozed = find(&events, "thread.unsnoozed");
        assert_eq!(settled["payload"]["settledAt"], SETTLED_AT);
        assert_eq!(settled["payload"]["updatedAt"], NOW);
        assert_ne!(unsnoozed["payload"]["updatedAt"], NOW);
    }

    #[test]
    fn settling_a_pinned_and_snoozed_thread_clears_the_pin_and_snooze() {
        let env = env();
        let events = decide(
            &env,
            settle("cmd-settle-pinned-snoozed"),
            &make_read_model(
                None,
                None,
                Value::Null,
                json!([]),
                json!([]),
                json!({"pinnedAt": SETTLED_AT, "snoozedUntil": "1970-01-02T09:00:00.000Z"}),
            ),
        )
        .unwrap();
        assert_eq!(types(&events), ["thread.settled", "thread.unpinned", "thread.unsnoozed"]);
    }

    #[test]
    fn rejects_settling_a_thread_with_a_live_session() {
        let env = env();
        for status in ["starting", "running"] {
            let error = decide(
                &env,
                settle(&format!("cmd-settle-live-{status}")),
                &make_read_model(None, None, make_session(status), json!([]), json!([]), json!({})),
            )
            .unwrap_err();
            assert_settle_blocked(&error);
        }
        // Stopped/error sessions are settleable — only live work is protected.
        let settled = decide(
            &env,
            settle("cmd-settle-stopped"),
            &make_read_model(None, None, make_session("stopped"), json!([]), json!([]), json!({})),
        )
        .unwrap();
        assert_eq!(settled[0]["type"], "thread.settled");
    }

    fn request_activity(kind: &str, request_id: &str, at: &str) -> Value {
        json!({
            "id": format!("activity-{request_id}-{kind}"),
            "tone": "approval",
            "kind": kind,
            "summary": kind,
            "payload": {"requestId": request_id},
            "turnId": null,
            "createdAt": at,
        })
    }

    #[test]
    fn rejects_settling_a_thread_with_an_open_approval_or_user_input_request() {
        let env = env();
        let with_activities = |activities: Value| make_read_model(None, None, Value::Null, activities, json!([]), json!({}));

        // Open approval request: settle rejected.
        let open_error = decide(
            &env,
            settle("cmd-settle-pending"),
            &with_activities(json!([request_activity("approval.requested", "req-1", NOW)])),
        )
        .unwrap_err();
        assert_settle_blocked(&open_error);

        // Same request later resolved: settleable again.
        let settled = decide(
            &env,
            settle("cmd-settle-resolved"),
            &with_activities(json!([
                request_activity("approval.requested", "req-1", NOW),
                request_activity("approval.resolved", "req-1", NOW),
            ])),
        )
        .unwrap();
        assert_eq!(settled[0]["type"], "thread.settled");

        // Open user-input request: also rejected.
        let input_error = decide(
            &env,
            settle("cmd-settle-pending-input"),
            &with_activities(json!([request_activity("user-input.requested", "req-2", NOW)])),
        )
        .unwrap_err();
        assert_settle_blocked(&input_error);
    }

    fn async_question(request_id: &str) -> Value {
        json!({
            "id": request_id,
            "kind": "user-input.requested",
            "summary": "Question",
            "tone": "approval",
            "turnId": null,
            "createdAt": "1969-12-31T00:00:00.000Z",
            "payload": {"requestId": request_id, "responseMode": "message"},
        })
    }

    #[test]
    fn manual_settlement_dismisses_async_questions_without_starting_a_turn() {
        let env = env();
        let mut answer = async_question("answered");
        merge(
            &mut answer,
            json!({"id": "answer", "createdAt": "1969-12-31T01:00:00.000Z", "kind": "user-input.resolved"}),
        );
        let model = make_read_model(
            None,
            None,
            make_session("ready"),
            json!([async_question("first"), async_question("second"), async_question("answered"), answer]),
            json!([]),
            json!({}),
        );
        let command = settle("settle-async");
        let events = decide(&env, command.clone(), &model).unwrap();
        assert_eq!(types(&events), ["thread.settled", "thread.activity-appended", "thread.activity-appended"]);
        for (event, request_id) in events[1..].iter().zip(["first", "second"]) {
            let payload = event["payload"].as_object().unwrap();
            let mut keys: Vec<&str> = payload.keys().map(String::as_str).collect();
            keys.sort_unstable();
            assert_eq!(keys, ["activity", "threadId"], "payload: {payload:?}");
            assert_eq!(payload["threadId"], "thread-1");
            let activity = &payload["activity"];
            assert_eq!(activity["kind"], "user-input.resolved");
            assert_eq!(activity["summary"], "User input dismissed");
            assert_json_eq(&activity["payload"], &json!({"requestId": request_id, "responseMode": "message"}));
        }
        let mut projected = model;
        apply_decided(&mut projected, &events);
        let thread = first_thread(&projected);
        assert_eq!(thread["settledOverride"], "settled");
        assert_eq!(thread["messages"], json!([]));
        let repeated = decide(&env, command, &projected).unwrap();
        // TS: `expect(repeated).toMatchObject({ type: "thread.settled" })` (a single event).
        assert_eq!(types(&repeated), ["thread.settled"]);
    }

    #[test]
    fn async_questions_do_not_bypass_automatic_settlement_or_other_blockers() {
        let env = env();
        let question = json!({
            "id": "async-question",
            "kind": "user-input.requested",
            "summary": "Question",
            "tone": "approval",
            "turnId": null,
            "createdAt": NOW,
            "payload": {"requestId": "async-question", "responseMode": "message"},
        });
        for blocker in ["auto", "running", "starting", "approval", "native"] {
            let command = if blocker == "auto" {
                json!({
                    "type": "thread.auto-settle",
                    "commandId": format!("settle-{blocker}"),
                    "threadId": "thread-1",
                    "snapshotSequence": 0,
                    "settledAt": NOW,
                })
            } else {
                json!({"type": "thread.settle", "commandId": format!("settle-{blocker}"), "threadId": "thread-1"})
            };
            let mut activities = vec![question.clone()];
            if blocker == "approval" || blocker == "native" {
                let mut blocking = question.clone();
                merge(
                    &mut blocking,
                    json!({
                        "id": "blocking-request",
                        "kind": if blocker == "approval" { "approval.requested" } else { "user-input.requested" },
                        "payload": {"requestId": "blocking-request"},
                    }),
                );
                activities.push(blocking);
            }
            let session = make_session(if blocker == "running" || blocker == "starting" { blocker } else { "ready" });
            let error = decide(&env, command, &make_read_model(None, None, session, json!(activities), json!([]), json!({}))).unwrap_err();
            assert_eq!(tag(&error), "OrchestrationThreadSettleBlockedError", "blocker {blocker}: {error:?}");
        }
    }

    #[test]
    fn clears_an_open_request_when_its_respond_failure_marks_it_stale() {
        let env = env();
        let activity = |kind: &str, request_id: &str, extra: Value| {
            let mut payload = json!({"requestId": request_id});
            merge(&mut payload, extra);
            json!({
                "id": format!("activity-{request_id}-{kind}"),
                "tone": "approval",
                "kind": kind,
                "summary": kind,
                "payload": payload,
                "turnId": null,
                "createdAt": NOW,
            })
        };
        let with_activities = |activities: Value| make_read_model(None, None, Value::Null, activities, json!([]), json!({}));

        // Stale-failure details clear the request, matching the projection flags.
        let settled = decide(
            &env,
            settle("cmd-settle-stale-failed"),
            &with_activities(json!([
                activity("approval.requested", "req-1", json!({})),
                activity(
                    "provider.approval.respond.failed",
                    "req-1",
                    json!({"detail": "Unknown pending approval request req-1"})
                ),
                activity("user-input.requested", "req-2", json!({})),
                activity(
                    "provider.user-input.respond.failed",
                    "req-2",
                    json!({"detail": "stale pending user-input request req-2"})
                ),
            ])),
        )
        .unwrap();
        assert_eq!(settled[0]["type"], "thread.settled");

        // A non-stale respond failure (transient provider error) keeps the
        // request open: the user can retry, so it is still blocked-on-you.
        let still_open = decide(
            &env,
            settle("cmd-settle-transient-failed"),
            &with_activities(json!([
                activity("approval.requested", "req-3", json!({})),
                activity("provider.approval.respond.failed", "req-3", json!({"detail": "provider connection reset"})),
            ])),
        )
        .unwrap_err();
        assert_settle_blocked(&still_open);
    }

    #[test]
    fn bounds_the_queued_turn_grace_window_against_client_clock_skew() {
        let env = env();
        let user_message = |created_at: &str| {
            json!([{
                "id": "message-queued",
                "role": "user",
                "text": "Continue",
                "turnId": null,
                "streaming": false,
                "createdAt": created_at,
                "updatedAt": created_at,
            }])
        };

        // The decider's clock is pinned to the epoch: timestamps here are relative to
        // 1970-01-01T00:00:00.000Z.

        // Within the grace window: genuinely queued, settle rejected.
        let queued_error = decide(
            &env,
            settle("cmd-settle-queued"),
            &make_read_model(None, None, Value::Null, json!([]), user_message("1969-12-31T23:59:30.000Z"), json!({})),
        )
        .unwrap_err();
        assert_settle_blocked(&queued_error);

        // Message timestamp far in the FUTURE (client clock ahead of server):
        // a negative age must not read as queued forever — past the grace
        // bound in either direction the thread is settleable.
        let skewed = decide(
            &env,
            settle("cmd-settle-skewed"),
            &make_read_model(None, None, Value::Null, json!([]), user_message("1970-01-01T01:00:00.000Z"), json!({})),
        )
        .unwrap();
        assert_eq!(skewed[0]["type"], "thread.settled");
    }

    #[test]
    fn rejects_settling_and_unsettling_archived_threads() {
        let env = env();
        let settle_error = decide(
            &env,
            settle("cmd-settle-archived"),
            &make_read_model(None, Some(NOW), Value::Null, json!([]), json!([]), json!({})),
        )
        .unwrap_err();
        assert_eq!(tag(&settle_error), "OrchestrationCommandInvariantError");

        let unsettle_error = decide(
            &env,
            json!({"type": "thread.unsettle", "commandId": "cmd-unsettle-archived", "threadId": "thread-1", "reason": "user"}),
            &make_read_model(Some("settled"), Some(NOW), Value::Null, json!([]), json!([]), json!({})),
        )
        .unwrap_err();
        assert_eq!(tag(&unsettle_error), "OrchestrationCommandInvariantError");
    }

    #[test]
    fn maps_unsettle_reasons_to_overrides_and_re_emits_idempotently() {
        let env = env();
        let user_events = decide(
            &env,
            json!({"type": "thread.unsettle", "commandId": "cmd-unsettle-user", "threadId": "thread-1", "reason": "user"}),
            &simple_model(Some("settled")),
        )
        .unwrap();
        assert_eq!(user_events.len(), 1);
        assert_eq!(user_events[0]["type"], "thread.unsettled");
        assert_eq!(user_events[0]["payload"]["reason"], "user");

        // Re-dispatching against the already-reached state re-emits rather than
        // producing zero events (the engine rejects empty commands).
        let user_again = decide(
            &env,
            json!({"type": "thread.unsettle", "commandId": "cmd-unsettle-user-again", "threadId": "thread-1", "reason": "user"}),
            &simple_model(Some("active")),
        )
        .unwrap();
        assert_eq!(user_again.len(), 1);
        assert_eq!(user_again[0]["type"], "thread.unsettled");
    }

    // Command-to-projection: an accepted un-settle must land as the re-entry stamp clients sort
    // by, so the thread surfaces above threads created after it.
    #[test]
    fn an_accepted_un_settle_re_anchors_the_thread_for_the_active_list() {
        let env = env();
        let model = simple_model(Some("settled"));
        let events = decide(
            &env,
            json!({"type": "thread.unsettle", "commandId": "cmd-unsettle-anchor", "threadId": "thread-1", "reason": "user"}),
            &model,
        )
        .unwrap();
        let unsettled = events[0].clone();
        assert_eq!(unsettled["type"], "thread.unsettled");

        let mut projected = model;
        apply_decided(&mut projected, std::slice::from_ref(&unsettled));
        let thread = first_thread(&projected);
        assert_eq!(thread["settledOverride"], "active");
        // The stamp is the decider's accept time: every thread created before
        // the un-settle anchors below it.
        assert_eq!(thread["unsettledAt"], unsettled["occurredAt"]);
        assert_eq!(thread["unsettledAt"], unsettled["payload"]["updatedAt"]);
    }

    fn turn_start(command_id: &str, message_id: &str) -> Value {
        json!({
            "type": "thread.turn.start",
            "commandId": command_id,
            "threadId": "thread-1",
            "message": {"messageId": message_id, "role": "user", "text": "Continue", "attachments": []},
            "runtimeMode": "full-access",
            "interactionMode": "default",
            "createdAt": NOW,
        })
    }

    fn approval_append(command_id: &str, activity_id: &str) -> Value {
        json!({
            "type": "thread.activity.append",
            "commandId": command_id,
            "threadId": "thread-1",
            "activity": {
                "id": activity_id,
                "tone": "approval",
                "kind": "approval.requested",
                "summary": "Command approval requested",
                "payload": null,
                "turnId": null,
                "createdAt": NOW,
            },
            "createdAt": NOW,
        })
    }

    fn session_set(command_id: &str, status: &str) -> Value {
        json!({
            "type": "thread.session.set",
            "commandId": command_id,
            "threadId": "thread-1",
            "session": make_session(status),
            "createdAt": NOW,
        })
    }

    #[test]
    fn prepends_activity_unsets_for_turn_starts_and_live_session_updates() {
        let env = env();
        let turn_events = decide(&env, turn_start("cmd-turn-start", "message-1"), &simple_model(Some("settled"))).unwrap();
        assert_eq!(types(&turn_events), ["thread.unsettled", "thread.message-sent", "thread.turn-start-requested"]);

        // A keep-active pin is also an override: real activity clears it
        // back to neutral so auto-settle can apply again later.
        let session_events = decide(&env, session_set("cmd-session-set", "running"), &simple_model(Some("active"))).unwrap();
        assert_eq!(types(&session_events), ["thread.unsettled", "thread.session-set"]);
    }

    #[test]
    fn clears_a_keep_active_pin_on_real_activity() {
        let env = env();
        let turn_events = decide(&env, turn_start("cmd-active-turn-start", "message-active"), &simple_model(Some("active"))).unwrap();
        // The pin exists to suppress AUTO-settle, not to survive real work:
        // activity resets it to neutral, restoring the default lifecycle.
        assert_eq!(types(&turn_events), ["thread.unsettled", "thread.message-sent", "thread.turn-start-requested"]);

        let activity_events = decide(&env, approval_append("cmd-active-approval", "activity-active"), &simple_model(Some("active"))).unwrap();
        assert_eq!(types(&activity_events), ["thread.unsettled", "thread.activity-appended"]);
    }

    #[test]
    fn does_not_unsettle_for_session_stop_error_status_writes() {
        let env = env();
        for status in ["stopped", "error", "ready", "idle"] {
            let events = decide(&env, session_set(&format!("cmd-session-{status}"), status), &simple_model(Some("settled"))).unwrap();
            assert_eq!(types(&events), ["thread.session-set"], "status {status}");
        }
    }

    #[test]
    fn unsettles_for_approval_and_user_input_activities_but_not_others() {
        let env = env();
        let approval_events = decide(&env, approval_append("cmd-activity-approval", "activity-1"), &simple_model(Some("settled"))).unwrap();
        assert_eq!(types(&approval_events), ["thread.unsettled", "thread.activity-appended"]);

        let routine_events = decide(
            &env,
            json!({
                "type": "thread.activity.append",
                "commandId": "cmd-activity-routine",
                "threadId": "thread-1",
                "activity": {
                    "id": "activity-2",
                    "tone": "info",
                    "kind": "tool.completed",
                    "summary": "Tool completed",
                    "payload": null,
                    "turnId": null,
                    "createdAt": NOW,
                },
                "createdAt": NOW,
            }),
            &simple_model(Some("settled")),
        )
        .unwrap();
        assert_eq!(types(&routine_events), ["thread.activity-appended"]);
    }

    #[test]
    fn drops_an_only_if_settled_session_stop_when_the_thread_was_re_engaged() {
        let env = env();
        let stop_command = |command_id: &str| {
            json!({
                "type": "thread.session.stop",
                "commandId": command_id,
                "threadId": "thread-1",
                "createdAt": NOW,
                "onlyIfSettled": true,
            })
        };
        let with_session =
            |settled_override: Option<&str>, status: &str| make_read_model(settled_override, None, make_session(status), json!([]), json!([]), json!({}));

        // Still settled with an idle session: the cleanup stop goes through.
        let stopped = decide(&env, stop_command("cmd-stop-settled-idle"), &with_session(Some("settled"), "ready")).unwrap();
        assert_eq!(types(&stopped), ["thread.session-stop-requested"]);

        // Re-engaged before the stop was decided (a turn start unsettles the
        // thread): the stale cleanup stop must not kill the new session.
        let unsettled_error = decide(&env, stop_command("cmd-stop-unsettled"), &with_session(None, "starting")).unwrap_err();
        assert_eq!(tag(&unsettled_error), "OrchestrationCommandInvariantError");

        // Still settled but the session is already coming alive: same drop.
        let alive_error = decide(&env, stop_command("cmd-stop-session-alive"), &with_session(Some("settled"), "starting")).unwrap_err();
        assert_eq!(tag(&alive_error), "OrchestrationCommandInvariantError");

        // Without the flag the stop stays unconditional (archive, stop button).
        let unconditional = decide(
            &env,
            json!({
                "type": "thread.session.stop",
                "commandId": "cmd-stop-unconditional",
                "threadId": "thread-1",
                "createdAt": NOW,
            }),
            &with_session(None, "starting"),
        )
        .unwrap();
        assert_eq!(types(&unconditional), ["thread.session-stop-requested"]);
    }
}

// ---------------------------------------------------------------------------------------------
mod snoozed {
    //! `decider.snoozed.test.ts` ("snoozed thread decider").

    use super::*;

    const NOW: &str = "2026-01-01T00:00:00.000Z";
    // The decider's clock is pinned to the epoch, so "future" wake times are relative to
    // 1970-01-01T00:00:00.000Z.
    const FUTURE_WAKE: &str = "1970-01-02T09:00:00.000Z";
    const PAST_WAKE: &str = "1969-12-31T09:00:00.000Z";
    const SNOOZED_AT: &str = "1969-12-30T00:00:00.000Z";

    /// `makeReadModel({ snoozedUntil, snoozedAt, archivedAt, activities, messages })`.
    fn make_read_model(input: Value) -> zc_contracts::OrchestrationReadModel {
        let get = |key: &str| input.get(key).cloned().unwrap_or(Value::Null);
        let snoozed_until = get("snoozedUntil");
        let snoozed_at = match input.get("snoozedAt") {
            Some(value) if !value.is_null() => value.clone(),
            _ if !snoozed_until.is_null() => json!(SNOOZED_AT),
            _ => Value::Null,
        };
        model_with_thread(
            thread_json(
                "thread-1",
                "project-1",
                NOW,
                json!({
                    "archivedAt": get("archivedAt"),
                    "snoozedUntil": snoozed_until,
                    "snoozedAt": snoozed_at,
                    "messages": input.get("messages").cloned().unwrap_or(json!([])),
                    "activities": input.get("activities").cloned().unwrap_or(json!([])),
                }),
            ),
            json!([]),
            NOW,
        )
    }

    fn snooze(command_id: &str, snoozed_until: &str) -> Value {
        json!({"type": "thread.snooze", "commandId": command_id, "threadId": "thread-1", "snoozedUntil": snoozed_until})
    }

    fn unsnooze(command_id: &str) -> Value {
        json!({"type": "thread.unsnooze", "commandId": command_id, "threadId": "thread-1", "reason": "user"})
    }

    #[test]
    fn snoozes_a_thread_to_a_future_wake_time() {
        let env = env();
        let events = decide(&env, snooze("cmd-snooze", FUTURE_WAKE), &make_read_model(json!({}))).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "thread.snoozed");
        assert_eq!(events[0]["payload"]["snoozedUntil"], FUTURE_WAKE);
        assert_eq!(events[0]["payload"]["snoozedAt"], events[0]["payload"]["updatedAt"]);
    }

    #[test]
    fn rejects_a_wake_time_that_is_not_in_the_future() {
        let env = env();
        let error = decide(&env, snooze("cmd-snooze-past", PAST_WAKE), &make_read_model(json!({}))).unwrap_err();
        assert_eq!(tag(&error), "OrchestrationCommandInvariantError");
    }

    #[test]
    fn rejects_an_unparseable_wake_time() {
        // IsoDateTime is structurally a string, so garbage can reach the
        // decider; a NaN wake time must never persist as snooze state.
        let env = env();
        let error = decide(&env, snooze("cmd-snooze-garbage", "not-a-date"), &make_read_model(json!({}))).unwrap_err();
        assert_eq!(tag(&error), "OrchestrationCommandInvariantError");
    }

    #[test]
    fn rejects_snoozing_blocked_on_you_work() {
        let env = env();
        let request_activity = json!({
            "id": "activity-req-1",
            "tone": "approval",
            "kind": "approval.requested",
            "summary": "approval.requested",
            "payload": {"requestId": "req-1"},
            "turnId": null,
            "createdAt": NOW,
        });
        let error = decide(
            &env,
            snooze("cmd-snooze-blocked", FUTURE_WAKE),
            &make_read_model(json!({"activities": [request_activity]})),
        )
        .unwrap_err();
        assert_eq!(tag(&error), "OrchestrationCommandInvariantError");
    }

    #[test]
    fn re_emits_idempotently_for_a_duplicate_snooze_to_the_same_wake_time() {
        let env = env();
        let events = decide(
            &env,
            snooze("cmd-snooze-again", FUTURE_WAKE),
            &make_read_model(json!({"snoozedUntil": FUTURE_WAKE})),
        )
        .unwrap();
        assert_eq!(events.len(), 1);
        // TS guards the payload checks with `type === "thread.snoozed"`; asserted here.
        assert_eq!(events[0]["type"], "thread.snoozed");
        // Original snoozedAt preserved; updatedAt must not churn.
        assert_eq!(events[0]["payload"]["snoozedAt"], SNOOZED_AT);
        assert_eq!(events[0]["payload"]["updatedAt"], NOW);
    }

    #[test]
    fn re_snoozing_to_a_different_wake_time_stamps_fresh() {
        let env = env();
        let events = decide(
            &env,
            snooze("cmd-snooze-extend", "1970-01-03T09:00:00.000Z"),
            &make_read_model(json!({"snoozedUntil": FUTURE_WAKE})),
        )
        .unwrap();
        // TS guards the payload checks with `type === "thread.snoozed"`; asserted here.
        assert_eq!(events[0]["type"], "thread.snoozed");
        assert_eq!(events[0]["payload"]["snoozedUntil"], "1970-01-03T09:00:00.000Z");
        assert_ne!(events[0]["payload"]["updatedAt"], NOW);
    }

    #[test]
    fn unsnoozes_with_reason_user_and_re_emits_idempotently_when_awake() {
        let env = env();
        let events = decide(&env, unsnooze("cmd-unsnooze"), &make_read_model(json!({"snoozedUntil": FUTURE_WAKE}))).unwrap();
        assert_eq!(events[0]["type"], "thread.unsnoozed");
        assert_eq!(events[0]["payload"]["reason"], "user");
        assert_ne!(events[0]["payload"]["updatedAt"], NOW);

        let awake = decide(&env, unsnooze("cmd-unsnooze-awake"), &make_read_model(json!({}))).unwrap();
        assert_eq!(awake[0]["type"], "thread.unsnoozed");
        // No state change — keep the existing updatedAt.
        assert_eq!(awake[0]["payload"]["updatedAt"], NOW);
    }

    #[test]
    fn rejects_snoozing_a_thread_with_a_queued_turn_start() {
        // A user message 30s before the (epoch) decider clock with no adopting turn is queued
        // work.
        let env = env();
        let queued_message = json!({
            "id": "message-queued",
            "role": "user",
            "text": "Continue",
            "turnId": null,
            "streaming": false,
            "createdAt": "1969-12-31T23:59:30.000Z",
            "updatedAt": "1969-12-31T23:59:30.000Z",
        });
        let error = decide(
            &env,
            snooze("cmd-snooze-queued", FUTURE_WAKE),
            &make_read_model(json!({"messages": [queued_message]})),
        )
        .unwrap_err();
        assert_eq!(tag(&error), "OrchestrationCommandInvariantError");
    }

    #[test]
    fn rejects_snoozing_an_archived_thread() {
        let env = env();
        let error = decide(&env, snooze("cmd-snooze-archived", FUTURE_WAKE), &make_read_model(json!({"archivedAt": NOW}))).unwrap_err();
        assert_eq!(tag(&error), "OrchestrationCommandInvariantError");
    }

    #[test]
    fn a_user_message_spends_the_snooze_return_ticket_activity_wake() {
        let env = env();
        let events = decide(
            &env,
            json!({
                "type": "thread.turn.start",
                "commandId": "cmd-turn-start",
                "threadId": "thread-1",
                "message": {"messageId": "message-1", "role": "user", "text": "Continue", "attachments": []},
                "runtimeMode": "full-access",
                "interactionMode": "default",
                "createdAt": NOW,
            }),
            &make_read_model(json!({"snoozedUntil": FUTURE_WAKE})),
        )
        .unwrap();
        let unsnoozed = events
            .iter()
            .find(|event| event["type"] == "thread.unsnoozed")
            .unwrap_or_else(|| panic!("no thread.unsnoozed in {events:#?}"));
        assert_eq!(unsnoozed["payload"]["reason"], "activity");
    }
}

// ---------------------------------------------------------------------------------------------
mod title_regeneration {
    //! `decider.titleRegeneration.test.ts` ("title regeneration decider").

    use super::*;

    const UPDATED_AT: &str = "2026-01-01T00:00:00.000Z";

    fn model(overrides: Value) -> zc_contracts::OrchestrationReadModel {
        let mut thread_overrides = json!({"title": "Manual title", "snoozedUntil": null, "snoozedAt": null});
        merge(&mut thread_overrides, overrides);
        model_with_thread(thread_json("thread-1", "project-1", UPDATED_AT, thread_overrides), json!([]), UPDATED_AT)
    }

    #[test]
    fn preserves_updated_at_for_a_stale_completion() {
        let env = env();
        let events = decide(
            &env,
            json!({
                "type": "thread.title.regeneration.complete",
                "commandId": "cmd-regeneration-complete",
                "threadId": "thread-1",
                "requestId": "cmd-old-regeneration-request",
                "title": "Generated title",
            }),
            &model(json!({})),
        )
        .unwrap();
        let event = &events[0];
        assert_eq!(event["type"], "thread.meta-updated");
        assert_eq!(event["payload"], json!({"threadId": "thread-1", "updatedAt": UPDATED_AT}));
    }

    #[test]
    fn rejects_an_initial_result_after_a_manual_rename_to_the_same_text() {
        let env = env();
        let events = decide(
            &env,
            json!({
                "type": "thread.title.generate.complete",
                "commandId": "generated",
                "threadId": "thread-1",
                "expectedTitle": "Manual title",
                "expectedVersion": null,
                "title": "Automatic title",
                "needsRefinement": true,
            }),
            &model(json!({"titleState": {"source": "manual", "version": "manual", "needsRefinement": false}})),
        )
        .unwrap();
        assert_eq!(events[0]["payload"], json!({"threadId": "thread-1", "updatedAt": UPDATED_AT}));
    }

    #[test]
    fn records_manual_ownership_even_when_the_title_text_does_not_change() {
        let env = env();
        let events = decide(
            &env,
            json!({
                "type": "thread.meta.update",
                "commandId": "manual-rename",
                "threadId": "thread-1",
                "title": "Manual title",
            }),
            &model(json!({})),
        )
        .unwrap();
        assert_match(
            &events[0]["payload"],
            &json!({"titleState": {"source": "manual", "version": "manual-rename", "needsRefinement": false}}),
        );
    }
}

// ---------------------------------------------------------------------------------------------
mod turn_diff_complete {
    //! `decider.turnDiffComplete.test.ts` ("turn diff complete decider").

    use super::*;

    const NOW: &str = "2026-01-01T00:00:00.000Z";

    fn make_read_model(checkpoints: Value) -> zc_contracts::OrchestrationReadModel {
        model_with_thread(
            thread_json(
                "thread-1",
                "project-1",
                NOW,
                json!({
                    "snoozedUntil": null,
                    "snoozedAt": null,
                    "pinnedAt": null,
                    "pinOrderKey": null,
                    "checkpoints": checkpoints,
                }),
            ),
            json!([]),
            NOW,
        )
    }

    fn make_checkpoint(status: &str) -> Value {
        json!({
            "turnId": "turn-1",
            "checkpointTurnCount": 1,
            "checkpointRef": format!("existing:{status}"),
            "status": status,
            "files": [],
            "assistantMessageId": null,
            "completedAt": NOW,
        })
    }

    fn placeholder_command() -> Value {
        json!({
            "type": "thread.turn.diff.complete",
            "commandId": "cmd-diff-placeholder",
            "threadId": "thread-1",
            "turnId": "turn-1",
            "completedAt": NOW,
            "checkpointRef": "provider-diff:event-1",
            "status": "missing",
            "files": [],
            "assistantMessageId": "assistant:turn-1",
            "checkpointTurnCount": 2,
            "createdAt": NOW,
        })
    }

    #[test]
    fn rejects_a_placeholder_when_the_turn_already_has_a_captured_checkpoint() {
        let env = env();
        let result = decide(&env, placeholder_command(), &make_read_model(json!([make_checkpoint("ready")])));
        assert!(result.is_err(), "expected a failure, got {result:?}");
    }

    #[test]
    fn accepts_a_placeholder_when_the_turn_only_has_a_placeholder() {
        let env = env();
        let events = decide(&env, placeholder_command(), &make_read_model(json!([make_checkpoint("missing")]))).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "thread.turn-diff-completed");
    }

    #[test]
    fn lets_a_captured_checkpoint_replace_an_earlier_placeholder() {
        let env = env();
        let mut command = placeholder_command();
        merge(&mut command, json!({"checkpointRef": "refs/t3/checkpoints/turn-1", "status": "ready"}));
        let events = decide(&env, command, &make_read_model(json!([make_checkpoint("missing")]))).unwrap();
        assert_eq!(events[0]["type"], "thread.turn-diff-completed");
    }
}

// ---------------------------------------------------------------------------------------------
mod user_input_dismiss {
    //! `decider.userInputDismiss.test.ts` ("user input dismiss decider").

    use super::*;
    use zc_orchestration::errors::CommandRejection;

    const NOW: &str = "2026-01-01T00:00:00.000Z";
    const REQUEST_ID: &str = "question-1";

    fn make_request(response_mode: Option<&str>) -> Value {
        let mut payload = json!({
            "requestId": REQUEST_ID,
            "questions": [{"id": "0", "header": "Q", "question": "Continue?", "options": []}],
        });
        if let Some(mode) = response_mode {
            payload["responseMode"] = json!(mode);
        }
        json!({
            "id": REQUEST_ID,
            "kind": "user-input.requested",
            "summary": "Question",
            "tone": "approval",
            "turnId": null,
            "createdAt": NOW,
            "payload": payload,
        })
    }

    fn make_read_model(activities: Value) -> zc_contracts::OrchestrationReadModel {
        model_with_thread(
            thread_json(
                "thread-1",
                "project-1",
                NOW,
                json!({"snoozedUntil": null, "snoozedAt": null, "pinnedAt": null, "activities": activities}),
            ),
            json!([]),
            NOW,
        )
    }

    fn command_json() -> Value {
        json!({
            "type": "thread.user-input.dismiss",
            "commandId": "dismiss-1",
            "threadId": "thread-1",
            "requestId": REQUEST_ID,
            "createdAt": NOW,
        })
    }

    #[track_caller]
    fn assert_invariant_detail(error: &CommandRejection, expected: &str) {
        assert_eq!(tag(error), "OrchestrationCommandInvariantError", "{error:?}");
        match error {
            CommandRejection::Invariant { detail, .. } => assert_eq!(detail, expected),
            other => panic!("expected an invariant rejection, got {other:?}"),
        }
    }

    #[test]
    fn closes_an_async_question_without_sending_a_message_or_starting_a_turn() {
        let env = env();
        let request = make_request(Some("message"));
        let model = make_read_model(json!([request]));
        let events = decide_with(&env, command_json(), &model, Some(&activity(request))).unwrap();
        assert_eq!(types(&events), ["thread.activity-appended"]);
        assert_match(
            &events[0]["payload"],
            &json!({
                "threadId": "thread-1",
                "activity": {
                    "kind": "user-input.resolved",
                    "summary": "User input dismissed",
                    "payload": {"requestId": REQUEST_ID, "responseMode": "message"},
                },
            }),
        );
        let mut projected = model;
        apply_decided(&mut projected, &events[..1]);
        let thread = first_thread(&projected);
        assert_eq!(thread["messages"], json!([]));
        assert_eq!(thread.get("latestTurn"), Some(&Value::Null));
    }

    #[test]
    fn rejects_dismissing_a_native_callback_question() {
        let env = env();
        let request = make_request(None);
        let error = decide_with(&env, command_json(), &make_read_model(json!([request])), Some(&activity(request))).unwrap_err();
        assert_invariant_detail(&error, "This question needs an answer. Answer it or stop the turn.");
    }

    #[test]
    fn rejects_dismissing_a_question_that_was_already_resolved() {
        let env = env();
        let mut resolved = make_request(Some("message"));
        merge(&mut resolved, json!({"id": "resolved", "kind": "user-input.resolved"}));
        let error = decide_with(
            &env,
            command_json(),
            &make_read_model(json!([make_request(Some("message")), resolved])),
            Some(&activity(resolved)),
        )
        .unwrap_err();
        assert_invariant_detail(&error, "This question has already been answered.");
    }
}

// ---------------------------------------------------------------------------------------------
mod user_message_append {
    //! `decider.userMessageAppend.test.ts` ("thread.message.user.append").

    use super::*;

    const CREATED_AT: &str = "2026-08-24T10:00:00.000Z";
    const PROJECT_ID: &str = "project-1";
    const THREAD_ID: &str = "thread-bootstrap";
    const MESSAGE_ID: &str = "message-bootstrap";

    fn read_model_with_thread() -> zc_contracts::OrchestrationReadModel {
        let mut model = empty_model(CREATED_AT);
        apply(
            &mut model,
            json!({
                "sequence": 1,
                "eventId": "event-project-created",
                "aggregateKind": "project",
                "aggregateId": PROJECT_ID,
                "type": "project.created",
                "occurredAt": CREATED_AT,
                "commandId": "command-project-created",
                "causationEventId": null,
                "correlationId": "command-project-created",
                "metadata": {},
                "payload": {
                    "projectId": PROJECT_ID,
                    "title": "Project",
                    "workspaceRoot": "/tmp/project",
                    "defaultModelSelection": null,
                    "scripts": [],
                    "createdAt": CREATED_AT,
                    "updatedAt": CREATED_AT,
                },
            }),
        );
        apply(
            &mut model,
            json!({
                "sequence": 2,
                "eventId": "event-thread-created",
                "aggregateKind": "thread",
                "aggregateId": THREAD_ID,
                "type": "thread.created",
                "occurredAt": CREATED_AT,
                "commandId": "command-thread-created",
                "causationEventId": null,
                "correlationId": "command-thread-created",
                "metadata": {},
                "payload": {
                    "threadId": THREAD_ID,
                    "projectId": PROJECT_ID,
                    "title": "Bootstrap thread",
                    "modelSelection": {"instanceId": "codex", "model": "gpt-5"},
                    "runtimeMode": "full-access",
                    "interactionMode": "default",
                    "branch": null,
                    "worktreePath": null,
                    "createdAt": CREATED_AT,
                    "updatedAt": CREATED_AT,
                },
            }),
        );
        model
    }

    fn append_command() -> Value {
        json!({
            "type": "thread.message.user.append",
            "commandId": "command-append",
            "threadId": THREAD_ID,
            "message": {"messageId": MESSAGE_ID, "text": "Build it", "attachments": []},
            "createdAt": CREATED_AT,
        })
    }

    fn turn_start_command() -> Value {
        json!({
            "type": "thread.turn.start",
            "commandId": "command-turn-start",
            "threadId": THREAD_ID,
            "message": {"messageId": MESSAGE_ID, "role": "user", "text": "Build it", "attachments": []},
            "runtimeMode": "full-access",
            "interactionMode": "default",
            "createdAt": CREATED_AT,
        })
    }

    #[test]
    fn persists_a_user_message_without_a_turn_tagged_as_deferred() {
        let env = env();
        let events = decide(&env, append_command(), &read_model_with_thread()).unwrap();
        assert_eq!(types(&events), ["thread.message-sent"]);
        assert_eq!(events[0]["metadata"]["deferredTurn"], true);
        assert_match(&events[0]["payload"], &json!({"messageId": MESSAGE_ID, "role": "user", "turnId": null}));
    }

    #[test]
    fn rejects_a_message_id_that_already_exists_on_the_thread() {
        let env = env();
        let mut model = read_model_with_thread();
        let first = decide(&env, append_command(), &model).unwrap();
        apply_decided(&mut model, &first[..1]);
        assert_eq!(model.snapshot_sequence, 3);
        let error = decide(&env, append_command(), &model).unwrap_err();
        assert_eq!(tag(&error), "OrchestrationCommandInvariantError");
        assert!(error.to_string().contains("already exists"), "message: {error}");
    }

    #[test]
    fn lets_the_following_turn_start_reference_the_message_instead_of_re_sending_it() {
        let env = env();
        let model = read_model_with_thread();
        let appended = decide(&env, append_command(), &model).unwrap();
        let mut with_message = model.clone();
        apply_decided(&mut with_message, &appended[..1]);

        let events = decide(&env, turn_start_command(), &with_message).unwrap();
        assert_eq!(types(&events), ["thread.turn-start-requested"]);
        assert_match(&events[0]["payload"], &json!({"messageId": MESSAGE_ID}));

        // Without the append the turn start still carries the message itself.
        let direct = decide(&env, turn_start_command(), &model).unwrap();
        assert_eq!(types(&direct), ["thread.message-sent", "thread.turn-start-requested"]);
    }
}

// ---------------------------------------------------------------------------------------------
mod message_context {
    //! `messageContext.test.ts` ("message context plumbing").

    use super::*;

    const NOW: &str = "2026-01-01T00:00:00.000Z";

    fn context() -> Value {
        json!({
            "version": 1,
            "records": [{
                "version": 1,
                "contextId": "ctx_1",
                "kind": "skill",
                "label": "$pinchtab",
                "name": "pinchtab",
            }],
        })
    }

    fn make_read_model() -> zc_contracts::OrchestrationReadModel {
        model_with_thread(
            thread_json("thread-1", "project-1", NOW, json!({"snoozedUntil": null, "snoozedAt": null, "pinnedAt": null})),
            json!([]),
            NOW,
        )
    }

    /// `makeEvent(sequence, type, payload)` (thread aggregate `thread-1`).
    fn make_thread_event(sequence: i64, event_type: &str, payload: Value) -> Value {
        make_event(sequence, event_type, NOW, "thread-1", payload)
    }

    #[test]
    fn carries_context_records_from_turn_start_into_the_message_sent_event() {
        let env = env();
        let events = decide(
            &env,
            json!({
                "type": "thread.turn.start",
                "commandId": "cmd-turn-start",
                "threadId": "thread-1",
                "message": {
                    "messageId": "message-1",
                    "role": "user",
                    "text": "Use [$pinchtab](t3-context://v1/skill/ctx_1)",
                    "attachments": [],
                    "context": context(),
                },
                "runtimeMode": "full-access",
                "interactionMode": "default",
                "createdAt": NOW,
            }),
            &make_read_model(),
        )
        .unwrap();
        let sent = events
            .iter()
            .find(|event| event["type"] == "thread.message-sent")
            .unwrap_or_else(|| panic!("no thread.message-sent in {events:#?}"));
        assert_json_eq(&sent["payload"]["context"], &context());
    }

    #[test]
    fn projects_context_records_onto_the_read_model_message() {
        let mut model = empty_model(NOW);
        // TS uses the legacy `modelSelection: {provider, model}` shape; Rust reads the canonical
        // `{instanceId, model}` one.
        apply(
            &mut model,
            make_thread_event(
                1,
                "thread.created",
                json!({
                    "threadId": "thread-1",
                    "projectId": "project-1",
                    "title": "demo",
                    "modelSelection": {"instanceId": "codex", "model": "gpt-5.4"},
                    "runtimeMode": "full-access",
                    "branch": null,
                    "worktreePath": null,
                    "createdAt": NOW,
                    "updatedAt": NOW,
                }),
            ),
        );
        apply(
            &mut model,
            make_thread_event(
                2,
                "thread.message-sent",
                json!({
                    "threadId": "thread-1",
                    "messageId": "message-1",
                    "role": "user",
                    "text": "Use [$pinchtab](t3-context://v1/skill/ctx_1)",
                    "attachments": [],
                    "context": context(),
                    "turnId": null,
                    "streaming": false,
                    "createdAt": NOW,
                    "updatedAt": NOW,
                }),
            ),
        );
        let message = &first_thread(&model)["messages"][0];
        assert_json_eq(&message["context"], &context());

        // A later non-streaming update without context keeps the original records.
        apply(
            &mut model,
            make_thread_event(
                3,
                "thread.message-sent",
                json!({
                    "threadId": "thread-1",
                    "messageId": "message-1",
                    "role": "user",
                    "text": "edited",
                    "turnId": null,
                    "streaming": false,
                    "createdAt": NOW,
                    "updatedAt": NOW,
                }),
            ),
        );
        assert_json_eq(&first_thread(&model)["messages"][0]["context"], &context());
    }
}
