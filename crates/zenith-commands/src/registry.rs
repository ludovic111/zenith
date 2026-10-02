//! The command registry: every action on zenith as a named command (`family.verb`, JSON
//! parameters in, JSON result out, validated here), run against the server through
//! `zenith-client`. The window, `zenith-cli` and `zenith-mcp` all call [`run`]; the CLI's
//! help, the MCP tool list and `docs/COMMANDS.md` are generated from [`COMMANDS`].
//!
//! What agents may run through MCP is set by the agent permissions ([`crate::permissions`]),
//! checked here for every request.

use std::time::Duration;

use serde_json::{json, Map, Value};
use zc_contracts::{OrchestrationThread, ThreadId};
use zenith_client::{Client, RpcError, StreamEvent};
use zenith_model::requests::pending_requests;
use zenith_model::shell::{self, Shell};
use zenith_model::thread::ThreadState;
use zenith_model::time::now_millis;
use zenith_model::timeline::{self, TimelineItem};
use zenith_model::worklog;

use crate::permissions::{Access, Permissions};

/// Who runs a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Caller {
    Window,
    Cli,
    /// An agent, through zenith-mcp.
    Agent,
}

/// What a command does to zenith.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Reads only.
    Read,
    /// Changes threads, projects or settings, or starts agents.
    Write,
    /// Deletes something for good.
    Destructive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ty {
    String,
    Bool,
    Integer,
    Object,
    StringList,
    Enum(&'static [&'static str]),
    /// Any JSON value.
    Any,
}

#[derive(Clone, Copy, Debug)]
pub struct Param {
    pub name: &'static str,
    pub ty: Ty,
    pub required: bool,
    pub help: &'static str,
}

#[derive(Clone, Copy, Debug)]
pub struct Spec {
    pub name: &'static str,
    pub summary: &'static str,
    pub effect: Effect,
    pub params: &'static [Param],
}

const fn req(name: &'static str, ty: Ty, help: &'static str) -> Param {
    Param {
        name,
        ty,
        required: true,
        help,
    }
}

const fn opt(name: &'static str, ty: Ty, help: &'static str) -> Param {
    Param {
        name,
        ty,
        required: false,
        help,
    }
}

const THREAD: Param = req("threadId", Ty::String, "The thread's id (see thread.list).");
const WHERE_THREAD: Param = opt("threadId", Ty::String, "Where this thread works (its worktree, else its project's folder).");
const WHERE_PROJECT: Param = opt("projectId", Ty::String, "Or this project's folder.");
const TERMINAL: Param = opt("terminalId", Ty::String, "Which of the thread's terminals (default \"default\").");
const GIT_ACTIONS: &[&str] = &["commit", "push", "pr", "commit_push", "commit_push_pr"];
const PR_ACTIONS: &[&str] = &[
    "merge",
    "ready",
    "draft",
    "close",
    "reopen",
    "update-branch",
    "enable-auto-merge",
    "disable-auto-merge",
];
const RUNTIME_MODES: &[&str] = &["approval-required", "auto-accept-edits", "auto", "full-access"];
const SECTIONS: &[&str] = &["pinned", "active", "snoozed", "settled", "archived"];
const DECISIONS: &[&str] = &["accept", "acceptForSession", "acceptAlways", "decline", "cancel"];

