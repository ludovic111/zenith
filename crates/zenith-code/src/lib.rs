//! zenith code's server, in Rust (see docs/zenith-code-rust-plan.md). The crates under
//! `crates/` hold the parts; this crate wires them together.

pub mod app;
pub mod cli;
pub mod project_cli;
pub mod serve;
pub mod server;
