# zenith commands

Generated from the registry (`crates/zenith-commands/src/registry.rs`); do not edit by hand (`zenith-cli docs > docs/COMMANDS.md`). Every command takes a JSON object and returns JSON, the same through the window, `zenith-cli <command> [--param value…]` and `zenith-mcp --live` (tool names use `_` for `.`).

Effects: **read** changes nothing; **write** changes threads, projects or settings or starts agents; **destructive** deletes for good. Agents get what Settings › Agents allows (off, read, full).

## app

### `app.version` (read)

The versions of zenith and of the server it talks to.

### `app.checkUpdates` (read)

Asks GitHub Releases for a newer zenith (signed releases only).

## server

### `server.status` (read)

Whether the local server answers, where, and its environment.

## project

### `project.list` (read)

Every project: id, title, folder.

### `project.overview` (read)

Everything at once: projects, each with its threads grouped by sidebar section, their status and what they wait on.

| Parameter | Type | Required | |
|---|---|---|---|
| `projectId` | string |  | Only this project. |

### `project.add` (write)

Adds a folder as a project.

| Parameter | Type | Required | |
|---|---|---|---|
| `path` | string | yes | Absolute path of the folder. |
| `title` | string |  | Defaults to the folder's name. |
| `create` | bool |  | Create the folder if it does not exist. |

### `project.rename` (write)

Renames a project.

| Parameter | Type | Required | |
|---|---|---|---|
| `projectId` | string | yes | The project's id. |
| `title` | string | yes | The new title. |

### `project.remove` (destructive)

Removes a project from zenith (its folder stays on disk).

| Parameter | Type | Required | |
|---|---|---|---|
| `projectId` | string | yes | The project's id. |
| `force` | bool |  | Also when it still has threads. |

## thread

### `thread.list` (read)

Threads in sidebar order with their section and status.

| Parameter | Type | Required | |
|---|---|---|---|
| `projectId` | string |  | Only this project's threads. |
| `section` | `pinned` \| `active` \| `snoozed` \| `settled` \| `archived` |  | Only this section. |
| `query` | string |  | Only threads whose title or branch contains this. |

### `thread.get` (read)

One thread: status, the last messages, what the agent did, what it waits on, its plan and changed files.

| Parameter | Type | Required | |
|---|---|---|---|
| `threadId` | string | yes | The thread's id (see thread.list). |
| `messages` | integer |  | How many recent timeline items (default 30). |

### `thread.new` (write)

Starts a thread in a project with a first message.