/// Every command, in the order the docs list them.
pub static COMMANDS: &[Spec] = &[
    Spec {
        name: "app.version",
        summary: "The versions of zenith and of the server it talks to.",
        effect: Effect::Read,
        params: &[],
    },
    Spec {
        name: "app.checkUpdates",
        summary: "Asks GitHub Releases for a newer zenith (signed releases only).",
        effect: Effect::Read,
        params: &[],
    },
    Spec {
        name: "server.status",
        summary: "Whether the server answers, where, whether it is this machine's or a remote one (`zenith-cli remote`), and its environment.",
        effect: Effect::Read,
        params: &[],
    },
    Spec {
        name: "project.list",
        summary: "Every project: id, title, folder.",
        effect: Effect::Read,
        params: &[],
    },
    Spec {
        name: "project.overview",
        summary: "Everything at once: projects, each with its threads grouped by sidebar section, their status and what they wait on.",
        effect: Effect::Read,
        params: &[opt("projectId", Ty::String, "Only this project.")],
    },
    Spec {
        name: "project.add",
        summary: "Adds a folder as a project.",
        effect: Effect::Write,
        params: &[
            req("path", Ty::String, "Absolute path of the folder."),
            opt("title", Ty::String, "Defaults to the folder's name."),
            opt("create", Ty::Bool, "Create the folder if it does not exist."),
        ],
    },
    Spec {
        name: "project.browse",
        summary: "Folders on the server's machine whose path starts with `path` (`~/` lists the home folder): what to give `project.add` when the server is on another machine.",
        effect: Effect::Read,
        params: &[req("path", Ty::String, "A folder ending in `/` lists its folders; otherwise the folders it starts.")],
    },
    Spec {
        name: "project.rename",
        summary: "Renames a project.",
        effect: Effect::Write,
        params: &[req("projectId", Ty::String, "The project's id."), req("title", Ty::String, "The new title.")],
    },
    Spec {
        name: "project.remove",
        summary: "Removes a project from zenith (its folder stays on disk).",
        effect: Effect::Destructive,
        params: &[
            req("projectId", Ty::String, "The project's id."),
            opt("force", Ty::Bool, "Also when it still has threads."),
        ],
    },
    Spec {
        name: "thread.list",
        summary: "Threads in sidebar order with their section and status.",
        effect: Effect::Read,
        params: &[
            opt("projectId", Ty::String, "Only this project's threads."),
            opt("section", Ty::Enum(SECTIONS), "Only this section."),
            opt("query", Ty::String, "Only threads whose title or branch contains this."),
        ],
    },
    Spec {
        name: "thread.get",
        summary: "One thread: status, the last messages, what the agent did, what it waits on, its plan and changed files.",
        effect: Effect::Read,
        params: &[THREAD, opt("messages", Ty::Integer, "How many recent timeline items (default 30).")],
    },
    Spec {
        name: "thread.new",
        summary: "Starts a thread in a project with a first message.",
        effect: Effect::Write,
        params: &[
            req("projectId", Ty::String, "The project's id."),
            req("prompt", Ty::String, "The first message."),
            opt(
                "images",
                Ty::StringList,
                "Images to send with it: paths of PNG, JPEG, GIF or WebP files (10 MB each at most).",
            ),
            opt(
                "provider",
                Ty::String,
                "Provider instance (claudeAgent, codex…); default: the server's default.",
            ),
            opt("model", Ty::String, "Model slug; default: the provider's default."),
            opt(
                "modelOptions",
                Ty::Any,
                "The model's options, [{id, value}] (reasoning effort…; see provider.list).",
            ),
            opt(
                "runtimeMode",
                Ty::Enum(RUNTIME_MODES),
                "How much the agent may do without asking (default: the server's).",
            ),
            opt("plan", Ty::Bool, "Plan first: the agent proposes a plan and waits."),
            opt("worktree", Ty::Bool, "Work on a new branch in its own worktree."),
            opt("threadId", Ty::String, "The new thread's id (default: a new UUID)."),
            opt(
                "baseBranch",
                Ty::String,
                "Branch the worktree starts from (default: the checkout's current branch).",
            ),
            opt("wait", Ty::Bool, "Wait until the agent stops or needs you (like thread.wait)."),
        ],
    },
    Spec {
        name: "thread.send",
        summary: "Sends a message to a thread (the agent starts a turn).",
        effect: Effect::Write,
        params: &[
            THREAD,
            req("prompt", Ty::String, "The message."),
            opt(
                "images",
                Ty::StringList,
                "Images to send with it: paths of PNG, JPEG, GIF or WebP files (10 MB each at most).",
            ),
            opt("provider", Ty::String, "Switch to this provider instance."),
            opt("model", Ty::String, "Switch to this model slug."),
            opt("modelOptions", Ty::Any, "The model's options, [{id, value}]."),
            opt("runtimeMode", Ty::Enum(RUNTIME_MODES), "Switch the approval mode."),
            opt("plan", Ty::Bool, "Plan mode for this turn."),
            opt("implementPlan", Ty::String, "Implement this proposed plan (its id; see thread.get)."),
            opt("wait", Ty::Bool, "Wait until the agent stops or needs you."),
        ],
    },
    Spec {
        name: "thread.wait",
        summary: "Waits until the thread's agent stops working or needs you, then returns the thread.",
        effect: Effect::Read,
        params: &[THREAD, opt("timeoutSeconds", Ty::Integer, "Give up after this long (default 600).")],
    },
    Spec {
        name: "thread.interrupt",
        summary: "Stops the agent's current turn.",
        effect: Effect::Write,
        params: &[THREAD],
    },
    Spec {
        name: "thread.approve",
        summary: "Answers an approval the agent waits on.",
        effect: Effect::Write,
        params: &[
            THREAD,
            opt("requestId", Ty::String, "Default: the oldest pending approval."),
            req("decision", Ty::Enum(DECISIONS), "What to answer."),
        ],
    },
    Spec {
        name: "thread.answer",
        summary: "Answers the agent's questions: {questionId: \"option label or text\"} (lists for multiple choice).",
        effect: Effect::Write,
        params: &[
            THREAD,
            opt("requestId", Ty::String, "Default: the oldest pending question."),
            req("answers", Ty::Object, "Answers by question id."),
        ],
    },
    Spec {
        name: "thread.dismissQuestion",
        summary: "Dismisses a question the agent asked without answering it (when it allows).",
        effect: Effect::Write,
        params: &[THREAD, opt("requestId", Ty::String, "Default: the oldest pending question.")],
    },
    Spec {
        name: "thread.rename",
        summary: "Renames a thread.",
        effect: Effect::Write,
        params: &[THREAD, req("title", Ty::String, "The new title.")],
    },
    Spec {
        name: "thread.regenerateTitle",
        summary: "Asks for a new title written from the conversation.",
        effect: Effect::Write,
        params: &[THREAD],
    },
    Spec {
        name: "thread.pin",
        summary: "Pins a thread to the top of the sidebar.",
        effect: Effect::Write,
        params: &[THREAD],
    },
    Spec {
        name: "thread.unpin",
        summary: "Unpins a thread.",
        effect: Effect::Write,
        params: &[THREAD],
    },
    Spec {
        name: "thread.settle",
        summary: "Settles a thread (done for now).",
        effect: Effect::Write,
        params: &[THREAD],
    },
    Spec {
        name: "thread.reopen",
        summary: "Moves a settled thread back to Active.",
        effect: Effect::Write,
        params: &[THREAD],
    },
    Spec {
        name: "thread.snooze",
        summary: "Hides a thread until a time (it comes back early if the agent needs you).",
        effect: Effect::Write,
        params: &[
            THREAD,
            opt("until", Ty::String, "ISO date; default in one hour."),
            opt("hours", Ty::Integer, "Or: in this many hours."),
        ],
    },
    Spec {
        name: "thread.wake",
        summary: "Ends a snooze.",
        effect: Effect::Write,
        params: &[THREAD],
    },
    Spec {
        name: "thread.archive",
        summary: "Archives a thread.",
        effect: Effect::Write,
        params: &[THREAD],
    },
    Spec {
        name: "thread.unarchive",
        summary: "Brings an archived thread back.",
        effect: Effect::Write,
        params: &[THREAD],
    },
    Spec {
        name: "thread.stop",
        summary: "Stops the thread's agent session.",
        effect: Effect::Write,
        params: &[THREAD],
    },
    Spec {
        name: "thread.revert",
        summary: "Reverts files and conversation to after turn N (0: before the first turn).",
        effect: Effect::Destructive,
        params: &[THREAD, req("turnCount", Ty::Integer, "The number of turns to keep.")],
    },
    Spec {
        name: "thread.delete",
        summary: "Deletes a thread for good.",
        effect: Effect::Destructive,
        params: &[THREAD],
    },
    Spec {
        name: "thread.diff",
        summary: "The unified diff of turns (default: the whole thread so far).",
        effect: Effect::Read,
        params: &[
            THREAD,
            opt("fromTurn", Ty::Integer, "Start after this many turns."),
            opt("toTurn", Ty::Integer, "End after this many turns."),
        ],
    },
    Spec {
        name: "thread.search",
        summary: "Searches every thread's messages.",
        effect: Effect::Read,
        params: &[
            req("query", Ty::String, "What to look for."),
            opt("limit", Ty::Integer, "At most this many matches."),
        ],
    },
    Spec {
        name: "git.status",
        summary: "The git state where a thread works (or a project's folder): branch, changes, ahead/behind, its pull request.",
        effect: Effect::Read,
        params: &[WHERE_THREAD, WHERE_PROJECT],
    },
    Spec {
        name: "git.pull",
        summary: "Pulls the branch where a thread works (or a project's folder).",
        effect: Effect::Write,
        params: &[WHERE_THREAD, WHERE_PROJECT],
    },
    Spec {
        name: "git.commit",
        summary: "Commits, pushes and/or opens a pull request in one step (messages written for you when left out).",
        effect: Effect::Write,
        params: &[
            WHERE_THREAD,
            WHERE_PROJECT,
            opt("action", Ty::Enum(GIT_ACTIONS), "What to do (default commit)."),
            opt("message", Ty::String, "The commit message (default: written from the changes)."),
            opt("files", Ty::StringList, "Only these paths (default: every change)."),
            opt("newBranch", Ty::Bool, "Commit on a new feature branch."),
        ],
    },
    Spec {
        name: "pr.list",
        summary: "Pull requests of zenith's projects (GitHub, GitLab, Azure DevOps, Forgejo, Bitbucket).",
        effect: Effect::Read,
        params: &[
            opt("projectId", Ty::String, "Only this project's."),
            opt("state", Ty::Enum(&["open", "closed", "merged", "all"]), "Default open."),
            opt("query", Ty::String, "Only those matching this."),
            opt("limit", Ty::Integer, "At most this many (default 50)."),
        ],
    },
    Spec {
        name: "pr.action",
        summary: "Acts on a pull request: merge, ready, draft, close, reopen, update its branch, auto-merge.",
        effect: Effect::Write,
        params: &[
            req("projectId", Ty::String, "The project it belongs to."),
            req("repository", Ty::String, "owner/name, as pr.list gives it."),
            req("number", Ty::Integer, "The pull request's number."),
            req("action", Ty::Enum(PR_ACTIONS), "What to do."),
            opt("mergeMethod", Ty::Enum(&["merge", "squash", "rebase"]), "For merge (default: the project's)."),
        ],
    },
    Spec {
        name: "terminal.open",
        summary: "Opens (or reuses) a terminal where a thread works.",
        effect: Effect::Write,
        params: &[
            THREAD,
            TERMINAL,
            opt("cols", Ty::Integer, "Width in columns (default 120)."),
            opt("rows", Ty::Integer, "Height in rows (default 32)."),
        ],
    },
    Spec {
        name: "terminal.run",
        summary: "Runs a shell command in a thread's terminal (opened if needed); read its output with terminal.read.",
        effect: Effect::Write,
        params: &[THREAD, req("command", Ty::String, "The command line."), TERMINAL],
    },
    Spec {
        name: "terminal.write",
        summary: "Types into a thread's terminal (\\r is Enter, \\u0003 is Ctrl-C).",
        effect: Effect::Write,
        params: &[THREAD, req("data", Ty::String, "What to type."), TERMINAL],
    },
    Spec {
        name: "terminal.read",
        summary: "What a thread's terminal shows: its recent output as plain text, and whether it still runs.",
        effect: Effect::Read,
        params: &[THREAD, TERMINAL, opt("lines", Ty::Integer, "The last this many lines (default 200).")],
    },
    Spec {
        name: "terminal.close",
        summary: "Closes a thread's terminal (and what runs in it).",
        effect: Effect::Write,
        params: &[THREAD, TERMINAL],
    },
    Spec {
        name: "project.scripts",
        summary: "A project's scripts (dev server, tests, lint…), as set in its settings.",
        effect: Effect::Read,
        params: &[req("projectId", Ty::String, "The project's id.")],
    },
    Spec {
        name: "project.runScript",
        summary: "Runs one of a project's scripts in a terminal of a thread of that project.",
        effect: Effect::Write,
        params: &[THREAD, req("scriptId", Ty::String, "The script's id (see project.scripts).")],
    },
    Spec {
        name: "project.saveScript",
        summary: "Adds a script (an action of the header's \"Add action\") to a project, or changes one.",
        effect: Effect::Write,
        params: &[
            req("projectId", Ty::String, "The project's id."),
            req("name", Ty::String, "What the button says."),
            req("command", Ty::String, "The shell command it runs."),
            opt(
                "icon",
                Ty::Enum(&["play", "test", "lint", "configure", "build", "debug"]),
                "Its icon (play by default).",
            ),
            opt("scriptId", Ty::String, "Change this script instead of adding one."),
        ],
    },
    Spec {
        name: "project.openInEditor",
        summary: "Opens a thread's folder (its worktree, else its project's) in an editor installed on the server's machine.",
        effect: Effect::Write,
        params: &[THREAD, opt("editor", Ty::String, "cursor, vscode, zed…; default: the first available (see app.editors).")],
    },
    Spec {
        name: "app.editors",
        summary: "The editors installed on the server's machine, that project.openInEditor can open.",
        effect: Effect::Read,
        params: &[],
    },
    Spec {
        name: "provider.list",
        summary: "The agents' providers: status, version, sign-in, models.",
        effect: Effect::Read,
        params: &[],
    },
    Spec {
        name: "provider.refresh",
        summary: "Checks the providers again (sign-in, versions, models).",
        effect: Effect::Write,
        params: &[opt("provider", Ty::String, "Only this provider instance.")],
    },
    Spec {
        name: "settings.get",
        summary: "The server's settings.",
        effect: Effect::Read,
        params: &[],
    },
    Spec {
        name: "settings.update",
        summary: "Changes server settings (a partial settings object).",
        effect: Effect::Write,
        params: &[req("patch", Ty::Object, "Keys of settings.get to change.")],
    },
    Spec {
        name: "settings.agentPermissions",
        summary: "What agents may do through zenith-mcp: off, read or full.",
        effect: Effect::Read,
        params: &[],
    },
    Spec {
        name: "settings.setAgentPermissions",
        summary: "Sets what agents may do through zenith-mcp (not available to agents themselves).",
        effect: Effect::Write,
        params: &[req(
            "mcp",
            Ty::Enum(&["off", "read", "full"]),
            "off: nothing; read: read-only commands; full: everything.",
        )],
    },
    Spec {
        name: "sessions.list",
        summary: "Claude Code and Codex sessions on this Mac, with costs and totals.",
        effect: Effect::Read,
        params: &[opt("limit", Ty::Integer, "At most this many sessions (default 100).")],
    },
    Spec {
        name: "lsuite.apps",
        summary: "The other lsuite apps installed (from ~/.lsuite/apps), and how to drive them.",
        effect: Effect::Read,
        params: &[],
    },
];

