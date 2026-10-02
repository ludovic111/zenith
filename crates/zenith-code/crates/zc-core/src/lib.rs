//! zc-core: the foundation every zenith code crate shares (WP-04 of
//! `docs/zenith-code-rust-plan.md`).
//!
//! | Module | Ported from |
//! |---|---|
//! | [`config`] | `config.ts` (`deriveServerPaths`, `ensureServerDirectories`, `resolveStaticDir`), `cli/config.ts` (`resolveServerConfig`, `T3CODE_*`, durations), `os-jank.ts` `resolveBaseDir`, shared `Net.ts` `findAvailablePort` |
//! | [`paths`] | `pathExpansion.ts`, `os-jank.ts` `expandHomePath`, Node `path.resolve`, `os.homedir()` |
//! | [`shell_env`] | `os-jank.ts` `fixPath`, shared `shell.ts` (login-shell markers, `mergePathEntries`, launchctl) |
//! | [`process`] | `processRunner.ts`, `stream/collectUint8StreamText.ts`, Effect `NodeChildProcessSpawner` kill semantics |
//! | [`vcs_process`] | `vcs/VcsProcess.ts` + the `VcsError` process members of `contracts/vcs.ts` |
//! | [`secrets`] | `auth/ServerSecretStore.ts` |
//! | [`atomic_write`] | `atomicWrite.ts`, Effect `makeTempDirectory` |
//! | [`lenient_json`] | shared `schemaJson.ts` (`fromLenientJson`, `extractJsonObject`) |
//! | [`time`] | `Date.prototype.toISOString` / `DateTime.formatIso` |
//! | [`ids`] | `crypto.randomUUID`, `Crypto.randomBytes`, command-id conventions |
//! | [`pubsub`] | Effect `PubSub.unbounded`, `utils/subscribeBeforeSnapshot.ts` |
//! | [`cache`] | Effect `Cache` (capacity + TTL + single flight) |
//! | [`environment_id`] | `environment/ServerEnvironment.ts` (`ServerEnvironmentIdentity`) |
//! | [`runtime_state`] | `serverRuntimeState.ts`, `startupAccess.ts` host helpers |
//! | [`defect`] | `Schema.Defect()` encoding, JS string length |
//! | [`lease`] | `workspace/workspaceLease.ts` (`withWorkspaceLease`), moved here from zc-terminal |

// The process errors mirror the wire's tagged errors field for field (callers match on them and
// serialize them), and one is produced per subprocess at most, so boxing them buys nothing.
#![allow(clippy::result_large_err)]

pub mod atomic_write;
pub mod cache;
pub mod config;
pub mod defect;
pub mod environment_id;
pub mod ids;
pub mod lease;
pub mod lenient_json;
pub mod lsuite;
pub mod paths;
pub mod process;
pub mod pubsub;
pub mod runtime_state;
pub mod secrets;
pub mod shell_env;
pub mod time;
pub mod vcs_process;

pub use atomic_write::{write_file_atomically, write_file_string_atomically};
pub use config::{derive_server_paths, resolve_base_dir, ServerConfig, ServerDerivedPaths};
pub use defect::Defect;
pub use ids::uuid_v4;
pub use lenient_json::{from_lenient_json, parse_lenient_json};
pub use paths::expand_home_path;
pub use process::{run_process, ProcessRunError, ProcessRunInput, ProcessRunOutput, ProcessRunner};
pub use pubsub::{PubSub, SnapshotHub, Subscription};
pub use secrets::ServerSecretStore;
pub use time::{iso_from_millis, now_iso, now_millis};
pub use vcs_process::{VcsProcess, VcsProcessError, VcsProcessInput, VcsProcessOutput};
