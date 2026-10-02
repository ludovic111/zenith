# Driving zenith from an agent or a script

Everything a person does in zenith's window is a named command of one registry
(`crates/zenith-commands/src/registry.rs`): `family.verb`, a JSON object in, JSON out, validated in
one place. The window, `zenith-cli` and `zenith-mcp` all run the same commands against the same
running zenith (its server, which runs from login). The full list, with every parameter, is
[COMMANDS.md](COMMANDS.md), generated from the registry.

## The three ways in

| | How | Good for |
| --- | --- | --- |
| **MCP** | `claude mcp add zenith -- /Applications/zenith.app/Contents/MacOS/zenith-mcp --live` | Any agent (Claude Code, Codex, other MCP clients). Tool names are the commands with `_` for `.` (`thread_new`). |
| **CLI** | `zenith-cli <command> [--param value …]` or `--json '{…}'`; `zenith-cli list`, `zenith-cli help <command>` | Scripts, terminals. Prints JSON; exits 1 on an error, 2 on a usage mistake. |
| **The window** | The menus, the sidebar, the palette (⌘K) | People. |

`zenith-cli` and `zenith-mcp` live in `zenith.app/Contents/MacOS` (and `~/.local/bin` after
`npm run mac:install`). Both wake the server up if it sleeps.

## What agents may do

zenith › Settings › Agents sets what an agent connected through `zenith-mcp` may run, checked for
every call (`~/.zenith/app/agent-permissions.json`, `settings.agentPermissions`):

- **Off**: nothing (no tools are listed);
- **Read only**: commands that change nothing (`project.overview`, `thread.get`, `thread.diff`…);
- **Full** (default): everything, including starting agents and deleting threads.

An agent cannot change these permissions itself (`settings.setAgentPermissions` is refused
through MCP). The CLI is you: it can do everything.

## A session, step by step

```bash
zenith-cli project.overview                      # projects, their threads by section, what waits on you
zenith-cli project.add --path ~/code/my-app      # → {"projectId": …}
zenith-cli thread.new --projectId <id> --prompt "Add a dark mode" --runtimeMode approval-required --wait
```

`--wait` (or `thread.wait`) returns once the agent stops or needs you: the result is the thread
(`thread.get`), with `pendingApprovals` and `pendingQuestions` when it waits on you:

```bash
zenith-cli thread.approve --threadId <id> --decision accept          # the oldest pending approval
zenith-cli thread.answer --threadId <id> --json '{"answers": {"color": "Blue"}}'
zenith-cli thread.send --threadId <id> --prompt "Now add tests" --wait
zenith-cli thread.diff --threadId <id>                                # the unified diff so far
zenith-cli thread.revert --threadId <id> --turnCount 1                # back to after turn 1
```

Git, pull requests, terminals and the project's scripts, where a thread works:

```bash
zenith-cli git.status --threadId <id>                                 # branch, changes, ahead/behind, its PR
zenith-cli git.commit --threadId <id> --action commit_push_pr         # messages written for you
zenith-cli pr.list --state open
zenith-cli terminal.run --threadId <id> --command "npm test"
zenith-cli terminal.read --threadId <id> --lines 50                   # its output, as plain text
zenith-cli project.runScript --threadId <id> --scriptId dev
```

Through MCP the same calls are `project_overview`, `thread_new {projectId, prompt, wait: true}`,
`thread_approve {threadId, decision}`… Results are JSON (also in `structuredContent`).

## Concepts

- **Project**: a folder (`workspaceRoot`). **Thread**: one conversation with one agent in a
  project, in its folder or in a worktree of its own (`thread.new --worktree`).
- **Sections** (as in the sidebar): `pinned`, `active`, `snoozed` (hidden until a time, back early
  if the agent needs you), `settled` (done for now), `archived`.
- **Status**: `approval` and `input` (the agent waits on you), `working`, `failed`, `monitoring`,
  `plan-ready` (a plan waits to be implemented), `ready`.
- **Approval modes** (`runtimeMode`): `approval-required` (asks before commands and file
  changes), `auto-accept-edits`, `auto`, `full-access` (never asks).
- **Plan mode** (`plan: true`): the agent proposes a plan and waits; send "implement" or use
  the window's button.

## Finding zenith

zenith writes `~/.lsuite/apps/zenith.json` when it starts (lsuite discovery, format 1): the
paths of the app, `zenith-cli` and `zenith-mcp`, the server's address (`bridge.url`, loopback
only) and the file holding the session token (`bridge.tokenFile`, 0600). zenith reads the other
apps' files too, and hands their MCP servers to the agents of its threads, so an agent working
in zenith can drive the suite's other apps.

## Below the commands

The commands use the server's WebSocket RPC (`/ws`, the same protocol as the web interface;
see [zenith-code-rust-plan.md](zenith-code-rust-plan.md) §1) with a bearer session issued by
`zenith-code auth session issue`. Prefer the commands: they are the stable surface.
