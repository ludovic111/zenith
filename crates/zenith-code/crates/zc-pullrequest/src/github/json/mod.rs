//! `pullRequest/gitHubPullRequestJson.ts`: the GraphQL documents and `gh --json` field lists the
//! GitHub CLI provider sends, the request bodies it builds, and the decoders that turn `gh`
//! output into the neutral provider types and the contract types.
//!
//! | Submodule | What |
//! |---|---|
//! | [`queries`] | every document/field-list constant (byte-identical to the TS), the `build*` request builders |
//! | [`types`] | the exported interfaces (and the decoders' anonymous result shapes) as structs |
//! | [`schema`] | [`DecodeFailure`] and the Effect-`Schema`-strict JSON reading the decoders are built on |
//! | `raw` | the `Raw*Schema`s: what each `gh` answer must look like |
//! | [`decode`] | the `decode*Json` functions, `reviewThreadConversation`, `gitHubReactionContent`, `quoteGitPatchPath` |
//!
//! Every name keeps its TS export's name in Rust style (`decodePullRequestListJson` →
//! [`decode_pull_request_list_json`], `PULL_REQUEST_CORE_GRAPHQL_QUERY` stays as is), and a TS
//! `Result.Result<A, DecodeFailure>` is a `Result<A, DecodeFailure>`.

pub mod decode;
pub mod queries;
mod raw;
pub mod schema;
pub mod types;

pub use decode::*;
pub use queries::*;
pub use schema::{DecodeFailure, PathSegment};
pub use types::*;
