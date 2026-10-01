//! The `pullRequests.*` handlers over a real zc-rpc server and an in-memory socket, against a
//! fake service: registration of every method, payload normalization, `withPullRequestViewer`,
//! the sync requests after `runAction` / `invalidate`, typed failures and the refresh stream.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::channel::mpsc;
use futures::future::BoxFuture;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use zc_contracts::*;
use zc_ports::EventStream;
use zc_pullrequest::error::PullRequestError;
use zc_pullrequest::rpc::{register, PullRequestLinks, PullRequestRpcServices, PullRequestServiceApi};
use zc_rpc::{AuthContext, ConnectionSetup, Inbound, Outbound, RpcRouter, RpcServer};

#[derive(Default)]
struct Calls(Mutex<Vec<String>>);

impl Calls {
    fn push(&self, call: impl Into<String>) {
        self.0.lock().unwrap().push(call.into());
    }
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut self.0.lock().unwrap())
    }
}

struct FakeService {
    calls: Arc<Calls>,
    refreshes: Mutex<Vec<futures::channel::mpsc::UnboundedSender<u64>>>,
}

fn reference(input: &PullRequestRef) -> String {
    format!("{}#{}", input.repository, input.number)
}

fn unsupported() -> PullRequestError {
    PullRequestError::unavailable(PullRequestUnavailableReason::ProviderUnsupported)
}

#[async_trait]
impl PullRequestServiceApi for FakeService {
    async fn list(&self, input: PullRequestListInput) -> Result<PullRequestListResult, PullRequestError> {
        self.calls.push(format!("list {:?}", input.query));
        Ok(PullRequestListResult {
            viewers: [("github.com".to_owned(), "octo-reader".to_owned())].into_iter().collect(),
            providers: Vec::new(),
            entries: Vec::new(),
            errors: Vec::new(),
            truncated: false,
            next_cursors: Default::default(),
        })
    }
    async fn list_stats(&self, _input: PullRequestListStatsInput) -> Result<PullRequestListStatsResult, PullRequestError> {
        Ok(PullRequestListStatsResult { stats: Vec::new() })
    }
    async fn routing(&self, _input: PullRequestRef) -> Result<PullRequestRoutingResult, PullRequestError> {
        Err(unsupported())
    }
    async fn routing_identity(&self, _input: PullRequestRoutingIdentityInput) -> Result<PullRequestRoutingIdentityResult, PullRequestError> {
        Err(unsupported())
    }
    async fn with_routing_credential(
        &self,
        input: PullRequestRef,
        operation: BoxFuture<'static, Result<Value, PullRequestError>>,
    ) -> Result<Value, PullRequestError> {
        self.calls.push(format!("viewer {} {:?}", reference(&input), input.expected_account_id));
        if input.expected_account_id.as_deref() == Some("someone-else") {
            return Err(PullRequestError::operation(
                "routeIdentity",
                "The GitHub account could not be verified before starting the operation.",
            ));
        }
        operation.await
    }
    async fn summary(&self, input: PullRequestRef) -> Result<PullRequestSummary, PullRequestError> {
        self.calls.push(format!("summary {}", reference(&input)));
        Err(PullRequestError::operation("summary", "The host did not answer."))
    }
    async fn stack(&self, input: PullRequestRef) -> Result<Option<PullRequestStack>, PullRequestError> {
        self.calls.push(format!("stack {}", reference(&input)));
        Ok(None)
    }
    async fn detail(&self, _input: PullRequestRef) -> Result<PullRequestDetail, PullRequestError> {
        Err(unsupported())
    }
    async fn preview(&self, _input: PullRequestRef) -> Result<PullRequestPreview, PullRequestError> {
        Err(unsupported())
    }
    async fn activity(&self, _input: PullRequestRef) -> Result<PullRequestActivity, PullRequestError> {
        Err(unsupported())
    }
    async fn thread_comments(&self, _input: PullRequestThreadCommentsInput) -> Result<PullRequestThreadCommentsResult, PullRequestError> {
        Err(unsupported())
    }
    async fn diff_file_contents(&self, _input: PullRequestDiffFileContentsInput) -> Result<PullRequestDiffFileContentsResult, PullRequestError> {
        Err(unsupported())
    }
    async fn files_viewed(&self, _input: PullRequestRef) -> Result<PullRequestFilesViewedResult, PullRequestError> {
        Ok(PullRequestFilesViewedResult {
            files: Vec::new(),
            truncated: false,
        })
    }
    async fn set_files_viewed(&self, _input: PullRequestSetFilesViewedInput) -> Result<(), PullRequestError> {
        Ok(())
    }
    async fn run_action(&self, input: PullRequestActionInput) -> Result<(), PullRequestError> {
        self.calls.push(format!("runAction {} {}", input.repository, input.action.as_str()));
        if input.action == PullRequestAction::Close {
            return Err(PullRequestError::operation(
                "runAction",
                "You need write access on this repository, or to have opened this change request, to close it.",
            ));
        }
        Ok(())
    }
    async fn update(&self, _input: PullRequestUpdateInput) -> Result<(), PullRequestError> {
        Ok(())
    }
    async fn comment(&self, input: PullRequestCommentInput) -> Result<(), PullRequestError> {
        self.calls.push(format!("comment {:?}", input.body));
        Ok(())
    }
    async fn update_comment(&self, _input: PullRequestCommentUpdateInput) -> Result<(), PullRequestError> {
        Ok(())
    }
    async fn submit_review(&self, _input: PullRequestSubmitReviewInput) -> Result<(), PullRequestError> {
        Ok(())
    }
    async fn reply_to_thread(&self, _input: PullRequestThreadReplyInput) -> Result<(), PullRequestError> {
        Ok(())
    }
    async fn set_thread_resolution(&self, _input: PullRequestThreadResolutionInput) -> Result<(), PullRequestError> {
        Ok(())
    }
    async fn set_reaction(&self, _input: PullRequestReactionInput) -> Result<(), PullRequestError> {
        Ok(())
    }
    async fn reviewer_candidates(&self, _input: PullRequestRef) -> Result<PullRequestReviewerCandidateList, PullRequestError> {
        Err(unsupported())
    }
    async fn request_reviewers(&self, _input: PullRequestReviewerRequestInput) -> Result<(), PullRequestError> {
        Ok(())
    }
    async fn label_candidates(&self, _input: PullRequestRef) -> Result<PullRequestLabelCandidateList, PullRequestError> {
        Err(unsupported())
    }
    async fn set_labels(&self, input: PullRequestLabelChangeInput) -> Result<(), PullRequestError> {
        self.calls.push(format!("setLabels {:?}", input.labels));
        Ok(())
    }
    async fn invalidate(&self, input: PullRequestInvalidateInput) {
        self.calls.push(format!(
            "invalidate {:?} {:?}",
            input.reference.as_ref().map(reference),
            input.files_viewed_only
        ));
    }
    fn subscribe_refreshes(&self) -> EventStream<u64> {
        let (sender, receiver) = futures::channel::mpsc::unbounded();
        sender.unbounded_send(0).unwrap();
        self.refreshes.lock().unwrap().push(sender);
        receiver.boxed()
    }
}

