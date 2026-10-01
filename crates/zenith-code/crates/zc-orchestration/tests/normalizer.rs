//! Ports of the normalizer and attachment-store tests:
//! - `orchestration/Normalizer.test.ts` ([`normalizer_timestamps`])
//! - `orchestration/Normalizer.attachments.test.ts` ([`normalizer_attachments`])
//! - `attachmentStore.test.ts` ([`attachment_store`])
//! - `imageMime.test.ts` ([`image_mime`]; every case there targets `parseBase64DataUrl` or
//!   `inferImageExtension`)
//!
//! The TS attachment tests run on `ServerConfig.layerTest`, whose attachments directory is
//! `<tempStateDir>/attachments`; here each test makes its own temp directory with an
//! `attachments` folder in it.

mod common;

use std::path::{Path, PathBuf};

use common::*;
use serde_json::{json, Value};
use zc_contracts::{ClientOrchestrationCommand, OrchestrationCommand, OrchestrationDispatchCommandError};
use zc_orchestration::normalizer::{canonicalize_client_command_timestamps, cleanup_failed_uploaded_attachments, normalize_dispatch_command};

/// A temp state dir and its `attachments` folder (removed on drop).
struct AttachmentsDir {
    _root: tempfile::TempDir,
    dir: PathBuf,
}

impl AttachmentsDir {
    fn new() -> Self {
        let root = tempfile::Builder::new()
            .prefix("zc-normalizer-attachments-")
            .tempdir()
            .expect("create a temp dir");
        let dir = root.path().join("attachments");
        std::fs::create_dir_all(&dir).expect("create the attachments dir");
        Self { _root: root, dir }
    }

    fn path(&self, file_name: &str) -> PathBuf {
        self.dir.join(file_name)
    }

    /// `NodeFS.readdirSync(attachmentsDir)` (sorted).
    fn entries(&self) -> Vec<String> {
        list_dir(&self.dir)
    }
}

