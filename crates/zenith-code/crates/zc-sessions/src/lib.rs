//! zc-sessions: every Claude Code and Codex session of this Mac, read from their own logs
//! (`~/.claude/projects/**/<id>.jsonl`, `~/.codex/sessions/**/*.jsonl`), and the
//! `GET /api/zenith/sessions` route of zenith code's Sessions page.
//!
//! A port of the former zenith dashboard's session reader (`agents.rs` / `agents.ts`), with
//! zenith's config replaced by zenith code's projects: a session belongs to the project
//! whose folder holds its working directory ([`ProjectRoot`], deepest folder wins).
//!
//! - [`parse`]: the two log formats, read line by line into running state;
//! - [`reader`]: finding the files, incremental reads, the 20 s cache ([`SessionReader`]);
//! - [`overview`]: the page's totals (live, today, this week, per day, per project);
//! - [`http`]: the route, its auth hook and its project source.
//!
//! # Mounting
//!
//! ```ignore
//! let api = zc_sessions::SessionsApi {
//!     reader: Arc::new(zc_sessions::SessionReader::new(zc_sessions::ReaderConfig::from_env())),
//!     projects: Arc::new(my_project_roots),   // impl ProjectRoots
//!     auth: Arc::new(my_http_auth),           // impl HttpAuthenticator
//! };
//! let routes = zc_sessions::router(api);      // merged inside the server's layers
//! ```
//!
//! # Auth
//!
//! The route calls [`HttpAuthenticator::authenticate`] with the request's headers, URI and
//! peer, then requires the `orchestration:read` scope. When zc-auth lands it implements
//! `HttpAuthenticator` for its session service (cookie → Bearer → DPoP, the same selection
//! as every typed endpoint), or the server passes its `/ws` authenticator through
//! [`WsAuthBridge`] if that one already reads cookies and bearer tokens. The dev server uses
//! `WsAuthBridge(DevAuthenticator)`.
//!
//! # Projects
//!
//! [`ProjectRoots::project_roots`] is called on each request (cheap: the session list is
//! cached, the mapping is not). The server's implementation should return each live
//! project's `workspaceRoot`, plus one entry per thread worktree outside it (same project
//! id), so sessions started in zenith code worktrees land in their project.

pub mod http;
pub mod model;
pub mod overview;
pub mod parse;
pub mod reader;

pub use http::{router, HttpAuthRequest, HttpAuthenticator, ProjectRoots, SessionsApi, StaticProjectRoots, WsAuthBridge, REQUIRED_SCOPE, SESSIONS_PATH};
pub use model::{project_of, Agent, Pr, ProjectRef, ProjectRoot, Session, LIVE_WINDOW_MS};
pub use overview::{overview, totals, DayCount, Overview, ProjectTotal, Totals, WeekTotals};
pub use reader::{ReaderConfig, SessionReader};