struct FakeLinks(Arc<Calls>);

#[async_trait]
impl PullRequestLinks for FakeLinks {
    async fn linked_threads(&self, reference: &PullRequestRef) -> Result<PullRequestLinkedThreadsResult, PullRequestError> {
        self.0.push(format!("linkedThreads {}#{}", reference.repository, reference.number));
        Ok(PullRequestLinkedThreadsResult {
            threads: vec![PullRequestLinkedThreadsResultThreadsItem {
                id: ThreadId::from("thread-1".to_owned()),
                project_id: reference.project_id.clone(),
                title: "Fix the widget".into(),
                archived_at: None,
            }],
        })
    }
    async fn request_sync(&self, reference: &PullRequestRef) {
        self.0.push(format!("requestSync {}#{}", reference.repository, reference.number));
    }
}

struct Harness {
    server: Arc<RpcServer>,
    calls: Arc<Calls>,
    service: Arc<FakeService>,
}

impl Harness {
    fn new() -> Self {
        let calls = Arc::new(Calls::default());
        let service = Arc::new(FakeService {
            calls: calls.clone(),
            refreshes: Mutex::new(Vec::new()),
        });
        let services = PullRequestRpcServices {
            service: service.clone(),
            links: Arc::new(FakeLinks(calls.clone())),
        };
        let router: RpcRouter = register(RpcRouter::builder(), services).build().unwrap();
        Self {
            server: RpcServer::new(router),
            calls,
            service,
        }
    }

    fn bump_refreshes(&self, value: u64) {
        for sender in self.service.refreshes.lock().unwrap().iter() {
            let _ = sender.unbounded_send(value);
        }
    }
}

struct Client {
    tx: mpsc::UnboundedSender<Inbound>,
    rx: mpsc::UnboundedReceiver<Outbound>,
}