pub fn spec(name: &str) -> Option<&'static Spec> {
    COMMANDS.iter().find(|s| s.name == name)
}

#[derive(Debug)]
pub enum CommandError {
    UnknownCommand(String),
    InvalidParams(String),
    NotAllowed(String),
    NotFound(String),
    Server(RpcError),
    Failed(String),
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownCommand(name) => write!(f, "unknown command {name:?} (see `zenith-cli list`)"),
            Self::InvalidParams(why) => write!(f, "invalid parameters: {why}"),
            Self::NotAllowed(why) => write!(f, "not allowed: {why}"),
            Self::NotFound(what) => write!(f, "not found: {what}"),
            Self::Server(error) => write!(f, "{error}"),
            Self::Failed(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for CommandError {}

impl From<RpcError> for CommandError {
    fn from(error: RpcError) -> Self {
        Self::Server(error)
    }
}

/// The JSON Schema of a command's parameters (MCP `inputSchema`).
pub fn input_schema(spec: &Spec) -> Value {
    let mut properties = Map::new();
    for p in spec.params {
        let mut schema = match p.ty {
            Ty::String => json!({"type": "string"}),
            Ty::Bool => json!({"type": "boolean"}),
            Ty::Integer => json!({"type": "integer"}),
            Ty::Object => json!({"type": "object"}),
            Ty::StringList => json!({"type": "array", "items": {"type": "string"}}),
            Ty::Enum(values) => json!({"type": "string", "enum": values}),
            Ty::Any => json!({}),
        };
        schema["description"] = json!(p.help);
        properties.insert(p.name.to_owned(), schema);
    }
    let required: Vec<&str> = spec.params.iter().filter(|p| p.required).map(|p| p.name).collect();
    json!({"type": "object", "properties": properties, "required": required, "additionalProperties": false})
}

/// Checks `params` against the spec: required keys, types, enum values, no unknown keys.
pub fn validate(spec: &Spec, params: &Value) -> Result<(), CommandError> {
    let empty = Map::new();
    let object = match params {
        Value::Null => &empty,
        Value::Object(o) => o,
        _ => return Err(CommandError::InvalidParams("expected a JSON object".into())),
    };
    for key in object.keys() {
        if !spec.params.iter().any(|p| p.name == key) {
            let known: Vec<&str> = spec.params.iter().map(|p| p.name).collect();
            return Err(CommandError::InvalidParams(format!("unknown parameter {key:?} (expected one of {known:?})")));
        }
    }
    for p in spec.params {
        match object.get(p.name) {
            None | Some(Value::Null) if p.required => {
                return Err(CommandError::InvalidParams(format!("{} is required", p.name)));
            }
            None | Some(Value::Null) => {}
            Some(value) => {
                let ok = match p.ty {
                    Ty::String => value.is_string(),
                    Ty::Bool => value.is_boolean(),
                    Ty::Integer => value.is_i64() || value.is_u64(),
                    Ty::Object => value.is_object(),
                    Ty::StringList => value.as_array().is_some_and(|a| a.iter().all(Value::is_string)),
                    Ty::Enum(values) => value.as_str().is_some_and(|v| values.contains(&v)),
                    Ty::Any => true,
                };
                if !ok {
                    let expected = match p.ty {
                        Ty::Enum(values) => format!("one of {values:?}"),
                        other => format!("{other:?}").to_lowercase(),
                    };
                    return Err(CommandError::InvalidParams(format!("{} must be {expected}", p.name)));
                }
            }
        }
    }
    Ok(())
}

/// Runs a command for `caller`.
pub async fn run(client: &Client, caller: Caller, name: &str, params: Value) -> Result<Value, CommandError> {
    let spec = spec(name).ok_or_else(|| CommandError::UnknownCommand(name.to_owned()))?;
    validate(spec, &params)?;
    if caller == Caller::Agent {
        let permissions = Permissions::load();
        let allowed = match permissions.mcp {
            Access::Off => false,
            Access::Read => spec.effect == Effect::Read,
            Access::Full => true,
        };
        if !allowed || spec.name == "settings.setAgentPermissions" {
            return Err(CommandError::NotAllowed(format!(
                "{} is off for agents (zenith › Settings › Agents, now {:?})",
                spec.name, permissions.mcp
            )));
        }
    }
    let p = Params(params);
    match name {
        "app.version" => {
            let server = server_descriptor(client).await.ok();
            Ok(json!({
                "zenith": env!("CARGO_PKG_VERSION"),
                "server": server.as_ref().and_then(|s| s.get("serverVersion")).cloned(),
            }))
        }
        "app.checkUpdates" => {
            let found = crate::update::check().await.map_err(|e| CommandError::Failed(e.to_string()))?;
            Ok(match found {
                Some(release) => json!({"available": true, "version": release.version, "notes": release.notes, "current": crate::update::current_version()}),
                None => json!({"available": false, "current": crate::update::current_version(), "disabled": crate::update::disabled()}),
            })
        }
        "server.status" => {
            let descriptor = server_descriptor(client).await;
            Ok(json!({
                "url": client.base_url(),
                "remote": client.is_remote(),
                "answers": descriptor.is_ok(),
                "connection": format!("{:?}", *client.status().borrow()),
                "environment": descriptor.ok(),
            }))
        }
        "project.list" => {
            let shell = load_shell(client).await?;
            Ok(Value::Array(
                shell
                    .projects_sorted()
                    .into_iter()
                    .map(|p| json!({"projectId": p.id.as_str(), "title": p.title, "path": p.workspace_root}))
                    .collect(),
            ))
        }
        "project.overview" => {
            let shell = load_shell(client).await?;
            let only = p.str("projectId");
            let now = now_millis();
            let projects: Vec<Value> = shell
                .projects_sorted()
                .into_iter()
                .filter(|project| only.is_none_or(|id| project.id.as_str() == id))
                .map(|project| {
                    let sections: Map<String, Value> = shell
                        .sidebar(Some(&project.id), "", now)
                        .into_iter()
                        .map(|(section, threads)| {
                            (
                                section.as_str().to_owned(),
                                Value::Array(threads.iter().map(|t| thread_row(&shell, t, now)).collect()),
                            )
                        })
                        .collect();
                    json!({"projectId": project.id.as_str(), "title": project.title, "path": project.workspace_root, "threads": sections})
                })
                .collect();
            Ok(json!({"projects": projects}))
        }
        "project.add" => {
            let path = p.req_str("path")?;
            if !std::path::Path::new(path).is_absolute() {
                return Err(CommandError::InvalidParams("path must be absolute".into()));
            }
            let title = p.str("title").map(String::from).unwrap_or_else(|| {
                std::path::Path::new(path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Project".into())
            });
            let project_id = zenith_client::new_id();
            let mut command = json!({"type": "project.create", "projectId": project_id, "title": title, "workspaceRoot": path});
            if p.bool("create") == Some(true) {
                command["createWorkspaceRootIfMissing"] = json!(true);
            }
            client.dispatch(command).await?;
            Ok(json!({"projectId": project_id, "title": title, "path": path}))
        }
        "project.browse" => Ok(client.call("filesystem.browse", json!({"partialPath": p.req_str("path")?})).await?),
        "project.rename" => {
            dispatch(
                client,
                json!({"type": "project.meta.update", "projectId": p.req_str("projectId")?, "title": p.req_str("title")?}),
            )
            .await
        }
        "project.remove" => {
            let mut command = json!({"type": "project.delete", "projectId": p.req_str("projectId")?});
            if p.bool("force") == Some(true) {
                command["force"] = json!(true);
            }
            dispatch(client, command).await
        }
        "thread.list" => {
            let now = now_millis();
            let section = p.str("section");
            let threads: Vec<Value> = if section == Some("archived") {
                let archived = client.call("orchestration.getArchivedShellSnapshot", json!({})).await?;
                let mut shell = load_shell(client).await?;
                if let Ok(snapshot) = serde_json::from_value::<zc_contracts::OrchestrationShellSnapshot>(archived) {
                    shell.threads = snapshot.threads;
                }
                shell
                    .threads
                    .iter()
                    .filter(|t| p.str("projectId").is_none_or(|id| t.project_id.as_str() == id))
                    .map(|t| thread_row(&shell, t, now))
                    .collect()
            } else {
                let shell = load_shell(client).await?;
                let project = p.str("projectId").map(zc_contracts::ProjectId::from);
                shell
                    .sidebar(project.as_ref(), p.str("query").unwrap_or(""), now)
                    .into_iter()
                    .filter(|(s, _)| section.is_none_or(|wanted| s.as_str() == wanted))
                    .flat_map(|(_, threads)| threads.into_iter().map(|t| thread_row(&shell, t, now)).collect::<Vec<_>>())
                    .collect()
            };
            Ok(Value::Array(threads))
        }
        "thread.get" => {
            let id = p.req_str("threadId")?;
            let state = load_thread(client, id).await?;
            let shell = load_shell(client).await.ok();
            let limit = p.int("messages").unwrap_or(30).clamp(1, 500) as usize;
            Ok(thread_detail(
                state.thread().ok_or_else(|| CommandError::NotFound(format!("thread {id}")))?,
                shell.as_ref(),
                limit,
            ))
        }
        "thread.new" => {
            let project = p.req_str("projectId")?.to_owned();
            let shell = load_shell(client).await?;
            let project_shell = shell
                .project(&zc_contracts::ProjectId::from(project.as_str()))
                .ok_or_else(|| CommandError::NotFound(format!("project {project}")))?
                .clone();
            let (mut model, runtime_mode) = resolve_model(client, p.str("provider"), p.str("model"), p.str("runtimeMode")).await?;
            if let Some(options) = p.0.get("modelOptions").filter(|o| o.is_array()) {
                model["options"] = options.clone();
            }
            let thread_id = p.str("threadId").map(String::from).unwrap_or_else(zenith_client::new_id);
            let prompt = p.req_str("prompt")?;
            let interaction = if p.bool("plan") == Some(true) { "plan" } else { "default" };
            let title = worklog::truncate(prompt.lines().next().unwrap_or("New thread").trim(), 60);
            let mut command = json!({
                "type": "thread.turn.start",
                "threadId": thread_id,
                "message": {"messageId": zenith_client::new_id(), "role": "user", "text": prompt, "attachments": image_attachments(&p)?},
                "modelSelection": model,
                "titleSeed": title,
                "runtimeMode": runtime_mode,
                "interactionMode": interaction,
                "bootstrap": {"createThread": {
                    "projectId": project, "title": title, "modelSelection": model, "runtimeMode": runtime_mode,
                    "interactionMode": interaction, "branch": null, "worktreePath": null, "createdAt": zenith_client::now_iso(),
                }},
            });
            if p.bool("worktree") == Some(true) {
                let base = match p.str("baseBranch") {
                    Some(branch) => branch.to_owned(),
                    None => current_branch(client, &project_shell.workspace_root).await.unwrap_or_else(|| "main".into()),
                };
                command["bootstrap"]["prepareWorktree"] = json!({
                    "projectCwd": project_shell.workspace_root,
                    "baseBranch": base,
                    "branch": format!("zenith/{}", thread_id.chars().take(8).collect::<String>()),
                });
                command["bootstrap"]["runSetupScript"] = json!(true);
            }
            client.dispatch(command).await?;
            if p.bool("wait") == Some(true) {
                return wait_for(client, &thread_id, Duration::from_secs(600)).await;
            }
            Ok(json!({"threadId": thread_id, "title": title}))
        }
        "thread.send" => {
            let id = p.req_str("threadId")?;
            let state = load_thread(client, id).await?;
            let thread = state.thread().ok_or_else(|| CommandError::NotFound(format!("thread {id}")))?;
            let mut model = serde_json::to_value(&thread.model_selection).unwrap_or(Value::Null);
            if p.str("provider").is_some() || p.str("model").is_some() {
                let provider = p.str("provider").unwrap_or(thread.model_selection.instance_id.as_str());
                let slug = match p.str("model") {
                    Some(slug) => slug.to_owned(),
                    None if provider == thread.model_selection.instance_id.as_str() => thread.model_selection.model.clone(),
                    None => resolve_model(client, Some(provider), None, None).await?.0["model"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                };
                model = json!({"instanceId": provider, "model": slug});
            }
            if let Some(options) = p.0.get("modelOptions").filter(|o| o.is_array()) {
                model["options"] = options.clone();
            }
            let runtime_mode = p
                .str("runtimeMode")
                .map(String::from)
                .unwrap_or_else(|| thread.runtime_mode.as_str().to_owned());
            let implement = p.str("implementPlan");
            let interaction = match (implement, p.bool("plan")) {
                (Some(_), _) | (None, Some(false)) => "default".to_owned(),
                (None, Some(true)) => "plan".to_owned(),
                (None, None) => thread.interaction_mode.as_str().to_owned(),
            };
            let mut command = json!({
                "type": "thread.turn.start",
                "threadId": id,
                "message": {"messageId": zenith_client::new_id(), "role": "user", "text": p.req_str("prompt")?, "attachments": image_attachments(&p)?},
                "modelSelection": model,
                "runtimeMode": runtime_mode,
                "interactionMode": interaction,
            });
            if let Some(plan) = implement {
                if !thread.proposed_plans.iter().any(|candidate| candidate.id == plan) {
                    return Err(CommandError::NotFound(format!("plan {plan} in thread {id}")));
                }
                command["sourceProposedPlan"] = json!({"threadId": id, "planId": plan});
            }
            client.dispatch(command).await?;
            if p.bool("wait") == Some(true) {
                return wait_for(client, id, Duration::from_secs(600)).await;
            }
            Ok(json!({"threadId": id, "sent": true}))
        }
        "thread.wait" => {
            let id = p.req_str("threadId")?;
            let timeout = p.int("timeoutSeconds").unwrap_or(600).clamp(1, 24 * 3600) as u64;
            wait_for(client, id, Duration::from_secs(timeout)).await
        }
        "thread.interrupt" => {
            let id = p.req_str("threadId")?;
            let state = load_thread(client, id).await?;
            let mut command = json!({"type": "thread.turn.interrupt", "threadId": id});
            if let Some(turn) = state.thread().and_then(|t| t.session.as_ref()).and_then(|s| s.active_turn_id.clone()) {
                command["turnId"] = json!(turn.as_str());
            }
            dispatch(client, command).await
        }
        "thread.approve" => {
            let id = p.req_str("threadId")?;
            let request = match p.str("requestId") {
                Some(r) => r.to_owned(),
                None => {
                    let state = load_thread(client, id).await?;
                    let pending = pending_requests(&state.thread().map(|t| t.activities.clone()).unwrap_or_default());
                    pending
                        .approvals
                        .first()
                        .map(|a| a.request_id.clone())
                        .ok_or_else(|| CommandError::NotFound("no pending approval".into()))?
                }
            };
            dispatch(
                client,
                json!({"type": "thread.approval.respond", "threadId": id, "requestId": request, "decision": p.req_str("decision")?}),
            )
            .await
        }
        "thread.answer" => {
            let id = p.req_str("threadId")?;
            let request = match p.str("requestId") {
                Some(r) => r.to_owned(),
                None => {
                    let state = load_thread(client, id).await?;
                    let pending = pending_requests(&state.thread().map(|t| t.activities.clone()).unwrap_or_default());
                    pending
                        .user_inputs
                        .first()
                        .map(|q| q.request_id.clone())
                        .ok_or_else(|| CommandError::NotFound("no pending question".into()))?
                }
            };
            let answers = p.0.get("answers").cloned().unwrap_or(json!({}));
            dispatch(
                client,
                json!({"type": "thread.user-input.respond", "threadId": id, "requestId": request, "answers": answers}),
            )
            .await
        }
        "thread.dismissQuestion" => {
            let id = p.req_str("threadId")?;
            let request = match p.str("requestId") {
                Some(r) => r.to_owned(),
                None => {
                    let state = load_thread(client, id).await?;
                    let pending = pending_requests(&state.thread().map(|t| t.activities.clone()).unwrap_or_default());
                    pending
                        .user_inputs
                        .first()
                        .map(|q| q.request_id.clone())
                        .ok_or_else(|| CommandError::NotFound("no pending question".into()))?
                }
            };
            dispatch(client, json!({"type": "thread.user-input.dismiss", "threadId": id, "requestId": request})).await
        }
        "thread.regenerateTitle" => {
            dispatch(
                client,
                json!({"type": "thread.meta.update", "threadId": p.req_str("threadId")?, "regenerateTitle": true}),
            )
            .await
        }
        "thread.rename" => {
            dispatch(
                client,
                json!({"type": "thread.meta.update", "threadId": p.req_str("threadId")?, "title": p.req_str("title")?}),
            )
            .await
        }
        "thread.pin" => dispatch(client, json!({"type": "thread.pin", "threadId": p.req_str("threadId")?})).await,
        "thread.unpin" => dispatch(client, json!({"type": "thread.unpin", "threadId": p.req_str("threadId")?})).await,
        "thread.settle" => dispatch(client, json!({"type": "thread.settle", "threadId": p.req_str("threadId")?})).await,
        "thread.reopen" => dispatch(client, json!({"type": "thread.unsettle", "threadId": p.req_str("threadId")?, "reason": "user"})).await,
        "thread.snooze" => {
            let until = match (p.str("until"), p.int("hours")) {
                (Some(until), _) => zc_contracts::DateTimeUtc::parse(until).map_err(CommandError::InvalidParams)?.to_iso_string(),
                (None, hours) => zc_contracts::DateTimeUtc::from_millis(now_millis() + hours.unwrap_or(1).max(1) * 3_600_000)
                    .map_err(CommandError::InvalidParams)?
                    .to_iso_string(),
            };
            dispatch(
                client,
                json!({"type": "thread.snooze", "threadId": p.req_str("threadId")?, "snoozedUntil": until}),
            )
            .await
        }
        "thread.wake" => dispatch(client, json!({"type": "thread.unsnooze", "threadId": p.req_str("threadId")?, "reason": "user"})).await,
        "thread.archive" => dispatch(client, json!({"type": "thread.archive", "threadId": p.req_str("threadId")?})).await,
        "thread.unarchive" => dispatch(client, json!({"type": "thread.unarchive", "threadId": p.req_str("threadId")?})).await,
        "thread.stop" => dispatch(client, json!({"type": "thread.session.stop", "threadId": p.req_str("threadId")?})).await,
        "thread.revert" => {
            dispatch(
                client,
                json!({"type": "thread.checkpoint.revert", "threadId": p.req_str("threadId")?, "turnCount": p.int("turnCount").unwrap_or(0).max(0)}),
            )
            .await
        }
        "thread.delete" => dispatch(client, json!({"type": "thread.delete", "threadId": p.req_str("threadId")?})).await,
        "thread.diff" => {
            let id = p.req_str("threadId")?;
            let to = match p.int("toTurn") {
                Some(to) => to,
                None => {
                    let state = load_thread(client, id).await?;
                    state
                        .thread()
                        .map(|t| t.checkpoints.iter().map(|c| c.checkpoint_turn_count).max().unwrap_or(0))
                        .unwrap_or(0)
                }
            };
            let result = match p.int("fromTurn") {
                Some(from) => {
                    client
                        .call("orchestration.getTurnDiff", json!({"threadId": id, "fromTurnCount": from, "toTurnCount": to}))
                        .await?
                }
                None => {
                    client
                        .call("orchestration.getFullThreadDiff", json!({"threadId": id, "toTurnCount": to}))
                        .await?
                }
            };
            Ok(result)
        }
        "thread.search" => {
            let mut payload = json!({"query": p.req_str("query")?});
            if let Some(limit) = p.int("limit") {
                payload["limit"] = json!(limit);
            }
            Ok(client.call("orchestration.searchThreads", payload).await?)
        }
        "git.status" => {
            let cwd = resolve_cwd(client, &p).await?;
            let status = client.call("vcs.refreshStatus", json!({"cwd": cwd})).await?;
            Ok(json!({"cwd": cwd, "status": status}))
        }
        "git.pull" => {
            let cwd = resolve_cwd(client, &p).await?;
            Ok(client.call("vcs.pull", json!({"cwd": cwd})).await?)
        }
        "git.commit" => {
            let cwd = resolve_cwd(client, &p).await?;
            let action = match p.str("action").unwrap_or("commit") {
                "pr" => "create_pr",
                other => other,
            };
            let mut payload = json!({"actionId": zenith_client::new_id(), "cwd": cwd, "action": action});
            if let Some(message) = p.str("message") {
                payload["commitMessage"] = json!(message);
            }
            if let Some(files) = p.0.get("files").filter(|f| f.is_array()) {
                payload["filePaths"] = files.clone();
            }
            if p.bool("newBranch") == Some(true) {
                payload["featureBranch"] = json!(true);
            }
            if let Some(thread) = p.str("threadId") {
                payload["threadId"] = json!(thread);
            }
            let mut stream = client.stream("git.runStackedAction", payload);
            let mut log = Vec::new();
            loop {
                match stream.next().await {
                    Some(StreamEvent::Item(event)) => match event.get("kind").and_then(Value::as_str) {
                        Some("action_finished") => return Ok(json!({"cwd": cwd, "result": event.get("result"), "log": log})),
                        Some("action_failed") => {
                            let message = event.get("message").and_then(Value::as_str).unwrap_or("the git action failed");
                            return Err(CommandError::Failed(message.to_owned()));
                        }
                        _ => {
                            let line = event.get("label").or_else(|| event.get("text")).and_then(Value::as_str).unwrap_or_default();
                            if !line.is_empty() {
                                log.push(line.to_owned());
                            }
                        }
                    },
                    Some(StreamEvent::End(Err(error))) => return Err(error.into()),
                    Some(StreamEvent::End(Ok(()))) | None => return Ok(json!({"cwd": cwd, "log": log})),
                }
            }
        }
        "pr.list" => {
            let mut payload = json!({"state": p.str("state").unwrap_or("open"), "limit": p.int("limit").unwrap_or(50).clamp(1, 200)});
            if let Some(project) = p.str("projectId") {
                payload["projectId"] = json!(project);
            }
            if let Some(query) = p.str("query") {
                payload["query"] = json!(query);
            }
            let result = client.call("pullRequests.list", payload).await?;
            let entries = result.get("entries").and_then(Value::as_array).cloned().unwrap_or_default();
            Ok(json!({
                "pullRequests": entries.iter().map(|e| json!({
                    "projectId": e.get("projectId"), "project": e.get("projectTitle"), "repository": e.get("repository"),
                    "number": e.get("number"), "title": e.get("title"), "url": e.get("url"), "state": e.get("state"),
                    "draft": e.get("isDraft"), "head": e.get("headBranch"), "base": e.get("baseBranch"),
                    "checks": e.get("checksState"), "review": e.get("reviewDecision"), "updatedAt": e.get("updatedAt"),
                })).collect::<Vec<_>>(),
                "errors": result.get("errors"),
            }))
        }
        "pr.action" => {
            let mut payload = json!({
                "projectId": p.req_str("projectId")?, "repository": p.req_str("repository")?,
                "number": p.int("number").unwrap_or(0), "action": p.req_str("action")?,
            });
            if let Some(method) = p.str("mergeMethod") {
                payload["mergeMethod"] = json!(method);
            }
            client.call("pullRequests.runAction", payload).await?;
            Ok(json!({"ok": true}))
        }
        "terminal.open" => {
            let snapshot = open_terminal(client, &p).await?;
            Ok(json!({"terminalId": snapshot.get("terminalId"), "status": snapshot.get("status"), "cwd": snapshot.get("cwd")}))
        }
        "terminal.run" => {
            open_terminal(client, &p).await?;
            let command = p.req_str("command")?;
            client
                .call(
                    "terminal.write",
                    json!({"threadId": p.req_str("threadId")?, "terminalId": terminal_id(&p), "data": format!("{command}\r")}),
                )
                .await?;
            Ok(json!({"terminalId": terminal_id(&p), "sent": command}))
        }
        "terminal.write" => {
            client
                .call(
                    "terminal.write",
                    json!({"threadId": p.req_str("threadId")?, "terminalId": terminal_id(&p), "data": p.req_str("data")?}),
                )
                .await?;
            Ok(json!({"ok": true}))
        }
        "terminal.read" => {
            let item = first_item(
                client,
                "terminal.attach",
                json!({"threadId": p.req_str("threadId")?, "terminalId": terminal_id(&p)}),
            )
            .await?;
            let snapshot = item.get("snapshot").cloned().unwrap_or(Value::Null);
            let history = strip_ansi(snapshot.get("history").and_then(Value::as_str).unwrap_or(""));
            let wanted = p.int("lines").unwrap_or(200).clamp(1, 5000) as usize;
            let lines: Vec<&str> = history.lines().collect();
            let start = lines.len().saturating_sub(wanted);
            Ok(json!({
                "terminalId": terminal_id(&p), "status": snapshot.get("status"), "exitCode": snapshot.get("exitCode"),
                "output": lines[start..].join("\n"),
            }))
        }
        "terminal.close" => {
            client
                .call("terminal.close", json!({"threadId": p.req_str("threadId")?, "terminalId": terminal_id(&p)}))
                .await?;
            Ok(json!({"ok": true}))
        }
        "project.scripts" => {
            let shell = load_shell(client).await?;
            let id = p.req_str("projectId")?;
            let project = shell
                .project(&zc_contracts::ProjectId::from(id))
                .ok_or_else(|| CommandError::NotFound(format!("project {id}")))?;
            Ok(serde_json::to_value(&project.scripts).unwrap_or(Value::Null))
        }
        "project.saveScript" => {
            let shell = load_shell(client).await?;
            let id = p.req_str("projectId")?;
            let project = shell
                .project(&zc_contracts::ProjectId::from(id))
                .ok_or_else(|| CommandError::NotFound(format!("project {id}")))?;
            let (name, command) = (p.req_str("name")?.trim(), p.req_str("command")?.trim());
            if name.is_empty() || command.is_empty() {
                return Err(CommandError::InvalidParams("name and command must not be empty".into()));
            }
            let mut scripts = serde_json::to_value(&project.scripts).unwrap_or_else(|_| json!([]));
            let list = scripts.as_array_mut().ok_or_else(|| CommandError::Failed("unexpected scripts".into()))?;
            let script_id = match p.str("scriptId") {
                Some(existing) => {
                    let script = list
                        .iter_mut()
                        .find(|s| s["id"] == existing)
                        .ok_or_else(|| CommandError::NotFound(format!("script {existing}")))?;
                    script["name"] = json!(name);
                    script["command"] = json!(command);
                    if let Some(icon) = p.str("icon") {
                        script["icon"] = json!(icon);
                    }
                    existing.to_owned()
                }
                None => {
                    let taken: Vec<String> = list.iter().filter_map(|s| s["id"].as_str().map(String::from)).collect();
                    let new_id = next_script_id(name, &taken);
                    list.push(json!({"id": new_id, "name": name, "command": command, "icon": p.str("icon").unwrap_or("play"), "runOnWorktreeCreate": false}));
                    new_id
                }
            };
            dispatch(client, json!({"type": "project.meta.update", "projectId": id, "scripts": scripts})).await?;
            Ok(json!({"scriptId": script_id}))
        }
        "app.editors" => {
            let config = load_config(client).await?;
            Ok(config.get("availableEditors").cloned().unwrap_or_else(|| json!([])))
        }
        "project.openInEditor" => {
            let shell = load_shell(client).await?;
            let thread_id = p.req_str("threadId")?;
            let thread = shell
                .thread(&ThreadId::from(thread_id))
                .ok_or_else(|| CommandError::NotFound(format!("thread {thread_id}")))?;
            let cwd = thread
                .worktree_path
                .clone()
                .or_else(|| shell.project(&thread.project_id).map(|p| p.workspace_root.clone()))
                .ok_or_else(|| CommandError::NotFound("the thread's folder".into()))?;
            let editor = match p.str("editor") {
                Some(editor) => editor.to_owned(),
                None => {
                    let config = load_config(client).await?;
                    config
                        .pointer("/availableEditors/0")
                        .and_then(Value::as_str)
                        .map(String::from)
                        .ok_or_else(|| CommandError::Failed("no editor is installed on the server's machine".into()))?
                }
            };
            client.call("shell.openInEditor", json!({"cwd": cwd, "editor": editor})).await?;
            Ok(json!({"ok": true, "cwd": cwd, "editor": editor}))
        }
        "project.runScript" => {
            let shell = load_shell(client).await?;
            let thread_id = p.req_str("threadId")?;
            let thread = shell
                .thread(&ThreadId::from(thread_id))
                .ok_or_else(|| CommandError::NotFound(format!("thread {thread_id}")))?;
            let project = shell
                .project(&thread.project_id)
                .ok_or_else(|| CommandError::NotFound("the thread's project".into()))?;
            let script_id = p.req_str("scriptId")?;
            let script = project
                .scripts
                .iter()
                .find(|s| s.id == script_id)
                .ok_or_else(|| CommandError::NotFound(format!("script {script_id} (see project.scripts)")))?;
            let terminal = format!("script-{}", script.id);
            let params = Params(json!({"threadId": thread_id, "terminalId": terminal}));
            open_terminal(client, &params).await?;
            client
                .call(
                    "terminal.write",
                    json!({"threadId": thread_id, "terminalId": terminal, "data": format!("{}\r", script.command)}),
                )
                .await?;
            Ok(json!({"terminalId": terminal, "script": script.name, "command": script.command}))
        }
        "provider.list" => {
            let config = load_config(client).await?;
            let providers = config.get("providers").and_then(Value::as_array).cloned().unwrap_or_default();
            Ok(Value::Array(
                providers
                    .iter()
                    .map(|p| {
                        json!({
                            "provider": p.get("instanceId"),
                            "driver": p.get("driver"),
                            "name": p.get("displayName"),
                            "enabled": p.get("enabled"),
                            "installed": p.get("installed"),
                            "version": p.get("version"),
                            "status": p.get("status"),
                            "message": p.get("message"),
                            "auth": p.get("auth"),
                            "models": p.get("models").and_then(Value::as_array).map(|models| models.iter().map(|m| json!({"model": m.get("slug"), "name": m.get("name"), "default": m.get("isDefault")})).collect::<Vec<_>>()),
                        })
                    })
                    .collect(),
            ))
        }
        "provider.refresh" => {
            let mut payload = json!({});
            if let Some(provider) = p.str("provider") {
                payload["instanceId"] = json!(provider);
            }
            Ok(client.call("server.refreshProviders", payload).await?)
        }
        "settings.get" => Ok(client.call("server.getSettings", json!({})).await?),
        "settings.update" => Ok(client
            .call("server.updateSettings", json!({"patch": p.0.get("patch").cloned().unwrap_or(json!({}))}))
            .await?),
        "settings.agentPermissions" => Ok(serde_json::to_value(Permissions::load()).unwrap_or(Value::Null)),
        "settings.setAgentPermissions" => {
            let access: Access = serde_json::from_value(json!(p.req_str("mcp")?)).map_err(|e| CommandError::InvalidParams(e.to_string()))?;
            let mut permissions = Permissions::load();
            permissions.mcp = access;
            permissions.save().map_err(|e| CommandError::Failed(e.to_string()))?;
            Ok(serde_json::to_value(permissions).unwrap_or(Value::Null))
        }
        "sessions.list" => {
            let limit = p.int("limit").unwrap_or(100).clamp(1, 1000);
            let path = format!("/api/zenith/sessions?limit={limit}");
            client.get_json(&path).await.map_err(|e| CommandError::Failed(format!("{e:#}")))
        }
        "lsuite.apps" => Ok(serde_json::to_value(crate::lsuite::installed_apps()).unwrap_or(Value::Null)),
        other => Err(CommandError::UnknownCommand(other.to_owned())),
    }
}

struct Params(Value);

impl Params {
    fn str(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(Value::as_str)
    }

    fn req_str(&self, key: &str) -> Result<&str, CommandError> {
        self.str(key).ok_or_else(|| CommandError::InvalidParams(format!("{key} is required")))
    }

    fn bool(&self, key: &str) -> Option<bool> {
        self.0.get(key).and_then(Value::as_bool)
    }

    fn int(&self, key: &str) -> Option<i64> {
        self.0.get(key).and_then(Value::as_i64)
    }
}

/// The largest image a turn takes (`PROVIDER_SEND_TURN_MAX_IMAGE_BYTES` on the server).
const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

/// `images` (file paths) as the turn's image attachments (data URLs).
fn image_attachments(p: &Params) -> Result<Vec<Value>, CommandError> {
    use base64::Engine;
    let Some(paths) = p.0.get("images").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    paths
        .iter()
        .filter_map(Value::as_str)
        .map(|path| {
            let path = std::path::Path::new(path);
            let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
            let mime = match extension.as_str() {
                "png" => "image/png",
                "jpg" | "jpeg" => "image/jpeg",
                "gif" => "image/gif",
                "webp" => "image/webp",
                _ => return Err(CommandError::InvalidParams(format!("{}: not a PNG, JPEG, GIF or WebP image", path.display()))),
            };
            let bytes = std::fs::read(path).map_err(|e| CommandError::InvalidParams(format!("{}: {e}", path.display())))?;
            if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
                return Err(CommandError::InvalidParams(format!("{}: images must be under 10 MB", path.display())));
            }
            Ok(json!({
                "type": "image",
                "name": path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "image".into()),
                "mimeType": mime,
                "sizeBytes": bytes.len(),
                "dataUrl": format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(&bytes)),
            }))
        })
        .collect()
}

/// Where a thread works (its worktree, else its project's folder), or a project's folder.
async fn resolve_cwd(client: &Client, p: &Params) -> Result<String, CommandError> {
    let shell = load_shell(client).await?;
    if let Some(id) = p.str("threadId") {
        let thread = shell
            .thread(&ThreadId::from(id))
            .ok_or_else(|| CommandError::NotFound(format!("thread {id}")))?;
        if let Some(path) = &thread.worktree_path {
            return Ok(path.clone());
        }
        return shell
            .project(&thread.project_id)
            .map(|project| project.workspace_root.clone())
            .ok_or_else(|| CommandError::NotFound("the thread's project".into()));
    }
    if let Some(id) = p.str("projectId") {
        return shell
            .project(&zc_contracts::ProjectId::from(id))
            .map(|project| project.workspace_root.clone())
            .ok_or_else(|| CommandError::NotFound(format!("project {id}")));
    }
    Err(CommandError::InvalidParams("give threadId or projectId".into()))
}

fn terminal_id(p: &Params) -> String {
    p.str("terminalId").unwrap_or("default").to_owned()
}

/// Opens (or reuses) a thread's terminal where the thread works.
async fn open_terminal(client: &Client, p: &Params) -> Result<Value, CommandError> {
    let thread_id = p.req_str("threadId")?;
    let shell = load_shell(client).await?;
    let thread = shell
        .thread(&ThreadId::from(thread_id))
        .ok_or_else(|| CommandError::NotFound(format!("thread {thread_id}")))?;
    let root = shell
        .project(&thread.project_id)
        .map(|project| project.workspace_root.clone())
        .ok_or_else(|| CommandError::NotFound("the thread's project".into()))?;
    let mut payload = json!({
        "threadId": thread_id,
        "terminalId": terminal_id(p),
        "cwd": thread.worktree_path.clone().unwrap_or(root),
        "cols": p.int("cols").unwrap_or(120),
        "rows": p.int("rows").unwrap_or(32),
    });
    if let Some(worktree) = &thread.worktree_path {
        payload["worktreePath"] = json!(worktree);
    }
    Ok(client.call("terminal.open", payload).await?)
}

/// Terminal output as plain text: escape sequences out, carriage returns resolved.
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.peek() {
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' || (c == '\u{1b}' && chars.peek() == Some(&'\\')) {
                            break;
                        }
                    }
                }
                _ => {
                    chars.next();
                }
            },
            '\r' if chars.peek() == Some(&'\n') => {}
            '\r' => {
                // A carriage return rewrites the line.
                if let Some(start) = out.rfind('\n') {
                    out.truncate(start + 1);
                } else {
                    out.clear();
                }
            }
            c if c.is_control() && c != '\n' && c != '\t' => {}
            c => out.push(c),
        }
    }
    out
}

