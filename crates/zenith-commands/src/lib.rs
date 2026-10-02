//! zenith's command registry and what goes with it, shared by the window (`zenith-app`),
//! `zenith-cli` and `zenith-mcp` (lsuite standard §1–§4):
//!
//! - [`registry`]: every action as a `family.verb` command, JSON in and out, validated in one
//!   place, with the CLI help, the MCP tool list and `docs/COMMANDS.md` generated from it;
//! - [`permissions`]: what agents may run through MCP;
//! - [`agent`]: the server's LaunchAgent, run from the installed app;
//! - [`lsuite`]: `~/.lsuite/apps` discovery;
//! - [`update`]: signed updates from GitHub Releases.

pub mod agent;
pub mod lsuite;
pub mod permissions;
pub mod registry;
pub mod update;

pub use registry::{run, Caller, CommandError, COMMANDS};