impl Client {
    fn connect(server: &Arc<RpcServer>, scopes: &[&str]) -> Self {
        let (tx, in_rx) = mpsc::unbounded();
        let (out_tx, rx) = mpsc::unbounded();
        let server = server.clone();
        let setup = ConnectionSetup {
            auth: AuthContext::new(scopes.iter().map(|scope| scope.to_string())),
            ..Default::default()
        };
        tokio::spawn(async move {
            server.serve_socket(setup, in_rx, out_tx.sink_map_err(|_| ())).await;
        });
        Self { tx, rx }
    }

    fn request(&self, id: u64, tag: &str, payload: Value) {
        let frame = json!({"_tag": "Request", "id": id.to_string(), "tag": tag, "payload": payload, "headers": []});
        self.tx.unbounded_send(Inbound::Text(frame.to_string())).unwrap();
    }

    async fn recv(&mut self) -> Value {
        match tokio::time::timeout(Duration::from_secs(10), self.rx.next()).await {
            Ok(Some(Outbound::Text(text))) => serde_json::from_str(&text).unwrap(),
            other => panic!("expected a frame, got {other:?}"),
        }
    }

    async fn call(&mut self, id: u64, tag: &str, payload: Value) -> Value {
        self.request(id, tag, payload);
        let frame = self.recv().await;
        assert_eq!(frame["_tag"], json!("Exit"), "{frame}");
        frame["exit"].clone()
    }

    async fn chunk(&mut self, id: u64) -> Vec<Value> {
        let frame = self.recv().await;
        assert_eq!(frame["_tag"], json!("Chunk"), "{frame}");
        let ack = json!({"_tag": "Ack", "requestId": id.to_string()});
        self.tx.unbounded_send(Inbound::Text(ack.to_string())).unwrap();
        frame["values"].as_array().unwrap().clone()
    }
}

const BOTH: &[&str] = &["orchestration:read", "orchestration:operate"];

fn pr(number: i64) -> Value {
    json!({"projectId": "project-1", "repository": "acme/widgets", "number": number})
}

fn fail_of(exit: &Value) -> Value {
    assert_eq!(exit["_tag"], json!("Failure"), "{exit}");
    exit["cause"][0]["error"].clone()
}

#[tokio::test]
async fn registers_every_pull_request_method() {
    let router = register(
        RpcRouter::builder(),
        PullRequestRpcServices {
            service: Harness::new().service,
            links: Arc::new(zc_pullrequest::rpc::NoLinks),
        },
    )
    .build()
    .unwrap();
    let expected: Vec<&str> = METHODS.iter().map(|spec| spec.tag).filter(|tag| tag.starts_with("pullRequests.")).collect();
    assert_eq!(expected.len(), 28);
    for tag in expected {
        assert!(router.contains(tag), "{tag} is not registered");
    }
    assert_eq!(router.is_stream("pullRequests.subscribeRefreshes"), Some(true));
}

#[tokio::test]
async fn list_trims_its_query_and_answers_without_the_viewer_wrapper() {
    let harness = Harness::new();
    let mut client = Client::connect(&harness.server, BOTH);
    let exit = client.call(1, "pullRequests.list", json!({"state": "open", "query": "  flaky test  "})).await;
    assert_eq!(exit["_tag"], json!("Success"), "{exit}");
    assert_eq!(exit["value"]["viewers"], json!({"github.com": "octo-reader"}));
    assert_eq!(harness.calls.take(), vec!["list Some(\"flaky test\")".to_owned()]);
}

#[tokio::test]
async fn reads_run_inside_the_viewer_wrapper_and_fail_with_the_contract_error() {
    let harness = Harness::new();
    let mut client = Client::connect(&harness.server, BOTH);
    let mut payload = pr(7);
    payload["repository"] = json!("  acme/widgets ");
    payload["expectedAccountId"] = json!("1234");
    let exit = client.call(1, "pullRequests.summary", payload).await;
    assert_eq!(
        fail_of(&exit),
        json!({"_tag": "PullRequestOperationError", "operation": "summary", "detail": "The host did not answer."})
    );
    assert_eq!(
        harness.calls.take(),
        vec!["viewer acme/widgets#7 Some(\"1234\")".to_owned(), "summary acme/widgets#7".to_owned()]
    );

    let mut refused = pr(7);
    refused["expectedAccountId"] = json!("someone-else");
    let exit = client.call(2, "pullRequests.stack", refused).await;
    assert_eq!(fail_of(&exit)["operation"], json!("routeIdentity"));
    assert_eq!(harness.calls.take(), vec!["viewer acme/widgets#7 Some(\"someone-else\")".to_owned()]);

    let exit = client.call(3, "pullRequests.stack", pr(8)).await;
    assert_eq!(exit, json!({"_tag": "Success", "value": null}));
}

