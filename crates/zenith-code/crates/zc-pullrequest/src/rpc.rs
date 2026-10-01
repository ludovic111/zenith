//! The `pullRequests.*` handlers of `ws.ts` (lines ~2763-2977).
//!
//! | Method | Scope | Behaviour |
//! |---|---|---|
//! | `list`, `listStats`, `routingIdentity`, `routing` | read | the service call |
//! | `summary`, `stack`, `detail`, `preview`, `activity`, `threadComments`, `diffFileContents`, `filesViewed`, `reviewerCandidates`, `labelCandidates` | read | the service call inside `withPullRequestViewer` |
//! | `linkedThreads` | read | the threads linked to the ref's sync key (none without a key) |
//! | `setFilesViewed`, `update`, `comment`, `updateComment`, `submitReview`, `replyToThread`, `setThreadResolution`, `setReaction`, `requestReviewers`, `setLabels` | operate | the service call inside `withPullRequestViewer` |
//! | `runAction` | operate | inside `withPullRequestViewer`, then a sync request for the ref's key |
//! | `invalidate` | read | `invalidate(input, {notifyReaders: true})`, then — unless `filesViewedOnly` or no reference — a sync request |
//! | `subscribeRefreshes` | read | the refresh counter stream |
//!
//! `withPullRequestViewer` is `PullRequestService.withRoutingCredential`: with an
//! `expectedAccountId` the call runs only under the verified, pinned credential of that account.
//! Payloads are normalized like the TS schema decode ([`crate::decode`]); a payload that does not
//! decode fails the request with a `Die`, like the TS server. Failures are the contract
//! `PullRequestUnavailableError | PullRequestOperationError`.

use std::future::Future;
use std::sync::Arc;

use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::StreamExt;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use zc_contracts::*;
use zc_ports::EventStream;
use zc_rpc::{MethodOptions, RpcError, RpcRouterBuilder, ScopeRule};

use crate::decode::{decode, PayloadSchema};
use crate::error::PullRequestError;

/// The `PullRequestService` operations the handlers call (implemented by
/// [`crate::service::PullRequestService`]; tests use fakes).
#[async_trait]
pub trait PullRequestServiceApi: Send + Sync + 'static {
    async fn list(&self, input: PullRequestListInput) -> Result<PullRequestListResult, PullRequestError>;
    async fn list_stats(&self, input: PullRequestListStatsInput) -> Result<PullRequestListStatsResult, PullRequestError>;
    async fn routing(&self, input: PullRequestRef) -> Result<PullRequestRoutingResult, PullRequestError>;
    async fn routing_identity(&self, input: PullRequestRoutingIdentityInput) -> Result<PullRequestRoutingIdentityResult, PullRequestError>;
    /// `withRoutingCredential(input, operation)`.
    async fn with_routing_credential(
        &self,
        input: PullRequestRef,
        operation: BoxFuture<'static, Result<Value, PullRequestError>>,
    ) -> Result<Value, PullRequestError>;
    async fn summary(&self, input: PullRequestRef) -> Result<PullRequestSummary, PullRequestError>;
    async fn stack(&self, input: PullRequestRef) -> Result<Option<PullRequestStack>, PullRequestError>;
    async fn detail(&self, input: PullRequestRef) -> Result<PullRequestDetail, PullRequestError>;
    async fn preview(&self, input: PullRequestRef) -> Result<PullRequestPreview, PullRequestError>;
    async fn activity(&self, input: PullRequestRef) -> Result<PullRequestActivity, PullRequestError>;
    async fn thread_comments(&self, input: PullRequestThreadCommentsInput) -> Result<PullRequestThreadCommentsResult, PullRequestError>;
    async fn diff_file_contents(&self, input: PullRequestDiffFileContentsInput) -> Result<PullRequestDiffFileContentsResult, PullRequestError>;
    async fn files_viewed(&self, input: PullRequestRef) -> Result<PullRequestFilesViewedResult, PullRequestError>;
    async fn set_files_viewed(&self, input: PullRequestSetFilesViewedInput) -> Result<(), PullRequestError>;
    async fn run_action(&self, input: PullRequestActionInput) -> Result<(), PullRequestError>;
    async fn update(&self, input: PullRequestUpdateInput) -> Result<(), PullRequestError>;
    async fn comment(&self, input: PullRequestCommentInput) -> Result<(), PullRequestError>;
    async fn update_comment(&self, input: PullRequestCommentUpdateInput) -> Result<(), PullRequestError>;
    async fn submit_review(&self, input: PullRequestSubmitReviewInput) -> Result<(), PullRequestError>;
    async fn reply_to_thread(&self, input: PullRequestThreadReplyInput) -> Result<(), PullRequestError>;
    async fn set_thread_resolution(&self, input: PullRequestThreadResolutionInput) -> Result<(), PullRequestError>;
    async fn set_reaction(&self, input: PullRequestReactionInput) -> Result<(), PullRequestError>;
    async fn reviewer_candidates(&self, input: PullRequestRef) -> Result<PullRequestReviewerCandidateList, PullRequestError>;
    async fn request_reviewers(&self, input: PullRequestReviewerRequestInput) -> Result<(), PullRequestError>;
    async fn label_candidates(&self, input: PullRequestRef) -> Result<PullRequestLabelCandidateList, PullRequestError>;
    async fn set_labels(&self, input: PullRequestLabelChangeInput) -> Result<(), PullRequestError>;
    /// `invalidate(input, {notifyReaders: true})`. Never fails.
    async fn invalidate(&self, input: PullRequestInvalidateInput);
    /// `subscribeRefreshes`.
    fn subscribe_refreshes(&self) -> EventStream<u64>;
}