async fn dispatch(client: &Client, command: Value) -> Result<Value, CommandError> {
    let sequence = client.dispatch(command).await?;
    Ok(json!({"ok": true, "sequence": sequence}))
}

/// A script's id from its name, as the web makes it (`nextProjectScriptId`): lowercase letters,
/// digits and hyphens, 24 at most, with "-2", "-3"… when taken.
fn next_script_id(name: &str, taken: &[String]) -> String {
    const MAX: usize = 24;
    let mut cleaned = String::new();
    for c in name.trim().to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            cleaned.push(c);
        } else if !cleaned.ends_with('-') {
            cleaned.push('-');
        }
    }
    let mut base = cleaned.trim_matches('-').to_owned();
    if base.len() > MAX {
        base = base[..MAX].trim_end_matches('-').to_owned();
    }
    if base.is_empty() {
        base = "script".into();
    }
    if !taken.contains(&base) {
        return base;
    }
    for suffix in 2..10_000 {
        let candidate = format!("{base}-{suffix}");
        let candidate = if candidate.len() <= MAX {
            candidate
        } else {
            let keep = MAX.saturating_sub(suffix.to_string().len() + 1).max(1);
            format!("{}-{suffix}", &base[..keep.min(base.len())])
        };
        if !taken.contains(&candidate) {
            return candidate;
        }
    }
    base
}