fn list_dir(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read the attachments dir")
        .map(|entry| entry.expect("dir entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn client(value: &Value) -> ClientOrchestrationCommand {
    decode(value.clone())
}

/// The server receipt time `normalizeDispatchCommand` reads from `DateTime.now`.
const RECEIVED_AT: &str = "2026-08-01T00:00:05.000Z";

fn normalize(dir: &Path, command: &Value) -> Result<OrchestrationCommand, OrchestrationDispatchCommandError> {
    normalize_dispatch_command(client(command), RECEIVED_AT, dir)
}

fn cleanup(dir: &Path, command: &Value, normalized: &OrchestrationCommand) {
    cleanup_failed_uploaded_attachments(&client(command), normalized, dir);
}

fn command_type(command: &OrchestrationCommand) -> String {
    to_json(command)["type"].as_str().unwrap_or_default().to_owned()
}

/// `normalized.message.attachments` of a `thread.turn.start` command, as wire JSON.
fn turn_attachments(normalized: &OrchestrationCommand) -> Vec<Value> {
    let value = to_json(normalized);
    assert_eq!(value["type"], "thread.turn.start", "Expected a thread.turn.start command.");
    value["message"]["attachments"].as_array().cloned().unwrap_or_default()
}

fn id_of(attachment: &Value) -> String {
    attachment["id"].as_str().expect("attachment id").to_owned()
}

mod normalizer_timestamps {
    //! `Normalizer.test.ts` (`canonicalizeClientCommandTimestamps`).
    use super::*;

    const CLIENT_CREATED_AT: &str = "2031-01-01T00:00:00.000Z";
    const SERVER_RECEIVED_AT: &str = "2026-07-18T00:00:00.000Z";

    #[test]
    fn replaces_a_client_command_timestamp_with_the_server_receipt_timestamp() {
        let command = json!({
            "type": "project.create",
            "commandId": "command-1",
            "projectId": "project-1",
            "title": "Clock-safe project",
            "workspaceRoot": "/tmp/clock-safe-project",
            "createdAt": CLIENT_CREATED_AT,
        });
        let mut expected = command.clone();
        expected["createdAt"] = json!(SERVER_RECEIVED_AT);

        let result = to_json(&canonicalize_client_command_timestamps(client(&command), SERVER_RECEIVED_AT));
        let diff = json_diff(&expected, &result, 20);
        assert!(diff.is_empty(), "{diff:#?}");
    }

    #[test]
    fn replaces_both_timestamps_when_the_first_turn_bootstraps_a_thread() {
        let command = json!({
            "type": "thread.turn.start",
            "commandId": "command-2",
            "threadId": "thread-1",
            "message": {
                "messageId": "message-1",
                "role": "user",
                "text": "Start a thread",
                "attachments": [],
            },
            "runtimeMode": "full-access",
            "interactionMode": "default",
            "bootstrap": {
                "createThread": {
                    "projectId": "project-1",
                    "title": "Clock-safe thread",
                    "modelSelection": {"instanceId": "codex", "model": "gpt-5.4"},
                    "runtimeMode": "full-access",
                    "interactionMode": "default",
                    "branch": null,
                    "worktreePath": null,
                    "createdAt": CLIENT_CREATED_AT,
                },
            },
            "createdAt": CLIENT_CREATED_AT,
        });

        let result = to_json(&canonicalize_client_command_timestamps(client(&command), SERVER_RECEIVED_AT));

        assert_eq!(result["type"], "thread.turn.start");
        assert_eq!(result["createdAt"], SERVER_RECEIVED_AT);
        assert_eq!(result["bootstrap"]["createThread"]["createdAt"], SERVER_RECEIVED_AT);
    }
}

mod normalizer_attachments {
    //! `Normalizer.attachments.test.ts`.
    use super::*;

    const ATTACHMENT_UUID: &str = "00000000-0000-4000-8000-0000000000aa";

    /// `turnStartCommand`: each attachment is `{type: "image", name: "screenshot.png",
    /// mimeType: "image/png", ...attachment}`.
    fn turn_start_command(thread_id: Option<&str>, attachments: Vec<Value>, context: Option<Value>) -> Value {
        let attachments: Vec<Value> = attachments
            .into_iter()
            .map(|attachment| {
                let mut full = json!({"type": "image", "name": "screenshot.png", "mimeType": "image/png"});
                merge(&mut full, attachment);
                full
            })
            .collect();
        let mut message = json!({
            "messageId": "message-1",
            "role": "user",
            "text": "look at this",
            "attachments": attachments,
        });
        if let Some(context) = context {
            message["context"] = context;
        }
        json!({
            "type": "thread.turn.start",
            "commandId": "command-1",
            "threadId": thread_id.unwrap_or("thread-1"),
            "message": message,
            "runtimeMode": "full-access",
            "interactionMode": "default",
            "createdAt": "2026-08-01T00:00:00.000Z",
        })
    }

    fn pending_id() -> String {
        format!("pending-{ATTACHMENT_UUID}")
    }

    #[test]
    fn accepts_100_inline_images_and_rejects_101_before_writing_files() {
        let store = AttachmentsDir::new();
        let attachments: Vec<Value> = (0..100).map(|_| json!({"dataUrl": "data:image/png;base64,cGl4ZWxz", "sizeBytes": 6})).collect();
        let mut too_many = attachments.clone();
        too_many.push(attachments[0].clone());

        let rejected = normalize(&store.dir, &turn_start_command(None, too_many, None)).unwrap_err();
        assert!(rejected.message.contains("up to 100"), "{}", rejected.message);
        assert_eq!(store.entries(), Vec::<String>::new());

        let accepted = normalize(&store.dir, &turn_start_command(None, attachments, None)).unwrap();
        assert_eq!(turn_attachments(&accepted).len(), 100);
        assert_eq!(store.entries().len(), 100);
    }

    #[test]
    fn rejects_decoded_image_overflow_before_writing_it_and_removes_earlier_files() {
        // Adaptation: the TS test wraps the FileSystem service to count written bytes
        // (expecting exactly 80 MiB: the eight 10 MiB images, never the overflowing ninth).
        // The Rust normalizer writes with std::fs, so this asserts the observable effect: the
        // 80 MiB error and an empty attachments dir (earlier files removed).
        let store = AttachmentsDir::new();
        // `Buffer.alloc(10 * 1024 * 1024).toString("base64")`: 3_495_253 full zero groups
        // ("AAAA") and one trailing zero byte ("AA==").
        let ten_mib_of_zeros = format!("{}AA==", "A".repeat(3_495_253 * 4));
        let data_url = format!("data:image/png;base64,{ten_mib_of_zeros}");
        let mut attachments: Vec<Value> = (0..8).map(|_| json!({"dataUrl": data_url, "sizeBytes": 1})).collect();
        attachments.push(json!({"dataUrl": "data:image/png;base64,YQ==", "sizeBytes": 0}));
        let command = turn_start_command(None, attachments, None);

        let error = normalize(&store.dir, &command).unwrap_err();
        assert!(error.message.contains("80 MiB"), "{}", error.message);
        assert_eq!(store.entries(), Vec::<String>::new());
    }

    #[test]
    fn rejects_duplicate_client_ids_before_persisting_attachments() {
        let store = AttachmentsDir::new();
        let error = normalize(
            &store.dir,
            &turn_start_command(
                None,
                vec![
                    json!({"id": "same", "dataUrl": "data:image/png;base64,cGl4ZWxz", "sizeBytes": 6}),
                    json!({"id": "same", "dataUrl": "data:image/png;base64,b3RoZXI=", "sizeBytes": 5}),
                ],
                None,
            ),
        )
        .unwrap_err();
        assert!(error.message.contains("duplicate attachment id"), "{}", error.message);
        assert_eq!(store.entries(), Vec::<String>::new());
    }

    #[test]
    fn rebinds_image_context_records_from_the_client_id_to_the_persisted_id() {
        let store = AttachmentsDir::new();
        let normalized = normalize(
            &store.dir,
            &turn_start_command(
                None,
                vec![json!({"id": "local-image-1", "dataUrl": "data:image/png;base64,cGl4ZWxz", "sizeBytes": 6})],
                Some(json!({
                    "version": 1,
                    "records": [
                        {
                            "version": 1,
                            "contextId": "local-image-1",
                            "kind": "image",
                            "label": "screenshot.png",
                            "attachmentId": "local-image-1",
                            "name": "screenshot.png",
                            "mimeType": "image/png",
                            "sizeBytes": 6,
                        },
                        {
                            "version": 1,
                            "contextId": "ctx-skill",
                            "kind": "skill",
                            "label": "$review",
                            "name": "review",
                        },
                    ],
                })),
            ),
        )
        .unwrap();
        let persisted_id = id_of(&turn_attachments(&normalized)[0]);
        assert!(persisted_id.starts_with("thread-1-"), "{persisted_id}");
        let value = to_json(&normalized);
        let records = value["message"]["context"]["records"].as_array().cloned().unwrap_or_default();
        assert_eq!(records[0]["kind"], "image");
        assert_eq!(records[0]["contextId"], "local-image-1");
        assert_eq!(records[0]["attachmentId"], json!(persisted_id));
        assert_eq!(records[1]["kind"], "skill");
        assert_eq!(records[1]["name"], "review");
    }

    #[test]
    fn preserves_inline_image_attachments_from_existing_mobile_clients() {
        let store = AttachmentsDir::new();
        let normalized = normalize(
            &store.dir,
            &turn_start_command(None, vec![json!({"dataUrl": "data:image/png;base64,cGl4ZWxz", "sizeBytes": 6})], None),
        )
        .unwrap();
        let id = id_of(&turn_attachments(&normalized)[0]);
        assert!(id.starts_with("thread-1-"), "{id}");
        assert_eq!(std::fs::read(store.path(&format!("{id}.png"))).unwrap(), b"pixels");
    }

    #[test]
    fn claims_uploaded_attachments_while_retaining_a_retryable_pending_copy() {
        let store = AttachmentsDir::new();
        let bytes = b"pixels";
        let pending_path = store.path(&format!("{}.png", pending_id()));
        std::fs::write(&pending_path, bytes).unwrap();

        let normalized = normalize(
            &store.dir,
            &turn_start_command(
                None,
                vec![json!({"id": pending_id(), "sizeBytes": bytes.len()})],
                Some(json!({
                    "version": 1,
                    "records": [{
                        "version": 1,
                        "contextId": "ctx_pending",
                        "kind": "image",
                        "label": "upload.png",
                        "attachmentId": pending_id(),
                        "name": "upload.png",
                        "mimeType": "image/png",
                        "sizeBytes": bytes.len(),
                    }],
                })),
            ),
        )
        .unwrap();

        let attachment_id = id_of(&turn_attachments(&normalized)[0]);
        assert!(attachment_id.starts_with("thread-1-"), "{attachment_id}");
        assert_ne!(attachment_id, format!("thread-1-{ATTACHMENT_UUID}"));
        assert_eq!(to_json(&normalized)["message"]["context"]["records"][0]["attachmentId"], json!(attachment_id));
        assert!(pending_path.exists());
        let claimed_png_path = store.path(&format!("{attachment_id}.png"));
        assert!(claimed_png_path.exists());
        // A copy, not a hard link: editing the delivered file must not mutate the retryable
        // pending upload.
        assert_distinct_files(&claimed_png_path, &pending_path);
        assert_eq!(std::fs::read(&claimed_png_path).unwrap(), bytes);
    }

    /// `statSync(a).ino !== statSync(b).ino`.
    fn assert_distinct_files(a: &Path, b: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_ne!(std::fs::metadata(a).unwrap().ino(), std::fs::metadata(b).unwrap().ino());
        }
        #[cfg(not(unix))]
        {
            assert_ne!(a, b);
        }
    }

    #[test]
    fn normalizes_inline_and_uploaded_attachments_in_the_same_turn() {
        let store = AttachmentsDir::new();
        std::fs::write(store.path(&format!("{}.png", pending_id())), b"pixels").unwrap();

        let normalized = normalize(
            &store.dir,
            &turn_start_command(
                None,
                vec![
                    json!({"dataUrl": "data:image/png;base64,cGl4ZWxz", "sizeBytes": 6}),
                    json!({"id": pending_id(), "sizeBytes": 6}),
                ],
                None,
            ),
        )
        .unwrap();

        let attachments = turn_attachments(&normalized);
        assert_eq!(attachments.len(), 2);
        assert!(id_of(&attachments[1]).starts_with("thread-1-"));
    }

    #[test]
    fn claims_uploaded_documents_without_changing_their_original_extension() {
        let store = AttachmentsDir::new();
        let pending = format!("pending-{ATTACHMENT_UUID}-pdf");
        let pending_path = store.path(&format!("{pending}.pdf"));
        std::fs::write(&pending_path, b"report").unwrap();

        let mut command = turn_start_command(None, vec![], None);
        command["message"]["attachments"] = json!([{
            "type": "file",
            "id": pending,
            "name": "report.pdf",
            "mimeType": "application/pdf",
            "sizeBytes": 6,
        }]);
        let normalized = normalize(&store.dir, &command).unwrap();

        let attachment = &turn_attachments(&normalized)[0];
        assert_eq!(attachment["type"], "file");
        let id = id_of(attachment);
        assert!(id.starts_with("thread-1-") && id.ends_with("-pdf"), "{id}");
        let claimed_path = store.path(&format!("{id}.pdf"));
        assert_eq!(std::fs::read(&claimed_path).unwrap(), b"report");
        assert_distinct_files(&claimed_path, &pending_path);
    }

    #[test]
    fn retries_a_failed_bootstrap_with_a_fresh_thread_id() {
        let store = AttachmentsDir::new();
        let bytes = b"pixels";
        std::fs::write(store.path(&format!("{}.png", pending_id())), bytes).unwrap();

        let first = normalize(
            &store.dir,
            &turn_start_command(None, vec![json!({"id": pending_id(), "sizeBytes": bytes.len()})], None),
        )
        .unwrap();
        let first_id = id_of(&turn_attachments(&first)[0]);
        std::fs::remove_file(store.path(&format!("{first_id}.png"))).unwrap();

        let retried = normalize(
            &store.dir,
            &turn_start_command(Some("thread-retry"), vec![json!({"id": pending_id(), "sizeBytes": bytes.len()})], None),
        )
        .unwrap();
        assert!(id_of(&turn_attachments(&retried)[0]).starts_with("thread-retry-"));
    }

    #[test]
    fn removes_failed_attachment_claims_without_deleting_their_pending_uploads() {
        let store = AttachmentsDir::new();
        let pending_path = store.path(&format!("{}.png", pending_id()));
        std::fs::write(&pending_path, b"pixels").unwrap();
        let command = turn_start_command(
            None,
            vec![
                json!({"dataUrl": "data:image/png;base64,cGl4ZWxz", "sizeBytes": 6}),
                json!({"id": pending_id(), "sizeBytes": 6}),
            ],
            None,
        );
        let normalized = normalize(&store.dir, &command).unwrap();

        let attachments = turn_attachments(&normalized);
        let inline_path = store.path(&format!("{}.png", id_of(&attachments[0])));
        let claimed_path = store.path(&format!("{}.png", id_of(&attachments[1])));
        cleanup(&store.dir, &command, &normalized);

        assert!(pending_path.exists());
        assert!(!claimed_path.exists());
        assert!(inline_path.exists());
    }

    #[test]
    fn removes_a_failed_claimed_copy_after_its_pending_original_was_removed() {
        let store = AttachmentsDir::new();
        let pending_path = store.path(&format!("{}.png", pending_id()));
        std::fs::write(&pending_path, b"pixels").unwrap();
        let command = turn_start_command(None, vec![json!({"id": pending_id(), "sizeBytes": 6})], None);
        let normalized = normalize(&store.dir, &command).unwrap();

        let claimed_path = store.path(&format!("{}.png", id_of(&turn_attachments(&normalized)[0])));
        std::fs::remove_file(&pending_path).unwrap();

        cleanup(&store.dir, &command, &normalized);

        assert!(!claimed_path.exists());
    }

    #[test]
    fn keeps_concurrent_claims_independent_when_one_dispatch_fails() {
        let store = AttachmentsDir::new();
        let pending_path = store.path(&format!("{}.png", pending_id()));
        std::fs::write(&pending_path, b"pixels").unwrap();
        let command = turn_start_command(None, vec![json!({"id": pending_id(), "sizeBytes": 6})], None);

        // `Effect.all([...], { concurrency: 2 })`: two dispatches of the same command at once.
        let (failed, succeeded) = std::thread::scope(|scope| {
            let first = scope.spawn(|| normalize(&store.dir, &command));
            let second = scope.spawn(|| normalize(&store.dir, &command));
            (first.join().unwrap().unwrap(), second.join().unwrap().unwrap())
        });

        let failed_path = store.path(&format!("{}.png", id_of(&turn_attachments(&failed)[0])));
        let succeeded_path = store.path(&format!("{}.png", id_of(&turn_attachments(&succeeded)[0])));
        assert_ne!(failed_path, succeeded_path);

        cleanup(&store.dir, &command, &failed);

        assert!(pending_path.exists());
        assert!(!failed_path.exists());
        assert!(succeeded_path.exists());
    }

    #[test]
    fn removes_earlier_claimed_copies_when_a_later_attachment_cannot_be_normalized() {
        let store = AttachmentsDir::new();
        let pending = pending_id();
        std::fs::write(store.path(&format!("{pending}.png")), b"pixels").unwrap();

        let failure = normalize(
            &store.dir,
            &turn_start_command(
                None,
                vec![
                    json!({"id": pending, "sizeBytes": 6}),
                    json!({"id": "pending-00000000-0000-4000-8000-0000000000ff", "sizeBytes": 6}),
                ],
                None,
            ),
        )
        .unwrap_err();

        assert!(failure.message.contains("not found"), "{}", failure.message);
        assert_eq!(store.entries(), vec![format!("{pending}.png")]);
    }

    #[test]
    fn rejects_uploaded_attachments_with_the_wrong_size_or_thread() {
        let store = AttachmentsDir::new();
        std::fs::write(store.path(&format!("{}.png", pending_id())), b"pixels").unwrap();

        let wrong_size = normalize(&store.dir, &turn_start_command(None, vec![json!({"id": pending_id(), "sizeBytes": 999})], None)).unwrap_err();
        assert!(wrong_size.message.contains("size"), "{}", wrong_size.message);

        let wrong_thread = normalize(
            &store.dir,
            &turn_start_command(None, vec![json!({"id": format!("another-thread-{ATTACHMENT_UUID}"), "sizeBytes": 6})], None),
        )
        .unwrap_err();
        assert!(wrong_thread.message.contains("pending upload"), "{}", wrong_thread.message);

        let mut mismatched_type_command = turn_start_command(None, vec![json!({"id": pending_id(), "sizeBytes": 6})], None);
        for attachment in mismatched_type_command["message"]["attachments"].as_array_mut().unwrap() {
            attachment["mimeType"] = json!("image/jpeg");
        }
        let mismatched_type = normalize(&store.dir, &mismatched_type_command).unwrap_err();
        assert!(mismatched_type.message.contains("attachment type"), "{}", mismatched_type.message);
    }

    // --- describe("question attachments") ---------------------------------------------------

    /// `Object.values(normalized.attachmentsByQuestionId).flat()`.
    fn question_attachments(normalized: &Value) -> Vec<Value> {
        normalized["attachmentsByQuestionId"]
            .as_object()
            .map(|by_question| {
                by_question
                    .values()
                    .flat_map(|attachments| attachments.as_array().cloned().unwrap_or_default())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn enforces_the_total_response_limit_and_claims_duplicate_filenames_independently() {
        let store = AttachmentsDir::new();
        let id = format!("pending-{ATTACHMENT_UUID}-txt");
        std::fs::write(store.path(&format!("{id}.txt")), "report").unwrap();
        let attachment = json!({
            "type": "file",
            "id": id,
            "name": "notes \"final\" ü.txt",
            "mimeType": "text/plain",
            "sizeBytes": 6,
        });
        let repeated = |count: usize| Value::Array(vec![attachment.clone(); count]);
        let command = json!({
            "type": "thread.user-input.respond",
            "commandId": "answer-cap",
            "threadId": "thread-1",
            "requestId": "request-cap",
            "answers": {"first": "", "second": ""},
            "createdAt": "2026-08-01T00:00:00.000Z",
            "attachmentsByQuestionId": {
                "first": repeated(50),
                "second": repeated(51),
            },
        });
        let failure = normalize(&store.dir, &command).unwrap_err();
        assert!(failure.message.contains("up to 100"), "{}", failure.message);
        assert_eq!(store.entries(), vec![format!("{id}.txt")]);

        let mut accepted = command.clone();
        accepted["attachmentsByQuestionId"]["second"] = repeated(50);
        let normalized = normalize(&store.dir, &accepted).unwrap();
        assert_eq!(command_type(&normalized), "thread.user-input.respond", "Wrong command");
        let attachments = question_attachments(&to_json(&normalized));
        assert_eq!(attachments.len(), 100);
        let unique: std::collections::HashSet<String> = attachments.iter().map(id_of).collect();
        assert_eq!(unique.len(), 100);
        for item in &attachments {
            assert_eq!(item["name"], attachment["name"]);
            assert_eq!(std::fs::read_to_string(store.path(&format!("{}.txt", id_of(item)))).unwrap(), "report");
        }
        cleanup(&store.dir, &accepted, &normalized);
        assert_eq!(store.entries(), vec![format!("{id}.txt")]);
    }

    #[test]
    fn requires_uploaded_metadata_for_question_images_including_pasted_images() {
        // `Schema.is(ClientOrchestrationCommand)` is false: an inline (data URL, no id) question
        // image does not decode.
        let value = json!({
            "type": "thread.user-input.respond",
            "commandId": "answer",
            "threadId": "thread-1",
            "requestId": "request",
            "answers": {"q": ""},
            "createdAt": "2026-08-01T00:00:00.000Z",
            "attachmentsByQuestionId": {
                "q": [{
                    "type": "image",
                    "name": "image.png",
                    "mimeType": "image/png",
                    "sizeBytes": 6,
                    "dataUrl": "data:image/png;base64,cGl4ZWxz",
                }],
            },
        });
        assert!(serde_json::from_value::<ClientOrchestrationCommand>(value).is_err());
    }

    #[test]
    fn preserves_a_proto_question_key_and_cleans_up_its_claimed_files() {
        let store = AttachmentsDir::new();
        let id = pending_id();
        std::fs::write(store.path(&format!("{id}.png")), "pixels").unwrap();
        let command = json!({
            "type": "thread.user-input.respond",
            "commandId": "answer",
            "threadId": "thread-1",
            "requestId": "request",
            "answers": {"__proto__": ""},
            "createdAt": "2026-08-01T00:00:00.000Z",
            "attachmentsByQuestionId": {
                "__proto__": [
                    {"type": "image", "id": id, "name": "image.png", "mimeType": "image/png", "sizeBytes": 6},
                ],
            },
        });
        let normalized = normalize(&store.dir, &command).unwrap();
        assert_eq!(command_type(&normalized), "thread.user-input.respond", "Wrong command");
        let value = to_json(&normalized);
        let keys: Vec<&String> = value["attachmentsByQuestionId"].as_object().unwrap().keys().collect();
        assert_eq!(keys, vec!["__proto__"]);
        let attachment = &value["attachmentsByQuestionId"]["__proto__"][0];
        let claimed_path = store.path(&format!("{}.png", id_of(attachment)));
        assert!(claimed_path.exists());
        cleanup(&store.dir, &command, &normalized);
        assert!(!claimed_path.exists());
    }

    #[test]
    fn claims_images_and_files_by_question_preserves_answers_and_cleans_up_failed_dispatches() {
        let store = AttachmentsDir::new();
        let image_id = pending_id();
        let file_id = format!("pending-{ATTACHMENT_UUID}-txt");
        std::fs::write(store.path(&format!("{image_id}.png")), "pixels").unwrap();
        std::fs::write(store.path(&format!("{file_id}.txt")), "report").unwrap();
        let command = json!({
            "type": "thread.user-input.respond",
            "commandId": "answer",
            "threadId": "thread-1",
            "requestId": "request",
            "answers": {"q1": ["Selected option"], "q2": ""},
            "createdAt": "2026-08-01T00:00:00.000Z",
            "attachmentsByQuestionId": {
                "q1": [{"type": "image", "id": image_id, "name": "image.png", "mimeType": "image/png", "sizeBytes": 6}],
                "q2": [{"type": "file", "id": file_id, "name": "report.txt", "mimeType": "text/plain", "sizeBytes": 6}],
            },
        });
        let normalized = normalize(&store.dir, &command).unwrap();
        assert_eq!(command_type(&normalized), "thread.user-input.respond", "Wrong command");
        let value = to_json(&normalized);
        assert_eq!(value["answers"], command["answers"]);
        let image = id_of(&value["attachmentsByQuestionId"]["q1"][0]);
        let file = id_of(&value["attachmentsByQuestionId"]["q2"][0]);
        assert_eq!(std::fs::read_to_string(store.path(&format!("{image}.png"))).unwrap(), "pixels");
        assert_eq!(std::fs::read_to_string(store.path(&format!("{file}.txt"))).unwrap(), "report");
        cleanup(&store.dir, &command, &normalized);
        assert!(!store.path(&format!("{image}.png")).exists());
        assert!(!store.path(&format!("{file}.txt")).exists());
        assert!(store.path(&format!("{image_id}.png")).exists());
        let retry = normalize(&store.dir, &command).unwrap();
        assert_eq!(command_type(&retry), "thread.user-input.respond");
    }

    #[test]
    fn removes_all_claimed_copies_if_a_later_question_upload_is_missing() {
        let store = AttachmentsDir::new();
        let id = pending_id();
        std::fs::write(store.path(&format!("{id}.png")), "pixels").unwrap();
        let result = normalize(
            &store.dir,
            &json!({
                "type": "thread.user-input.respond",
                "commandId": "answer",
                "threadId": "thread-1",
                "requestId": "request",
                "answers": {"q1": "", "q2": ""},
                "createdAt": "2026-08-01T00:00:00.000Z",
                "attachmentsByQuestionId": {
                    "q1": [{"type": "image", "id": id, "name": "image.png", "mimeType": "image/png", "sizeBytes": 6}],
                    "q2": [{
                        "type": "file",
                        "id": format!("{id}-txt"),
                        "name": "missing.txt",
                        "mimeType": "text/plain",
                        "sizeBytes": 6,
                    }],
                },
            }),
        );
        assert!(result.is_err());
        let thread_files: Vec<String> = store.entries().into_iter().filter(|name| name.starts_with("thread-1-")).collect();
        assert_eq!(thread_files, Vec::<String>::new());
    }
}

mod attachment_store {
    //! `attachmentStore.test.ts`.
    use std::time::{Duration, UNIX_EPOCH};

    use zc_orchestration::attachments::{
        attachment_file_extension, create_attachment_id, create_pending_attachment_id, parse_attachment_file_extension, parse_attachment_uuid,
        parse_thread_segment_from_attachment_id, plan_attachment_claim, resolve_attachment_path_by_id, sweep_stale_pending_attachments, AttachmentClaimPlan,
    };

    fn temp_dir(prefix: &str) -> tempfile::TempDir {
        tempfile::Builder::new().prefix(prefix).tempdir().expect("create a temp dir")
    }

    /// `/^[a-f0-9-]{36}$/`.
    fn is_uuid_shaped(value: Option<&str>) -> bool {
        value.is_some_and(|value| value.len() == 36 && value.chars().all(|c| matches!(c, 'a'..='f' | '0'..='9' | '-')))
    }

    #[test]
    fn sanitizes_thread_ids_when_creating_attachment_ids() {
        let attachment_id = create_attachment_id("thread.folder/unsafe space", None);
        assert!(attachment_id.as_deref().is_some_and(|id| !id.is_empty()));
        let Some(attachment_id) = attachment_id else { return };

        let thread_segment = parse_thread_segment_from_attachment_id(&attachment_id);
        assert!(thread_segment.as_deref().is_some_and(|segment| !segment.is_empty()));
        let thread_segment = thread_segment.unwrap();
        // `/^[a-z0-9_-]+$/i`
        assert!(
            thread_segment.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "{thread_segment}"
        );
        assert!(!thread_segment.contains('.'));
        assert!(!thread_segment.contains('%'));
        assert!(!thread_segment.contains('/'));
    }

    #[test]
    fn parses_exact_thread_segments_from_attachment_ids_without_prefix_collisions() {
        let foo_id = "foo-00000000-0000-4000-8000-000000000001";
        let foo_bar_id = "foo-bar-00000000-0000-4000-8000-000000000002";

        assert_eq!(parse_thread_segment_from_attachment_id(foo_id).as_deref(), Some("foo"));
        assert_eq!(parse_thread_segment_from_attachment_id(foo_bar_id).as_deref(), Some("foo-bar"));
    }

    #[test]
    fn normalizes_created_thread_segments_to_lowercase() {
        let attachment_id = create_attachment_id("Thread.Foo", None);
        assert!(attachment_id.is_some());
        let Some(attachment_id) = attachment_id else { return };
        assert_eq!(parse_thread_segment_from_attachment_id(&attachment_id).as_deref(), Some("thread-foo"));
    }

    #[test]
    fn reserves_the_pending_attachment_segment() {
        let pending_id = create_pending_attachment_id(None);
        assert_eq!(parse_thread_segment_from_attachment_id(&pending_id).as_deref(), Some("pending"));
        assert!(is_uuid_shaped(parse_attachment_uuid(&pending_id).as_deref()), "{pending_id}");
        assert_eq!(
            parse_thread_segment_from_attachment_id(&create_attachment_id("pending", None).unwrap()).as_deref(),
            Some("_pending")
        );
        assert_eq!(
            parse_thread_segment_from_attachment_id(&create_attachment_id("pending_thread", None).unwrap()).as_deref(),
            Some("pending_thread")
        );
    }

    #[test]
    fn preserves_safe_file_extensions_in_attachment_ids_and_paths() {
        let attachment_id = create_pending_attachment_id(Some(".PDF"));

        assert_eq!(parse_thread_segment_from_attachment_id(&attachment_id).as_deref(), Some("pending"));
        assert!(is_uuid_shaped(parse_attachment_uuid(&attachment_id).as_deref()), "{attachment_id}");
        assert_eq!(parse_attachment_file_extension(&attachment_id).as_deref(), Some("pdf"));
        assert_eq!(attachment_file_extension("report.PDF"), ".pdf");
        assert_eq!(attachment_file_extension("report"), ".bin");
        assert_eq!(attachment_file_extension("report.extensiontoolong"), ".bin");
        // ".part" is the in-flight upload suffix; storing it would make the file look like a
        // stale partial to the sweep.
        assert_eq!(attachment_file_extension("archive.part"), ".bin");
        let long = create_attachment_id(&"x".repeat(80), Some(".abcdefghij")).expect("an id");
        assert!(long.len() <= 128, "{} chars", long.len());
    }

    #[test]
    fn resolves_attachment_path_by_id_using_the_extension_that_exists_on_disk() {
        let root = temp_dir("zc-attachment-store-");
        let attachments_dir = root.path();
        let attachment_id = "thread-1-attachment";
        let png_path = attachments_dir.join(format!("{attachment_id}.png"));
        std::fs::write(&png_path, b"hello").unwrap();

        assert_eq!(resolve_attachment_path_by_id(attachments_dir, attachment_id), Some(png_path));
    }

    #[test]
    fn returns_null_when_no_attachment_file_exists_for_the_id() {
        let root = temp_dir("zc-attachment-store-");
        assert_eq!(resolve_attachment_path_by_id(root.path(), "thread-1-missing"), None);
    }

    #[test]
    fn resolves_generic_attachments_without_scanning_the_attachment_directory() {
        let root = temp_dir("zc-file-attachment-");
        let attachments_dir = root.path();
        let attachment_id = "thread-1-00000000-0000-4000-8000-000000000001-zip";
        let archive_path = attachments_dir.join(format!("{attachment_id}.zip"));
        std::fs::write(&archive_path, b"archive").unwrap();

        assert_eq!(resolve_attachment_path_by_id(attachments_dir, attachment_id), Some(archive_path));
    }

    #[test]
    fn plans_pending_attachment_claims_with_direct_filename_lookups() {
        let root = temp_dir("zc-attachment-claim-");
        let attachments_dir = root.path();
        let uuid = "00000000-0000-4000-8000-000000000001";
        let pending_path = attachments_dir.join(format!("pending-{uuid}.png"));
        std::fs::write(&pending_path, b"pixels").unwrap();

        let claim = plan_attachment_claim(attachments_dir, "thread-1", &format!("pending-{uuid}"));
        let AttachmentClaimPlan::Ok {
            final_id,
            current_path,
            final_path,
        } = claim
        else {
            panic!("expected an ok claim, got {claim:?}");
        };
        assert_eq!(current_path, pending_path);
        assert_eq!(parse_thread_segment_from_attachment_id(&final_id).as_deref(), Some("thread-1"));
        assert_ne!(parse_attachment_uuid(&final_id).as_deref(), Some(uuid));
        assert_eq!(final_path, attachments_dir.join(format!("{final_id}.png")));
    }

    #[test]
    fn rejects_thread_owned_attachments_even_when_thread_segments_collide() {
        let root = temp_dir("zc-attachment-ownership-");
        let attachments_dir = root.path();
        let attachment_id = "a-b-00000000-0000-4000-8000-000000000003";
        std::fs::write(attachments_dir.join(format!("{attachment_id}.png")), "pixels").unwrap();

        assert_eq!(
            plan_attachment_claim(attachments_dir, "a b", attachment_id),
            AttachmentClaimPlan::Rejected {
                reason: "attachment must be a pending upload".to_owned(),
            }
        );
    }

    #[test]
    fn removes_expired_pending_and_partial_files_without_touching_thread_attachments() {
        let root = temp_dir("zc-attachment-sweep-");
        let attachments_dir = root.path();
        let now: i64 = 1_800_000_000_000;
        let old_time = UNIX_EPOCH + Duration::from_millis((now - 2 * 24 * 60 * 60 * 1000) as u64);
        let uuid = "00000000-0000-4000-8000-000000000002";
        let pending_path = attachments_dir.join(format!("pending-{uuid}.png"));
        let pending_file_path = attachments_dir.join(format!("pending-{uuid}-pdf.pdf"));
        let thread_path = attachments_dir.join(format!("thread-1-{uuid}.png"));
        let partial_path = attachments_dir.join(format!("{uuid}.part"));
        for file_path in [&pending_path, &pending_file_path, &thread_path, &partial_path] {
            std::fs::write(file_path, b"pixels").unwrap();
            // `utimesSync(filePath, old, old)`: only the mtime matters to the sweep.
            std::fs::File::options().write(true).open(file_path).unwrap().set_modified(old_time).unwrap();
        }

        assert_eq!(sweep_stale_pending_attachments(attachments_dir, now), 3);
        assert!(!pending_path.exists());
        assert!(!pending_file_path.exists());
        assert!(!partial_path.exists());
        assert!(thread_path.exists());
    }
}

mod image_mime {
    //! `imageMime.test.ts`.
    use zc_orchestration::attachments::{infer_image_extension, parse_base64_data_url, DataUrl};

    fn data_url(mime_type: &str, base64: &str) -> Option<DataUrl> {
        Some(DataUrl {
            mime_type: mime_type.to_owned(),
            base64: base64.to_owned(),
        })
    }

    #[test]
    fn parses_base64_data_url_with_mime_type() {
        assert_eq!(parse_base64_data_url("data:image/png;base64,SGVsbG8="), data_url("image/png", "SGVsbG8="));
    }

    #[test]
    fn parses_base64_data_url_with_mime_parameters() {
        assert_eq!(
            parse_base64_data_url("data:image/png;charset=utf-8;base64,SGVsbG8="),
            data_url("image/png", "SGVsbG8=")
        );
    }

    #[test]
    fn rejects_non_base64_data_url() {
        assert_eq!(parse_base64_data_url("data:image/png;charset=utf-8,hello"), None);
    }

    #[test]
    fn rejects_missing_mime_type() {
        assert_eq!(parse_base64_data_url("data:;base64,SGVsbG8="), None);
    }

    #[test]
    fn parses_base64_data_url_with_spaces_in_payload() {
        assert_eq!(parse_base64_data_url("data:image/png;base64,SGVs bG8=\n"), data_url("image/png", "SGVsbG8="));
    }

    #[test]
    fn rejects_payload_with_characters_outside_the_base64_alphabet() {
        assert_eq!(parse_base64_data_url("data:image/png;base64,SGVs!bG8="), None);
        assert_eq!(parse_base64_data_url("data:image/png;base64,SGVs,bG8="), None);
    }

    #[test]
    fn rejects_structurally_malformed_base64() {
        // '=' before the trailing padding position
        assert_eq!(parse_base64_data_url("data:image/png;base64,AB=CD==="), None);
        assert_eq!(parse_base64_data_url("data:image/png;base64,SGV=bG8="), None);
        // more than two padding characters
        assert_eq!(parse_base64_data_url("data:image/png;base64,SGVsbG8=====AAA"), None);
        // length not a multiple of 4
        assert_eq!(parse_base64_data_url("data:image/png;base64,SGVsbG8"), None);
    }

    #[test]
    fn accepts_base64_with_one_or_two_trailing_padding_characters() {
        assert_eq!(parse_base64_data_url("data:image/png;base64,SGVsbA=="), data_url("image/png", "SGVsbA=="));
        assert_eq!(parse_base64_data_url("data:image/png;base64,SGVsbG8h"), data_url("image/png", "SGVsbG8h"));
    }

    #[test]
    fn rejects_empty_and_whitespace_only_payloads() {
        assert_eq!(parse_base64_data_url("data:image/png;base64,"), None);
        assert_eq!(parse_base64_data_url("data:image/png;base64, \r\n"), None);
    }

    #[test]
    fn parses_a_case_insensitive_scheme_and_mime_type() {
        assert_eq!(parse_base64_data_url("DATA:IMAGE/PNG;BASE64,SGVsbG8="), data_url("image/png", "SGVsbG8="));
    }

    #[test]
    fn parses_a_multi_megabyte_payload_from_a_deep_call_stack() {
        // Regression in TS: a regex over the payload borrowed the JS call stack, so a ~10 MB
        // image parsed deep inside fiber execution overflowed it. Rust has no such shared
        // stack to exhaust by recursion depth; the equivalent check parses the 14 MB payload
        // on a thread with a deliberately small stack (64 KiB), so any recursion or
        // stack-heavy matching over the payload would overflow.
        let data_url = format!("data:image/png;base64,{}", "A".repeat(14_000_000));
        let result = std::thread::Builder::new()
            .stack_size(64 * 1024)
            .spawn(move || parse_base64_data_url(&data_url))
            .expect("spawn a small-stack thread")
            .join()
            .expect("parse without overflowing the stack");
        assert_eq!(result.as_ref().map(|parsed| parsed.mime_type.as_str()), Some("image/png"));
        assert_eq!(result.map(|parsed| parsed.base64.len()), Some(14_000_000));
    }

    #[test]
    fn does_not_read_inherited_keys_from_mime_extension_map() {
        assert_eq!(infer_image_extension("constructor", None), ".bin");
    }
}