/// The thread-link side of the handlers: `resolvePullRequestSyncKey` (ws.ts), the linked-thread
/// read and `PullRequestSyncReactor.requestSync`, keyed by a reference.
#[async_trait]
pub trait PullRequestLinks: Send + Sync + 'static {
    /// `listLinkedPullRequestThreads(key)`, or no threads when the ref has no sync key.
    async fn linked_threads(&self, reference: &PullRequestRef) -> Result<PullRequestLinkedThreadsResult, PullRequestError>;
    /// `pullRequestSync.requestSync(key)` when the ref has a sync key.
    async fn request_sync(&self, reference: &PullRequestRef);
}

/// Links that do nothing (no reactors): no linked threads, no sync.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoLinks;

#[async_trait]
impl PullRequestLinks for NoLinks {
    async fn linked_threads(&self, _reference: &PullRequestRef) -> Result<PullRequestLinkedThreadsResult, PullRequestError> {
        Ok(PullRequestLinkedThreadsResult { threads: Vec::new() })
    }
    async fn request_sync(&self, _reference: &PullRequestRef) {}
}

/// The production links: `resolvePullRequestSyncKey` over the projections, the linked-thread
/// query over the database, and the sync reactor when it runs.
pub struct ReactorLinks {
    pub projections: Arc<dyn zc_ports::ProjectionReads>,
    pub db: zc_db::Db,
    pub sync: Option<Arc<crate::reactors::sync::PullRequestSyncReactor>>,
}

#[async_trait]
impl PullRequestLinks for ReactorLinks {
    async fn linked_threads(&self, reference: &PullRequestRef) -> Result<PullRequestLinkedThreadsResult, PullRequestError> {
        match crate::sync_key::resolve_pull_request_sync_key(&*self.projections, reference).await {
            None => Ok(PullRequestLinkedThreadsResult { threads: Vec::new() }),
            Some(key) => crate::linked_threads::list_linked_pull_request_threads(&self.db, &key).await,
        }
    }

    async fn request_sync(&self, reference: &PullRequestRef) {
        let Some(sync) = &self.sync else { return };
        if let Some(key) = crate::sync_key::resolve_pull_request_sync_key(&*self.projections, reference).await {
            sync.request_sync(&key).await;
        }
    }
}

/// What the handlers need.
pub struct PullRequestRpcServices<S: PullRequestServiceApi> {
    pub service: Arc<S>,
    pub links: Arc<dyn PullRequestLinks>,
}

impl<S: PullRequestServiceApi> Clone for PullRequestRpcServices<S> {
    fn clone(&self) -> Self {
        Self {
            service: self.service.clone(),
            links: self.links.clone(),
        }
    }
}

fn options(rpc: Rpc) -> MethodOptions {
    MethodOptions::default().scope(ScopeRule::required(rpc.spec().scope.as_str()))
}

fn encode<T: Serialize>(value: &T) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(|error| RpcError::die(format!("could not encode the result: {error}")))
}

fn fail(error: PullRequestError) -> RpcError {
    RpcError::Fail(error.to_wire())
}

fn decode_payload<T: DeserializeOwned>(schema: PayloadSchema, payload: Value) -> Result<T, RpcError> {
    decode(schema, payload).map_err(|issue| RpcError::die_text(issue.0))
}