async fn server_descriptor(client: &Client) -> anyhow::Result<Value> {
    client.get_json("/.well-known/t3/environment").await
}

/// The first item of a stream (its snapshot); the stream is interrupted afterwards.
async fn first_item(client: &Client, tag: &str, payload: Value) -> Result<Value, CommandError> {
    let mut stream = client.stream(tag, payload);
    match stream.next().await {
        Some(StreamEvent::Item(item)) => Ok(item),
        Some(StreamEvent::End(Err(error))) => Err(error.into()),
        Some(StreamEvent::End(Ok(()))) | None => Err(CommandError::Failed(format!("{tag} ended without data"))),
    }
}

pub async fn load_shell(client: &Client) -> Result<Shell, CommandError> {
    let item = first_item(client, "orchestration.subscribeShell", json!({})).await?;
    let mut shell = Shell::default();
    shell.apply(item).map_err(CommandError::Failed)?;
    Ok(shell)
}

async fn load_config(client: &Client) -> Result<Value, CommandError> {
    let item = first_item(client, "subscribeServerConfig", json!({})).await?;
    Ok(item.get("config").cloned().unwrap_or(Value::Null))
}

pub async fn load_thread(client: &Client, id: &str) -> Result<ThreadState, CommandError> {
    let item = first_item(client, "orchestration.subscribeThread", json!({"threadId": id, "turnLimit": 50})).await?;
    let mut state = ThreadState::new(ThreadId::from(id));
    state.apply(item).map_err(CommandError::Failed)?;
    if state.thread().is_none() {
        return Err(CommandError::NotFound(format!("thread {id}")));
    }
    Ok(state)
}

