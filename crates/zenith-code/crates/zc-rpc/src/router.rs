//! The method table: handlers registered by tag, with their scope and ack options.

use std::any::Any;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use futures::future::BoxFuture;
use futures::{FutureExt, Stream, TryStreamExt};
use serde_json::Value;

use crate::context::RequestContext;
use crate::error::RpcError;

/// The items of a stream handler: encoded values, or the error that ends the stream.
pub type BoxValueStream = Pin<Box<dyn Stream<Item = Result<Value, RpcError>> + Send>>;

/// A decoded payload, type-erased (a `Value` for untyped handlers, the method's payload
/// type for typed ones).
pub(crate) type ErasedPayload = Box<dyn Any + Send>;
pub(crate) type DecodeFn = Arc<dyn Fn(Value) -> Result<ErasedPayload, String> + Send + Sync>;
pub(crate) type UnaryFn = Arc<dyn Fn(RequestContext, ErasedPayload) -> BoxFuture<'static, Result<Value, RpcError>> + Send + Sync>;
pub(crate) type StreamFn = Arc<dyn Fn(RequestContext, ErasedPayload) -> BoxFuture<'static, Result<BoxValueStream, RpcError>> + Send + Sync>;

pub(crate) enum Handler {
    Unary(UnaryFn),
    Stream(StreamFn),
}

/// How many chunks of a stream may be on the wire without the client's `Ack`.
///
/// Effect RPC waits for an `Ack` after every chunk ([`AckWindow::PER_CHUNK`]). The TS
/// server makes one exception, for terminal output (`terminal/OutputProtocol.ts`):
/// up to 8 unacknowledged chunks or 64 KiB ([`AckWindow::TERMINAL`]), with the server
/// acknowledging for the client while the window has room.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AckWindow {
    pub max_chunks: usize,
    /// Measured on the encoded `Chunk` frame, like the TS server's `JSON.stringify` size.
    pub max_bytes: usize,
}

impl AckWindow {
    pub const PER_CHUNK: Self = Self {
        max_chunks: 1,
        max_bytes: usize::MAX,
    };
    pub const TERMINAL: Self = Self {
        max_chunks: 8,
        max_bytes: 64 * 1024,
    };
}

impl Default for AckWindow {
    fn default() -> Self {
        Self::PER_CHUNK
    }
}

/// Which scope a method needs.
#[derive(Clone)]
pub enum ScopeRule {
    /// No check.
    Public,
    Required(String),
    /// Computed from the raw payload (`device.list` needs `orchestration:operate` only
    /// when `retryHostId` or `updateTool` is set).
    Dynamic(Arc<dyn Fn(&Value) -> String + Send + Sync>),
}

impl ScopeRule {
    pub fn required(scope: impl Into<String>) -> Self {
        Self::Required(scope.into())
    }

    pub fn dynamic(rule: impl Fn(&Value) -> String + Send + Sync + 'static) -> Self {
        Self::Dynamic(Arc::new(rule))
    }

    pub(crate) fn scope_for(&self, payload: &Value) -> Option<String> {
        match self {
            Self::Public => None,
            Self::Required(scope) => Some(scope.clone()),
            Self::Dynamic(rule) => Some(rule(payload)),
        }
    }
}

impl std::fmt::Debug for ScopeRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Public => f.write_str("Public"),
            Self::Required(scope) => write!(f, "Required({scope})"),
            Self::Dynamic(_) => f.write_str("Dynamic(..)"),
        }
    }
}

/// Method tag → required scope, as data (`RPC_REQUIRED_SCOPES`; zc-contracts will
/// generate it).
#[derive(Clone, Debug, Default)]
pub struct ScopeTable(HashMap<String, String>);

impl ScopeTable {
    pub fn get(&self, tag: &str) -> Option<&str> {
        self.0.get(tag).map(String::as_str)
    }
}