/// The `PullRequestRef` fields of an input that spreads them.
fn reference_of<T: Serialize>(input: &T) -> Result<PullRequestRef, RpcError> {
    let value = serde_json::to_value(input).map_err(|error| RpcError::die(error.to_string()))?;
    let mut reference = serde_json::Map::new();
    if let Value::Object(fields) = value {
        for key in ["projectId", "host", "expectedAccountId", "allowStale", "repository", "number"] {
            if let Some(field) = fields.get(key) {
                reference.insert(key.to_owned(), field.clone());
            }
        }
    }
    serde_json::from_value(Value::Object(reference)).map_err(|error| RpcError::die(error.to_string()))
}

/// Registers a unary method: decode, run, encode.
fn unary<S, I, O, F, Fut>(builder: RpcRouterBuilder, rpc: Rpc, schema: PayloadSchema, services: &PullRequestRpcServices<S>, handler: F) -> RpcRouterBuilder
where
    S: PullRequestServiceApi,
    I: DeserializeOwned + Send + 'static,
    O: Serialize + Send + 'static,
    F: Fn(PullRequestRpcServices<S>, I) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<O, PullRequestError>> + Send + 'static,
{
    let services = services.clone();
    let handler = Arc::new(handler);
    builder.unary_with(rpc.tag(), options(rpc), move |_ctx, payload| {
        let services = services.clone();
        let handler = handler.clone();
        async move {
            let input: I = decode_payload(schema, payload)?;
            tracing::debug!(rpc = rpc.tag(), "rpc.aggregate" = "pull-requests", "pull request rpc");
            let output = handler(services, input).await.map_err(fail)?;
            encode(&output)
        }
    })
}

/// Registers a method run inside `withPullRequestViewer`.
fn viewed<S, I, O, F, Fut>(builder: RpcRouterBuilder, rpc: Rpc, schema: PayloadSchema, services: &PullRequestRpcServices<S>, handler: F) -> RpcRouterBuilder
where
    S: PullRequestServiceApi,
    I: DeserializeOwned + Serialize + Send + 'static,
    O: Serialize + Send + 'static,
    F: Fn(PullRequestRpcServices<S>, I) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<O, PullRequestError>> + Send + 'static,
{
    viewed_then(builder, rpc, schema, services, handler, false)
}

/// [`viewed`], then (`sync_after`) a sync request for the ref once the call succeeded, outside
/// the viewer scope (`withPullRequestViewer(…).pipe(Effect.tap(requestSync))`).
fn viewed_then<S, I, O, F, Fut>(
    builder: RpcRouterBuilder,
    rpc: Rpc,
    schema: PayloadSchema,
    services: &PullRequestRpcServices<S>,
    handler: F,
    sync_after: bool,
) -> RpcRouterBuilder
where
    S: PullRequestServiceApi,
    I: DeserializeOwned + Serialize + Send + 'static,
    O: Serialize + Send + 'static,
    F: Fn(PullRequestRpcServices<S>, I) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<O, PullRequestError>> + Send + 'static,
{
    let services = services.clone();
    let handler = Arc::new(handler);
    builder.unary_with(rpc.tag(), options(rpc), move |_ctx, payload| {
        let services = services.clone();
        let handler = handler.clone();
        async move {
            let input: I = decode_payload(schema, payload)?;
            let reference = reference_of(&input)?;
            tracing::debug!(rpc = rpc.tag(), "rpc.aggregate" = "pull-requests", "pull request rpc");
            let operation = {
                let services = services.clone();
                async move {
                    let output = handler(services, input).await?;
                    serde_json::to_value(&output).map_err(|error| PullRequestError::operation("encode", error.to_string()))
                }
            };
            let output = services
                .service
                .with_routing_credential(reference.clone(), Box::pin(operation))
                .await
                .map_err(fail)?;
            if sync_after {
                services.links.request_sync(&reference).await;
            }
            Ok(output)
        }
    })
}