async fn current_branch(client: &Client, cwd: &str) -> Option<String> {
    let result = client.call("vcs.listRefs", json!({"cwd": cwd})).await.ok()?;
    let refs = result.get("refs")?.as_array()?;
    refs.iter()
        .find(|r| r.get("current").and_then(Value::as_bool) == Some(true))
        .or_else(|| refs.iter().find(|r| r.get("isDefault").and_then(Value::as_bool) == Some(true)))
        .and_then(|r| r.get("name")?.as_str().map(String::from))
}

/// The model selection and approval mode for a new thread.
async fn resolve_model(client: &Client, provider: Option<&str>, model: Option<&str>, runtime_mode: Option<&str>) -> Result<(Value, String), CommandError> {
    let config = load_config(client).await?;
    let settings = config.get("settings").cloned().unwrap_or(Value::Null);
    let runtime_mode = runtime_mode
        .map(String::from)
        .or_else(|| settings.get("defaultRuntimeMode").and_then(Value::as_str).map(String::from))
        .unwrap_or_else(|| "full-access".into());
    let providers = config.get("providers").and_then(Value::as_array).cloned().unwrap_or_default();
    let usable = |p: &&Value| {
        p.get("enabled").and_then(Value::as_bool) == Some(true)
            && p.get("models").and_then(Value::as_array).is_some_and(|m| !m.is_empty())
            && p.get("status").and_then(Value::as_str) != Some("error")
    };
    if provider.is_none() && model.is_none() {
        if let Some(default) = settings.get("defaultModelSelection").filter(|v| !v.is_null()) {
            return Ok((default.clone(), runtime_mode));
        }
    }
    let chosen = match provider {
        Some(name) => providers
            .iter()
            .find(|p| p.get("instanceId").and_then(Value::as_str) == Some(name))
            .ok_or_else(|| CommandError::NotFound(format!("provider {name} (see provider.list)")))?,
        None => match model {
            Some(slug) => providers
                .iter()
                .filter(usable)
                .find(|p| {
                    p.get("models")
                        .and_then(Value::as_array)
                        .is_some_and(|ms| ms.iter().any(|m| m.get("slug").and_then(Value::as_str) == Some(slug)))
                })
                .ok_or_else(|| CommandError::NotFound(format!("model {slug} (see provider.list)")))?,
            None => providers
                .iter()
                .find(usable)
                .ok_or_else(|| CommandError::Failed("no provider is ready (see provider.list)".into()))?,
        },
    };
    let models = chosen.get("models").and_then(Value::as_array).cloned().unwrap_or_default();
    let slug = match model {
        Some(slug) => slug.to_owned(),
        None => models
            .iter()
            .find(|m| m.get("isDefault").and_then(Value::as_bool) == Some(true))
            .or_else(|| models.first())
            .and_then(|m| m.get("slug")?.as_str().map(String::from))
            .ok_or_else(|| CommandError::Failed("the provider has no model".into()))?,
    };
    Ok((json!({"instanceId": chosen.get("instanceId"), "model": slug}), runtime_mode))
}