impl<K: Into<String>, V: Into<String>> FromIterator<(K, V)> for ScopeTable {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        Self(iter.into_iter().map(|(k, v)| (k.into(), v.into())).collect())
    }
}

/// Per-method options.
#[derive(Clone, Debug, Default)]
pub struct MethodOptions {
    /// Overrides the scope table for this method.
    pub scope: Option<ScopeRule>,
    /// Streams only.
    pub ack_window: AckWindow,
}

impl MethodOptions {
    pub fn scope(mut self, rule: ScopeRule) -> Self {
        self.scope = Some(rule);
        self
    }

    pub fn ack_window(mut self, window: AckWindow) -> Self {
        self.ack_window = window;
        self
    }
}

pub(crate) struct Method {
    pub(crate) tag: Arc<str>,
    pub(crate) decode: DecodeFn,
    pub(crate) handler: Handler,
    pub(crate) scope: ScopeRule,
    pub(crate) ack_window: AckWindow,
}

/// A registration mistake, reported by [`RpcRouterBuilder::build`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RouterError {
    Duplicate(String),
    /// A scope table is set and this method is neither in it nor given a scope
    /// (the TS server makes this a type error).
    MissingScope(String),
    /// A typed method registered with the wrong kind (`stream` for a unary method…).
    WrongKind(String),
}

impl std::fmt::Display for RouterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Duplicate(tag) => write!(f, "RPC method {tag} is registered twice"),
            Self::MissingScope(tag) => {
                write!(f, "RPC method {tag} has no declared authorization scope")
            }
            Self::WrongKind(tag) => write!(f, "RPC method {tag} is registered with the wrong kind"),
        }
    }
}

impl std::error::Error for RouterError {}

struct Pending {
    tag: String,
    decode: DecodeFn,
    handler: Handler,
    options: MethodOptions,
}

/// Builds an [`RpcRouter`].
///
/// ```ignore
/// let router = RpcRouter::builder()
///     .scopes(ScopeTable::from_iter([("server.probe", "orchestration:read")]))
///     .unary("server.probe", |_ctx, _payload| async { Ok(json!({})) })
///     .stream_with("terminal.attach", MethodOptions::default().ack_window(AckWindow::TERMINAL),
///         |_ctx, payload| async move { Ok(futures::stream::iter([Ok(payload)])) })
///     .build()?;
/// ```
#[derive(Default)]
pub struct RpcRouterBuilder {
    methods: Vec<Pending>,
    scopes: Option<ScopeTable>,
    errors: Vec<RouterError>,
}

impl RpcRouterBuilder {
    /// Every method then needs a scope, from this table or from its options; methods in
    /// neither make `build` fail. Without a table, methods without options are public.
    pub fn scopes(mut self, table: ScopeTable) -> Self {
        self.scopes = Some(table);
        self
    }