/// Registers every `pullRequests.*` method.
pub fn register<S: PullRequestServiceApi>(builder: RpcRouterBuilder, services: PullRequestRpcServices<S>) -> RpcRouterBuilder {
    use PayloadSchema as P;
    let s = &services;
    let b = unary(
        builder,
        Rpc::PullRequestsList,
        P::List,
        s,
        |s, input| async move { s.service.list(input).await },
    );
    let b = unary(b, Rpc::PullRequestsListStats, P::ListStats, s, |s, input| async move {
        s.service.list_stats(input).await
    });
    let b = unary(b, Rpc::PullRequestsRoutingIdentity, P::RoutingIdentity, s, |s, input| async move {
        s.service.routing_identity(input).await
    });
    let b = unary(b, Rpc::PullRequestsRouting, P::Ref, s, |s, input| async move { s.service.routing(input).await });
    let b = viewed(b, Rpc::PullRequestsSummary, P::Ref, s, |s, input| async move { s.service.summary(input).await });
    let b = viewed(b, Rpc::PullRequestsStack, P::Ref, s, |s, input| async move { s.service.stack(input).await });
    let b = unary(b, Rpc::PullRequestsLinkedThreads, P::Ref, s, |s, input: PullRequestRef| async move {
        s.links.linked_threads(&input).await
    });
    let b = viewed(b, Rpc::PullRequestsDetail, P::Ref, s, |s, input| async move { s.service.detail(input).await });
    let b = viewed(b, Rpc::PullRequestsPreview, P::Ref, s, |s, input| async move { s.service.preview(input).await });
    let b = viewed(
        b,
        Rpc::PullRequestsActivity,
        P::Ref,
        s,
        |s, input| async move { s.service.activity(input).await },
    );
    let b = viewed(b, Rpc::PullRequestsThreadComments, P::ThreadComments, s, |s, input| async move {
        s.service.thread_comments(input).await
    });
    let b = viewed(b, Rpc::PullRequestsDiffFileContents, P::DiffFileContents, s, |s, input| async move {
        s.service.diff_file_contents(input).await
    });
    let b = viewed(b, Rpc::PullRequestsFilesViewed, P::Ref, s, |s, input| async move {
        s.service.files_viewed(input).await
    });
    let b = viewed(b, Rpc::PullRequestsSetFilesViewed, P::SetFilesViewed, s, |s, input| async move {
        s.service.set_files_viewed(input).await
    });
    // ws.ts: `withPullRequestViewer(input, runAction(input)).pipe(Effect.tap(requestSync))`.
    let b = viewed_then(
        b,
        Rpc::PullRequestsRunAction,
        P::Action,
        s,
        |s, input| async move { s.service.run_action(input).await },
        true,
    );
    let b = viewed(
        b,
        Rpc::PullRequestsUpdate,
        P::Update,
        s,
        |s, input| async move { s.service.update(input).await },
    );
    let b = viewed(
        b,
        Rpc::PullRequestsComment,
        P::Comment,
        s,
        |s, input| async move { s.service.comment(input).await },
    );
    let b = viewed(b, Rpc::PullRequestsUpdateComment, P::CommentUpdate, s, |s, input| async move {
        s.service.update_comment(input).await
    });
    let b = viewed(b, Rpc::PullRequestsSubmitReview, P::SubmitReview, s, |s, input| async move {
        s.service.submit_review(input).await
    });
    let b = viewed(b, Rpc::PullRequestsReplyToThread, P::ThreadReply, s, |s, input| async move {
        s.service.reply_to_thread(input).await
    });
    let b = viewed(b, Rpc::PullRequestsSetThreadResolution, P::ThreadResolution, s, |s, input| async move {
        s.service.set_thread_resolution(input).await
    });
    let b = viewed(b, Rpc::PullRequestsSetReaction, P::Reaction, s, |s, input| async move {
        s.service.set_reaction(input).await
    });
    let b = unary(
        b,
        Rpc::PullRequestsInvalidate,
        P::Invalidate,
        s,
        |s, input: PullRequestInvalidateInput| async move {
            let reference = input.reference.clone();
            let files_viewed_only = input.files_viewed_only == Some(true);
            s.service.invalidate(input).await;
            // A reader asking for fresh host state also wants the thread badges it feeds to catch up.
            if let Some(reference) = reference.filter(|_| !files_viewed_only) {
                s.links.request_sync(&reference).await;
            }
            Ok(())
        },
    );
    let b = viewed(b, Rpc::PullRequestsReviewerCandidates, P::Ref, s, |s, input| async move {
        s.service.reviewer_candidates(input).await
    });
    let b = viewed(b, Rpc::PullRequestsRequestReviewers, P::ReviewerRequest, s, |s, input| async move {
        s.service.request_reviewers(input).await
    });
    let b = viewed(b, Rpc::PullRequestsLabelCandidates, P::Ref, s, |s, input| async move {
        s.service.label_candidates(input).await
    });
    let b = viewed(b, Rpc::PullRequestsSetLabels, P::LabelChange, s, |s, input| async move {
        s.service.set_labels(input).await
    });
    let refreshes = services.clone();
    let rpc = Rpc::PullRequestsSubscribeRefreshes;
    b.stream_with(rpc.tag(), options(rpc), move |_ctx, payload| {
        let services = refreshes.clone();
        async move {
            let _: PullRequestsSubscribeRefreshesPayload = decode_payload(PayloadSchema::Empty, payload)?;
            Ok(services.service.subscribe_refreshes().map(|count| Ok(Value::from(count))))
        }
    })
}