fn thread_row(shell: &Shell, t: &zc_contracts::OrchestrationThreadShell, now: i64) -> Value {
    json!({
        "threadId": t.id.as_str(),
        "title": t.title,
        "projectId": t.project_id.as_str(),
        "project": shell.project(&t.project_id).map(|p| p.title.clone()),
        "section": if t.archived_at.is_some() { "archived" } else { shell::section(t, now).as_str() },
        "status": shell::status(t).as_str(),
        "branch": t.branch,
        "worktree": t.worktree_path,
        "model": t.model_selection.model,
        "provider": t.model_selection.instance_id.as_str(),
        "pendingApproval": t.has_pending_approvals,
        "pendingQuestion": t.has_pending_user_input,
        "createdAt": t.created_at,
        "lastActivity": zc_contracts::DateTimeUtc::from_millis(shell::activity_at(t)).map(|d| d.to_iso_string()).ok(),
    })
}

fn thread_detail(thread: &OrchestrationThread, shell: Option<&Shell>, limit: usize) -> Value {
    let pending = pending_requests(&thread.activities);
    let items = timeline::timeline(thread);
    let start = items.len().saturating_sub(limit);
    let timeline: Vec<Value> = items[start..]
        .iter()
        .map(|item| match item {
            TimelineItem::Message(m) => json!({"kind": "message", "role": m.role.as_str(), "text": m.text, "streaming": m.streaming, "at": m.created_at}),
            TimelineItem::Plan(p) => json!({"kind": "plan", "planId": p.id, "markdown": p.plan_markdown, "implemented": p.implemented_at.is_some()}),
            TimelineItem::Work(g) => json!({
                "kind": "work",
                "summary": g.summary,
                "running": g.running,
                "failed": g.failed,
                "entries": g.entries.iter().map(|e| json!({"label": e.label, "command": e.command, "files": e.changed_files, "failed": e.failed(), "detail": e.detail.as_deref().map(|d| worklog::truncate(d, 400))})).collect::<Vec<_>>(),
            }),
            TimelineItem::Diff(d) => json!({
                "kind": "diff",
                "turnCount": d.checkpoint_turn_count,
                "files": d.files.iter().map(|f| json!({"path": f.path, "additions": f.additions, "deletions": f.deletions})).collect::<Vec<_>>(),
            }),
        })
        .collect();
    let status = shell.and_then(|s| s.thread(&thread.id)).map(|t| shell::status(t).as_str());
    let plan = worklog::active_plan(&thread.activities, thread.latest_turn.as_ref().map(|t| t.turn_id.as_str()));
    json!({
        "threadId": thread.id.as_str(),
        "title": thread.title,
        "projectId": thread.project_id.as_str(),
        "status": status,
        "session": thread.session.as_ref().map(|s| json!({"status": s.status.as_str(), "error": s.last_error})),
        "branch": thread.branch,
        "worktree": thread.worktree_path,
        "model": serde_json::to_value(&thread.model_selection).ok(),
        "runtimeMode": thread.runtime_mode.as_str(),
        "interactionMode": thread.interaction_mode.as_str(),
        "pendingApprovals": pending.approvals.iter().map(|a| json!({"requestId": a.request_id, "kind": a.request_kind, "detail": a.detail, "choices": a.choices().iter().map(|c| c.decision.clone()).collect::<Vec<_>>()})).collect::<Vec<_>>(),
        "pendingQuestions": pending.user_inputs.iter().map(|q| json!({"requestId": q.request_id, "questions": q.questions.iter().map(|x| json!({"id": x.id, "question": x.question, "options": x.options.iter().map(|o| o.label.clone()).collect::<Vec<_>>(), "multiSelect": x.multi_select})).collect::<Vec<_>>()})).collect::<Vec<_>>(),
        "plan": plan.map(|p| json!({"steps": p.steps.iter().map(|s| json!({"step": s.step, "status": s.status})).collect::<Vec<_>>()})),
        "timeline": timeline,
    })
}