#[tokio::test]
async fn run_action_requests_a_sync_only_after_it_succeeds() {
    let harness = Harness::new();
    let mut client = Client::connect(&harness.server, BOTH);
    let mut merge = pr(3);
    merge["action"] = json!("merge");
    let exit = client.call(1, "pullRequests.runAction", merge).await;
    assert_eq!(exit, json!({"_tag": "Success", "value": null}));
    assert_eq!(
        harness.calls.take(),
        vec![
            "viewer acme/widgets#3 None".to_owned(),
            "runAction acme/widgets merge".to_owned(),
            "requestSync acme/widgets#3".to_owned()
        ]
    );
    let mut close = pr(3);
    close["action"] = json!("close");
    let exit = client.call(2, "pullRequests.runAction", close).await;
    assert_eq!(fail_of(&exit)["_tag"], json!("PullRequestOperationError"));
    assert_eq!(
        harness.calls.take(),
        vec!["viewer acme/widgets#3 None".to_owned(), "runAction acme/widgets close".to_owned()]
    );
}

#[tokio::test]
async fn invalidate_syncs_the_reference_unless_only_viewed_files_were_asked_for() {
    let harness = Harness::new();
    let mut client = Client::connect(&harness.server, &["orchestration:read"]);
    let exit = client.call(1, "pullRequests.invalidate", json!({"reference": pr(4)})).await;
    assert_eq!(exit["_tag"], json!("Success"));
    assert_eq!(
        harness.calls.take(),
        vec!["invalidate Some(\"acme/widgets#4\") None".to_owned(), "requestSync acme/widgets#4".to_owned()]
    );
    client
        .call(2, "pullRequests.invalidate", json!({"reference": pr(4), "filesViewedOnly": true}))
        .await;
    assert_eq!(harness.calls.take(), vec!["invalidate Some(\"acme/widgets#4\") Some(true)".to_owned()]);
    client.call(3, "pullRequests.invalidate", json!({})).await;
    assert_eq!(harness.calls.take(), vec!["invalidate None None".to_owned()]);
}

#[tokio::test]
async fn linked_threads_come_from_the_links() {
    let harness = Harness::new();
    let mut client = Client::connect(&harness.server, &["orchestration:read"]);
    let exit = client.call(1, "pullRequests.linkedThreads", pr(9)).await;
    assert_eq!(
        exit["value"],
        json!({"threads": [{"id": "thread-1", "projectId": "project-1", "title": "Fix the widget", "archivedAt": null}]})
    );
}

#[tokio::test]
async fn writes_need_the_operate_scope_and_bad_payloads_die() {
    let harness = Harness::new();
    let mut reader = Client::connect(&harness.server, &["orchestration:read"]);
    let mut comment = pr(5);
    comment["body"] = json!("  Looks good  ");
    let exit = reader.call(1, "pullRequests.comment", comment.clone()).await;
    assert_eq!(fail_of(&exit)["_tag"], json!("EnvironmentAuthorizationError"), "{exit}");
    assert!(harness.calls.take().is_empty());

    let mut writer = Client::connect(&harness.server, BOTH);
    let exit = writer.call(2, "pullRequests.comment", comment).await;
    assert_eq!(exit["_tag"], json!("Success"));
    assert_eq!(
        harness.calls.take(),
        vec!["viewer acme/widgets#5 None".to_owned(), "comment \"  Looks good  \"".to_owned()]
    );

    let mut labels = pr(5);
    labels["labels"] = json!([" bug ", "triage"]);
    labels["applied"] = json!(true);
    writer.call(3, "pullRequests.setLabels", labels).await;
    assert_eq!(harness.calls.take()[1], "setLabels [\"bug\", \"triage\"]");

    let exit = writer
        .call(4, "pullRequests.detail", json!({"projectId": "project-1", "repository": " ", "number": 1}))
        .await;
    assert_eq!(exit["_tag"], json!("Failure"));
    assert_eq!(exit["cause"][0]["_tag"], json!("Die"), "{exit}");
    assert!(harness.calls.take().is_empty());
}

#[tokio::test]
async fn subscribe_refreshes_streams_the_counter() {
    let harness = Harness::new();
    let mut client = Client::connect(&harness.server, &["orchestration:read"]);
    client.request(1, "pullRequests.subscribeRefreshes", json!({}));
    assert_eq!(client.chunk(1).await, vec![json!(0)]);
    harness.bump_refreshes(1);
    assert_eq!(client.chunk(1).await, vec![json!(1)]);
}
