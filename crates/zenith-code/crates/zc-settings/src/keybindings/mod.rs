//! Keybindings: `apps/server/src/keybindings.ts` and `packages/shared/src/keybindings.ts`.
//!
//! - [`when`]: the `when` expression parser and encoder.
//! - [`rules`]: defaults, shortcuts, compilation, merging, the rule schema checks.
//! - [`service`]: `keybindings.json` (defaults back-fill, watch, upsert/remove, issues).

pub mod rules;
pub mod service;
pub mod when;

pub use rules::{
    compile_resolved_keybinding_rule, compile_resolved_keybindings_config, default_keybindings, default_resolved_keybindings, merge_with_default_keybindings,
    parse_keybinding_shortcut, KeybindingRule, MAX_KEYBINDINGS_COUNT,
};
pub use service::{keybindings_config_json, KeybindingsConfigState, KeybindingsService};
pub use when::{encode_when_ast, parse_keybinding_when_expression, MAX_WHEN_EXPRESSION_DEPTH};