/// Follows the thread until its agent stops or waits on the person.
async fn wait_for(client: &Client, id: &str, timeout: Duration) -> Result<Value, CommandError> {
    let mut stream = client.stream(
        "orchestration.subscribeThread",
        json!({"threadId": id, "turnLimit": 20, "requestCompletionMarker": true}),
    );
    let mut state = ThreadState::new(ThreadId::from(id));
    let started = std::time::Instant::now();
    // A turn just requested may take a moment to show as running.
    let grace = Duration::from_secs(5);
    loop {
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break;
        }
        let next = tokio::time::timeout(remaining.min(Duration::from_secs(2)), stream.next()).await;
        match next {
            Ok(Some(StreamEvent::Item(item))) => {
                state.apply(item).map_err(CommandError::Failed)?;
            }
            Ok(Some(StreamEvent::End(Err(error)))) => return Err(error.into()),
            Ok(Some(StreamEvent::End(Ok(())))) | Ok(None) => break,
            Err(_) => {}
        }
        if !state.live {
            continue;
        }
        let Some(thread) = state.thread() else { continue };
        let pending = pending_requests(&thread.activities);
        let busy = state.is_running() || thread.latest_turn.as_ref().is_some_and(|t| t.state.as_str() == "running");
        if !pending.is_empty() || (!busy && started.elapsed() > grace) {
            break;
        }
    }
    let thread = state.thread().ok_or_else(|| CommandError::NotFound(format!("thread {id}")))?;
    Ok(thread_detail(thread, None, 12))
}

/// `docs/COMMANDS.md`, generated from [`COMMANDS`].
pub fn markdown() -> String {
    let mut out = String::from(
        "# zenith commands\n\nGenerated from the registry (`crates/zenith-commands/src/registry.rs`); do not edit by hand \
         (`zenith-cli docs > docs/COMMANDS.md`). Every command takes a JSON object and returns JSON, the same through the \
         window, `zenith-cli <command> [--param value…]` and `zenith-mcp --live` (tool names use `_` for `.`).\n\n\
         Effects: **read** changes nothing; **write** changes threads, projects or settings or starts agents; \
         **destructive** deletes for good. Agents get what Settings › Agents allows (off, read, full).\n\n",
    );
    let mut family = "";
    for spec in COMMANDS {
        let this_family = spec.name.split('.').next().unwrap_or("");
        if this_family != family {
            family = this_family;
            out.push_str(&format!("## {family}\n\n"));
        }
        let effect = match spec.effect {
            Effect::Read => "read",
            Effect::Write => "write",
            Effect::Destructive => "destructive",
        };
        out.push_str(&format!("### `{}` ({effect})\n\n{}\n\n", spec.name, spec.summary));
        if !spec.params.is_empty() {
            out.push_str("| Parameter | Type | Required | |\n|---|---|---|---|\n");
            for p in spec.params {
                let ty = match p.ty {
                    Ty::Enum(values) => values.iter().map(|v| format!("`{v}`")).collect::<Vec<_>>().join(" \\| "),
                    other => format!("{other:?}").to_lowercase(),
                };
                out.push_str(&format!("| `{}` | {ty} | {} | {} |\n", p.name, if p.required { "yes" } else { "" }, p.help));
            }
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {

    #[test]
    fn script_ids_follow_the_web() {
        assert_eq!(super::next_script_id("Dev server", &[]), "dev-server");
        assert_eq!(super::next_script_id("Dev server", &["dev-server".into()]), "dev-server-2");
        assert_eq!(super::next_script_id("  !!  ", &[]), "script");
        assert_eq!(super::next_script_id("A very long script name that goes on", &[]), "a-very-long-script-name");
    }

    use super::*;

    #[test]
    fn names_are_family_verb_and_unique() {
        let mut seen = std::collections::HashSet::new();
        for spec in COMMANDS {
            let parts: Vec<&str> = spec.name.split('.').collect();
            assert_eq!(parts.len(), 2, "{}", spec.name);
            assert!(
                parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric())),
                "{}",
                spec.name
            );
            assert!(seen.insert(spec.name), "duplicate {}", spec.name);
        }
    }

    #[test]
    fn every_command_has_a_handler() {
        let source = include_str!("registry.rs");
        for spec in COMMANDS {
            assert!(source.contains(&format!("\"{}\" =>", spec.name)), "{} has no arm in run()", spec.name);
        }
    }

    #[test]
    fn images_become_data_urls() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("shot.png");
        std::fs::write(&png, [0x89, b'P', b'N', b'G']).unwrap();
        let params = Params(json!({"images": [png.to_string_lossy()]}));
        let attachments = image_attachments(&params).unwrap();
        assert_eq!(attachments[0]["mimeType"], "image/png");
        assert_eq!(attachments[0]["sizeBytes"], 4);
        assert!(attachments[0]["dataUrl"].as_str().unwrap().starts_with("data:image/png;base64,"));
        let text = dir.path().join("notes.txt");
        std::fs::write(&text, "x").unwrap();
        assert!(image_attachments(&Params(json!({"images": [text.to_string_lossy()]}))).is_err());
    }

    #[test]
    fn terminal_text() {
        assert_eq!(strip_ansi("\u{1b}[1;32mok\u{1b}[0m\r\nnext"), "ok\nnext");
        assert_eq!(strip_ansi("50%\r100%\n"), "100%\n");
        assert_eq!(strip_ansi("\u{1b}]0;title\u{7}$ ls"), "$ ls");
    }

    #[test]
    fn validation() {
        let send = spec("thread.send").unwrap();
        assert!(validate(send, &json!({"threadId": "t", "prompt": "hi"})).is_ok());
        assert!(validate(send, &json!({"threadId": "t"})).is_err());
        assert!(validate(send, &json!({"threadId": "t", "prompt": "hi", "nope": 1})).is_err());
        assert!(validate(send, &json!({"threadId": "t", "prompt": "hi", "runtimeMode": "yolo"})).is_err());
        assert!(validate(spec("project.list").unwrap(), &Value::Null).is_ok());
        let schema = input_schema(send);
        assert_eq!(schema["required"], json!(["threadId", "prompt"]));
    }

    #[test]
    fn docs_are_up_to_date() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/COMMANDS.md");
        let current = std::fs::read_to_string(&path).unwrap_or_default();
        if current != markdown() {
            if std::env::var_os("ZENITH_WRITE_DOCS").is_some() {
                std::fs::write(&path, markdown()).unwrap();
            } else {
                panic!("docs/COMMANDS.md is stale: run `ZENITH_WRITE_DOCS=1 cargo test -p zenith-commands docs_are_up_to_date`");
            }
        }
    }
}
