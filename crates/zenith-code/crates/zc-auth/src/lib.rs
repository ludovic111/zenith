//! zc-auth: zenith code's authentication, the port of `apps/server/src/auth/**`,
//! `cliAuthFormat.ts` and the auth half of `ws.ts` (plan §2, WP-06).
//!
//! | Module | Ported from |
//! |---|---|
//! | [`token`] | `auth/utils.ts` (base64url, HMAC), the claims of `SessionStore.ts` |
//! | [`cookies`] | `auth/utils.ts` cookie names, Effect `Cookies` parse/serialize |
//! | [`client_metadata`] | `deriveAuthClientMetadata` |
//! | [`session_store`] | `auth/SessionStore.ts` |
//! | [`pairing`] | `auth/PairingGrantStore.ts` |
//! | [`dpop`] | `auth/dpop.ts`, `shared/dpop.ts`, `auth/replayMarkers.ts` |
//! | [`policy`] | `auth/EnvironmentAuthPolicy.ts` |
//! | [`environment_auth`] | `auth/EnvironmentAuth.ts` |
//! | [`http`] | `auth/http.ts` (the auth HTTP group, the authenticated-route middleware) |
//! | [`ws`] | the `/ws` authenticator and `subscribeAuthAccess` of `ws.ts` |
//! | [`scopes`] | `auth/RpcAuthorization.ts`, the scope constants of `contracts/auth.ts` |
//! | [`cli_format`] | `cliAuthFormat.ts` |
//!
//! Wiring, in short:
//!
//! ```ignore
//! let auth = Arc::new(EnvironmentAuth::open(db, secrets, cookie_input, system_clock()).await?);
//! let rpc = register_auth_access(RpcRouter::builder().scopes(rpc_scope_table()), auth.clone());
//! let app = Router::new().merge(zc_auth::http::routes(auth.clone()));
//! let ws_auth: Arc<dyn WsAuthenticator> = Arc::new(AuthWsAuthenticator::new(auth));
//! ```
//!
//! Not ported (zenith never runs them): the desktop bootstrap grant (`--bootstrap-fd`) and the
//! reusable dev token (`T3CODE_DEV_AUTH_TOKEN`).

// axum rejections are `Response`s; boxing them would only add noise at every call site.
#![allow(clippy::result_large_err)]

pub mod cli_format;
pub mod client_metadata;
pub mod clock;
pub mod cookies;
pub mod dpop;
pub mod environment_auth;
pub mod error;
pub mod http;
pub mod pairing;
pub mod policy;
pub mod scopes;
pub mod session_store;
pub mod token;
pub mod ws;

pub use clock::{system_clock, Clock, SharedClock, SystemClock, TestClock};
pub use cookies::{CookieNameInput, ServerMode};
pub use environment_auth::{
    AuthRequest, AuthenticatedSession, CreatePairingLinkInput, EnvironmentAuth, IssueBearerSessionInput, IssuedBearerSession, IssuedPairingLink,
    INTERNAL_ADMINISTRATIVE_BOOTSTRAP_SUBJECT,
};
pub use error::{BootstrapCredentialError, ServerAuthError, SessionCredentialError};
pub use pairing::PairingGrantStore;
pub use scopes::{rpc_method_options, rpc_scope_table, ADMINISTRATIVE_SCOPES, STANDARD_CLIENT_SCOPES};
pub use session_store::SessionStore;
pub use ws::{register_auth_access, AuthWsAuthenticator};