    /// A unary method over raw JSON: the payload as the client encoded it, the success
    /// value as it must go on the wire.
    pub fn unary<F, Fut>(self, tag: impl Into<String>, handler: F) -> Self
    where
        F: Fn(RequestContext, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value, RpcError>> + Send + 'static,
    {
        self.unary_with(tag, MethodOptions::default(), handler)
    }

    pub fn unary_with<F, Fut>(self, tag: impl Into<String>, options: MethodOptions, handler: F) -> Self
    where
        F: Fn(RequestContext, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value, RpcError>> + Send + 'static,
    {
        let handler: UnaryFn = Arc::new(move |ctx, payload| handler(ctx, downcast_value(payload)).boxed());
        self.push(tag.into(), value_decode(), Handler::Unary(handler), options)
    }

    /// A stream method over raw JSON. The future sets the stream up (its error fails the
    /// request before any chunk); each `Err` item ends the stream with that failure.
    pub fn stream<F, Fut, S>(self, tag: impl Into<String>, handler: F) -> Self
    where
        F: Fn(RequestContext, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<S, RpcError>> + Send + 'static,
        S: Stream<Item = Result<Value, RpcError>> + Send + 'static,
    {
        self.stream_with(tag, MethodOptions::default(), handler)
    }

    pub fn stream_with<F, Fut, S>(self, tag: impl Into<String>, options: MethodOptions, handler: F) -> Self
    where
        F: Fn(RequestContext, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<S, RpcError>> + Send + 'static,
        S: Stream<Item = Result<Value, RpcError>> + Send + 'static,
    {
        let handler: StreamFn = Arc::new(move |ctx, payload| handler(ctx, downcast_value(payload)).map(|r| r.map(|s| Box::pin(s) as BoxValueStream)).boxed());
        self.push(tag.into(), value_decode(), Handler::Stream(handler), options)
    }

    /// A unary method with typed payload, success and error (see [`crate::typed`]).
    pub fn typed_unary<M, F, Fut>(self, handler: F) -> Self
    where
        M: crate::typed::RpcMethod,
        F: Fn(RequestContext, M::Payload) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<M::Success, crate::typed::Failure<M::Error>>> + Send + 'static,
    {
        self.typed_unary_with::<M, F, Fut>(MethodOptions::default(), handler)
    }

    pub fn typed_unary_with<M, F, Fut>(mut self, options: MethodOptions, handler: F) -> Self
    where
        M: crate::typed::RpcMethod,
        F: Fn(RequestContext, M::Payload) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<M::Success, crate::typed::Failure<M::Error>>> + Send + 'static,
    {
        if M::STREAM {
            self.errors.push(RouterError::WrongKind(M::TAG.into()));
            return self;
        }
        let handler: UnaryFn = Arc::new(move |ctx, payload| {
            let payload = *payload.downcast::<M::Payload>().expect("payload decoded for this method");
            handler(ctx, payload)
                .map(|r| match r {
                    Ok(success) => crate::typed::encode_success(&success),
                    Err(failure) => Err(failure.into_rpc_error()),
                })
                .boxed()
        });
        self.push(M::TAG.into(), crate::typed::decode_fn::<M::Payload>(), Handler::Unary(handler), options)
    }

    /// A stream method with typed payload, items and error.
    pub fn typed_stream<M, F, Fut, S>(self, handler: F) -> Self
    where
        M: crate::typed::RpcMethod,
        F: Fn(RequestContext, M::Payload) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<S, crate::typed::Failure<M::Error>>> + Send + 'static,
        S: Stream<Item = Result<M::Success, crate::typed::Failure<M::Error>>> + Send + 'static,
    {
        self.typed_stream_with::<M, F, Fut, S>(MethodOptions::default(), handler)
    }

    pub fn typed_stream_with<M, F, Fut, S>(mut self, options: MethodOptions, handler: F) -> Self
    where
        M: crate::typed::RpcMethod,
        F: Fn(RequestContext, M::Payload) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<S, crate::typed::Failure<M::Error>>> + Send + 'static,
        S: Stream<Item = Result<M::Success, crate::typed::Failure<M::Error>>> + Send + 'static,
    {
        if !M::STREAM {
            self.errors.push(RouterError::WrongKind(M::TAG.into()));
            return self;
        }
        let handler: StreamFn = Arc::new(move |ctx, payload| {
            let payload = *payload.downcast::<M::Payload>().expect("payload decoded for this method");
            handler(ctx, payload)
                .map(|r| match r {
                    Ok(stream) => Ok(Box::pin(
                        stream
                            .map_err(crate::typed::Failure::into_rpc_error)
                            .and_then(|item| futures::future::ready(crate::typed::encode_success(&item))),
                    ) as BoxValueStream),
                    Err(failure) => Err(failure.into_rpc_error()),
                })
                .boxed()
        });
        self.push(M::TAG.into(), crate::typed::decode_fn::<M::Payload>(), Handler::Stream(handler), options)
    }

    /// Whether a handler is already registered for `tag` (so assemblies can fill in the
    /// methods nobody implements yet without registering one twice).
    pub fn is_registered(&self, tag: &str) -> bool {
        self.methods.iter().any(|pending| pending.tag == tag)
    }

    fn push(mut self, tag: String, decode: DecodeFn, handler: Handler, options: MethodOptions) -> Self {
        self.methods.push(Pending { tag, decode, handler, options });
        self
    }

    pub fn build(self) -> Result<RpcRouter, RouterError> {
        if let Some(error) = self.errors.into_iter().next() {
            return Err(error);
        }
        let mut methods = HashMap::new();
        for pending in self.methods {
            let scope = match (&pending.options.scope, &self.scopes) {
                (Some(rule), _) => rule.clone(),
                (None, Some(table)) => match table.get(&pending.tag) {
                    Some(scope) => ScopeRule::Required(scope.to_owned()),
                    None => return Err(RouterError::MissingScope(pending.tag)),
                },
                (None, None) => ScopeRule::Public,
            };
            let tag: Arc<str> = pending.tag.as_str().into();
            let method = Method {
                tag,
                decode: pending.decode,
                handler: pending.handler,
                scope,
                ack_window: pending.options.ack_window,
            };
            if methods.insert(pending.tag.clone(), Arc::new(method)).is_some() {
                return Err(RouterError::Duplicate(pending.tag));
            }
        }
        Ok(RpcRouter { methods })
    }
}

/// The registered methods, looked up by request tag.
pub struct RpcRouter {
    methods: HashMap<String, Arc<Method>>,
}

impl RpcRouter {
    pub fn builder() -> RpcRouterBuilder {
        RpcRouterBuilder::default()
    }

    pub(crate) fn get(&self, tag: &str) -> Option<&Arc<Method>> {
        self.methods.get(tag)
    }

    pub fn contains(&self, tag: &str) -> bool {
        self.methods.contains_key(tag)
    }

    pub fn tags(&self) -> impl Iterator<Item = &str> {
        self.methods.keys().map(String::as_str)
    }

    /// Whether `tag` is registered as a stream.
    pub fn is_stream(&self, tag: &str) -> Option<bool> {
        self.methods.get(tag).map(|m| matches!(m.handler, Handler::Stream(_)))
    }
}

fn value_decode() -> DecodeFn {
    Arc::new(|value| Ok(Box::new(value) as ErasedPayload))
}

fn downcast_value(payload: ErasedPayload) -> Value {
    *payload.downcast::<Value>().expect("untyped payloads are JSON values")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_table_is_enforced_at_build() {
        let err = RpcRouter::builder()
            .scopes(ScopeTable::from_iter([("a", "orchestration:read")]))
            .unary("a", |_, v| async move { Ok(v) })
            .unary("b", |_, v| async move { Ok(v) })
            .build()
            .err();
        assert_eq!(err, Some(RouterError::MissingScope("b".into())));

        let router = RpcRouter::builder()
            .scopes(ScopeTable::from_iter([("a", "orchestration:read")]))
            .unary("a", |_, v| async move { Ok(v) })
            .unary_with("b", MethodOptions::default().scope(ScopeRule::Public), |_, v| async move { Ok(v) })
            .build()
            .unwrap();
        assert!(matches!(&router.get("a").unwrap().scope, ScopeRule::Required(s) if s == "orchestration:read"));
        assert!(matches!(router.get("b").unwrap().scope, ScopeRule::Public));
    }

    #[test]
    fn duplicates_are_rejected() {
        let err = RpcRouter::builder()
            .unary("a", |_, v| async move { Ok(v) })
            .stream("a", |_, v| async move { Ok(futures::stream::iter([Ok(v)])) })
            .build()
            .err();
        assert_eq!(err, Some(RouterError::Duplicate("a".into())));
    }
}