| Parameter | Type | Required | |
|---|---|---|---|
| `projectId` | string | yes | The project's id. |
| `prompt` | string | yes | The first message. |
| `images` | stringlist |  | Images to send with it: paths of PNG, JPEG, GIF or WebP files (10 MB each at most). |
| `provider` | string |  | Provider instance (claudeAgent, codex…); default: the server's default. |
| `model` | string |  | Model slug; default: the provider's default. |
| `modelOptions` | any |  | The model's options, [{id, value}] (reasoning effort…; see provider.list). |
| `runtimeMode` | `approval-required` \| `auto-accept-edits` \| `auto` \| `full-access` |  | How much the agent may do without asking (default: the server's). |
| `plan` | bool |  | Plan first: the agent proposes a plan and waits. |
| `worktree` | bool |  | Work on a new branch in its own worktree. |
| `threadId` | string |  | The new thread's id (default: a new UUID). |
| `baseBranch` | string |  | Branch the worktree starts from (default: the checkout's current branch). |
| `wait` | bool |  | Wait until the agent stops or needs you (like thread.wait). |

### `thread.send` (write)

Sends a message to a thread (the agent starts a turn).

| Parameter | Type | Required | |
|---|---|---|---|
| `threadId` | string | yes | The thread's id (see thread.list). |
| `prompt` | string | yes | The message. |
| `images` | stringlist |  | Images to send with it: paths of PNG, JPEG, GIF or WebP files (10 MB each at most). |
| `provider` | string |  | Switch to this provider instance. |
| `model` | string |  | Switch to this model slug. |
| `modelOptions` | any |  | The model's options, [{id, value}]. |
| `runtimeMode` | `approval-required` \| `auto-accept-edits` \| `auto` \| `full-access` |  | Switch the approval mode. |
| `plan` | bool |  | Plan mode for this turn. |
| `implementPlan` | string |  | Implement this proposed plan (its id; see thread.get). |
| `wait` | bool |  | Wait until the agent stops or needs you. |

### `thread.wait` (read)

Waits until the thread's agent stops working or needs you, then returns the thread.

| Parameter | Type | Required | |
|---|---|---|---|
| `threadId` | string | yes | The thread's id (see thread.list). |
| `timeoutSeconds` | integer |  | Give up after this long (default 600). |

### `thread.interrupt` (write)

Stops the agent's current turn.

| Parameter | Type | Required | |
|---|---|---|---|
| `threadId` | string | yes | The thread's id (see thread.list). |

### `thread.approve` (write)

Answers an approval the agent waits on.

| Parameter | Type | Required | |
|---|---|---|---|
| `threadId` | string | yes | The thread's id (see thread.list). |
| `requestId` | string |  | Default: the oldest pending approval. |
| `decision` | `accept` \| `acceptForSession` \| `acceptAlways` \| `decline` \| `cancel` | yes | What to answer. |

### `thread.answer` (write)

Answers the agent's questions: {questionId: "option label or text"} (lists for multiple choice).

| Parameter | Type | Required | |
|---|---|---|---|
| `threadId` | string | yes | The thread's id (see thread.list). |
| `requestId` | string |  | Default: the oldest pending question. |
| `answers` | object | yes | Answers by question id. |

### `thread.dismissQuestion` (write)

Dismisses a question the agent asked without answering it (when it allows).

| Parameter | Type | Required | |
|---|---|---|---|
| `threadId` | string | yes | The thread's id (see thread.list). |
| `requestId` | string |  | Default: the oldest pending question. |

### `thread.rename` (write)

Renames a thread.

| Parameter | Type | Required | |
|---|---|---|---|
| `threadId` | string | yes | The thread's id (see thread.list). |
| `title` | string | yes | The new title. |

### `thread.regenerateTitle` (write)

Asks for a new title written from the conversation.

| Parameter | Type | Required | |
|---|---|---|---|
| `threadId` | string | yes | The thread's id (see thread.list). |

### `thread.pin` (write)

Pins a thread to the top of the sidebar.

| Parameter | Type | Required | |
|---|---|---|---|
| `threadId` | string | yes | The thread's id (see thread.list). |

### `thread.unpin` (write)

Unpins a thread.

| Parameter | Type | Required | |
|---|---|---|---|
| `threadId` | string | yes | The thread's id (see thread.list). |

### `thread.settle` (write)

Settles a thread (done for now).

| Parameter | Type | Required | |
|---|---|---|---|
| `threadId` | string | yes | The thread's id (see thread.list). |

### `thread.reopen` (write)

Moves a settled thread back to Active.

| Parameter | Type | Required | |
|---|---|---|---|
| `threadId` | string | yes | The thread's id (see thread.list). |

### `thread.snooze` (write)

Hides a thread until a time (it comes back early if the agent needs you).

| Parameter | Type | Required | |
|---|---|---|---|
| `threadId` | string | yes | The thread's id (see thread.list). |
| `until` | string |  | ISO date; default in one hour. |
| `hours` | integer |  | Or: in this many hours. |

### `thread.wake` (write)

Ends a snooze.

| Parameter | Type | Required | |
|---|---|---|---|
| `threadId` | string | yes | The thread's id (see thread.list). |

### `thread.archive` (write)

Archives a thread.

| Parameter | Type | Required | |
|---|---|---|---|
| `threadId` | string | yes | The thread's id (see thread.list). |

### `thread.unarchive` (write)

Brings an archived thread back.

| Parameter | Type | Required | |
|---|---|---|---|
| `threadId` | string | yes | The thread's id (see thread.list). |

### `thread.stop` (write)

Stops the thread's agent session.

| Parameter | Type | Required | |
|---|---|---|---|
| `threadId` | string | yes | The thread's id (see thread.list). |

### `thread.revert` (destructive)

Reverts files and conversation to after turn N (0: before the first turn).

| Parameter | Type | Required | |
|---|---|---|---|
| `threadId` | string | yes | The thread's id (see thread.list). |
| `turnCount` | integer | yes | The number of turns to keep. |

### `thread.delete` (destructive)

Deletes a thread for good.

| Parameter | Type | Required | |
|---|---|---|---|
| `threadId` | string | yes | The thread's id (see thread.list). |

### `thread.diff` (read)

The unified diff of turns (default: the whole thread so far).

| Parameter | Type | Required | |
|---|---|---|---|
| `threadId` | string | yes | The thread's id (see thread.list). |
| `fromTurn` | integer |  | Start after this many turns. |
| `toTurn` | integer |  | End after this many turns. |

### `thread.search` (read)

Searches every thread's messages.

| Parameter | Type | Required | |
|---|---|---|---|
| `query` | string | yes | What to look for. |
| `limit` | integer |  | At most this many matches. |

## git

### `git.status` (read)

The git state where a thread works (or a project's folder): branch, changes, ahead/behind, its pull request.

| Parameter | Type | Required | |
|---|---|---|---|
| `threadId` | string |  | Where this thread works (its worktree, else its project's folder). |
| `projectId` | string |  | Or this project's folder. |

### `git.pull` (write)

Pulls the branch where a thread works (or a project's folder).

| Parameter | Type | Required | |
|---|---|---|---|
| `threadId` | string |  | Where this thread works (its worktree, else its project's folder). |
| `projectId` | string |  | Or this project's folder. |

### `git.commit` (write)

Commits, pushes and/or opens a pull request in one step (messages written for you when left out).

| Parameter | Type | Required | |
|---|---|---|---|
| `threadId` | string |  | Where this thread works (its worktree, else its project's folder). |
| `projectId` | string |  | Or this project's folder. |
| `action` | `commit` \| `push` \| `pr` \| `commit_push` \| `commit_push_pr` |  | What to do (default commit). |
| `message` | string |  | The commit message (default: written from the changes). |
| `files` | stringlist |  | Only these paths (default: every change). |
| `newBranch` | bool |  | Commit on a new feature branch. |

## pr

### `pr.list` (read)

Pull requests of zenith's projects (GitHub, GitLab, Azure DevOps, Forgejo, Bitbucket).

| Parameter | Type | Required | |
|---|---|---|---|
| `projectId` | string |  | Only this project's. |
| `state` | `open` \| `closed` \| `merged` \| `all` |  | Default open. |
| `query` | string |  | Only those matching this. |
| `limit` | integer |  | At most this many (default 50). |

### `pr.action` (write)

Acts on a pull request: merge, ready, draft, close, reopen, update its branch, auto-merge.

| Parameter | Type | Required | |
|---|---|---|---|
| `projectId` | string | yes | The project it belongs to. |
| `repository` | string | yes | owner/name, as pr.list gives it. |
| `number` | integer | yes | The pull request's number. |
| `action` | `merge` \| `ready` \| `draft` \| `close` \| `reopen` \| `update-branch` \| `enable-auto-merge` \| `disable-auto-merge` | yes | What to do. |
| `mergeMethod` | `merge` \| `squash` \| `rebase` |  | For merge (default: the project's). |

## terminal

### `terminal.open` (write)

Opens (or reuses) a terminal where a thread works.

| Parameter | Type | Required | |
|---|---|---|---|
| `threadId` | string | yes | The thread's id (see thread.list). |
| `terminalId` | string |  | Which of the thread's terminals (default "default"). |
| `cols` | integer |  | Width in columns (default 120). |
| `rows` | integer |  | Height in rows (default 32). |

### `terminal.run` (write)

Runs a shell command in a thread's terminal (opened if needed); read its output with terminal.read.

| Parameter | Type | Required | |
|---|---|---|---|
| `threadId` | string | yes | The thread's id (see thread.list). |
| `command` | string | yes | The command line. |
| `terminalId` | string |  | Which of the thread's terminals (default "default"). |

### `terminal.write` (write)

Types into a thread's terminal (\r is Enter, \u0003 is Ctrl-C).

| Parameter | Type | Required | |
|---|---|---|---|
| `threadId` | string | yes | The thread's id (see thread.list). |
| `data` | string | yes | What to type. |
| `terminalId` | string |  | Which of the thread's terminals (default "default"). |

### `terminal.read` (read)

What a thread's terminal shows: its recent output as plain text, and whether it still runs.

| Parameter | Type | Required | |
|---|---|---|---|
| `threadId` | string | yes | The thread's id (see thread.list). |
| `terminalId` | string |  | Which of the thread's terminals (default "default"). |
| `lines` | integer |  | The last this many lines (default 200). |

### `terminal.close` (write)

Closes a thread's terminal (and what runs in it).

| Parameter | Type | Required | |
|---|---|---|---|
| `threadId` | string | yes | The thread's id (see thread.list). |
| `terminalId` | string |  | Which of the thread's terminals (default "default"). |

## project

### `project.scripts` (read)

A project's scripts (dev server, tests, lint…), as set in its settings.

| Parameter | Type | Required | |
|---|---|---|---|
| `projectId` | string | yes | The project's id. |

### `project.runScript` (write)

Runs one of a project's scripts in a terminal of a thread of that project.

| Parameter | Type | Required | |
|---|---|---|---|
| `threadId` | string | yes | The thread's id (see thread.list). |
| `scriptId` | string | yes | The script's id (see project.scripts). |

## provider

### `provider.list` (read)

The agents' providers: status, version, sign-in, models.

### `provider.refresh` (write)

Checks the providers again (sign-in, versions, models).

| Parameter | Type | Required | |
|---|---|---|---|
| `provider` | string |  | Only this provider instance. |

## settings

### `settings.get` (read)

The server's settings.

### `settings.update` (write)

Changes server settings (a partial settings object).

| Parameter | Type | Required | |
|---|---|---|---|
| `patch` | object | yes | Keys of settings.get to change. |

### `settings.agentPermissions` (read)

What agents may do through zenith-mcp: off, read or full.

### `settings.setAgentPermissions` (write)

Sets what agents may do through zenith-mcp (not available to agents themselves).

| Parameter | Type | Required | |
|---|---|---|---|
| `mcp` | `off` \| `read` \| `full` | yes | off: nothing; read: read-only commands; full: everything. |

## sessions

### `sessions.list` (read)

Claude Code and Codex sessions on this Mac, with costs and totals.

| Parameter | Type | Required | |
|---|---|---|---|
| `limit` | integer |  | At most this many sessions (default 100). |

## lsuite

### `lsuite.apps` (read)

The other lsuite apps installed (from ~/.lsuite/apps), and how to drive them.

