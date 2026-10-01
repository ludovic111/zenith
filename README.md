# zenith

Part of [lsuite](https://lsuite.xyz), the free, open-source creative suite your AI can drive.
Website: **[lsuite.xyz/zenith](https://lsuite.xyz/zenith)**.

**A Mac app for coding with agents.** zenith runs Claude Code, Codex and other coding agents in your projects: threads you can follow and steer, approvals, plans, diffs, terminals, worktrees, git and pull requests — and what it all costs. [Version française](README.fr.md).

It runs on your machine: the server answers only on `127.0.0.1`, uses your own Claude and ChatGPT subscriptions, and sends your code nowhere else.

## What you get

| | |
| --- | --- |
| **Threads** | One conversation per task, with Claude Code or Codex. Approvals, plan mode, interrupts, follow-ups while it works, images in your messages |
| **Code** | Diffs per turn and per thread, checkpoints you can revert to, a worktree per thread, file search, integrated terminals |
| **Git & PRs** | Commit, push and open a pull request in one step (messages written for you), review pull requests from GitHub, GitLab, Azure DevOps, Forgejo or Bitbucket |
| **Sessions & costs** | Every Claude Code and Codex session, in zenith or in your terminal: cost and tokens, spending by day and by model, your plan limits |
| **Agents can drive it** | Each thread hands its agent zenith's MCP server (`/mcp`), so it can link the pull requests it opens to its thread |
| **Mac app** | A native window: translucent sidebar, traffic lights in the title bar, light and dark appearance, menus, notifications |

## Install

Requirements: macOS, [Rust](https://rustup.rs), Node.js 22.16+ (24+ recommended, to build the interface), git. Optional: the [GitHub CLI](https://cli.github.com) logged in, Claude Code and/or Codex.

```bash
git clone https://github.com/ludovic111/zenith.git
cd zenith
npm install
npm run mac:install
```

This builds zenith, installs **zenith.app** and keeps its server running in the background (a LaunchAgent on `127.0.0.1:4747`), so agents keep working when the window is closed. Run it again after pulling changes; `npm run mac:uninstall` removes it. Your threads and settings live in `~/.zenith/code`.

The server is the Rust one. `ZENITH_SERVER=node npm run mac:install` installs the original TypeScript server instead; both read and write the same data, so you can switch back and forth.

## How it is built

- `crates/zenith-code` — the server, in Rust: the WebSocket RPC and HTTP API the interface talks to, the event-sourced SQLite store, the Claude Code and Codex drivers, git, worktrees, checkpoints, terminals, pull requests, usage, MCP. It is a port of zenith code's TypeScript server, checked against it ([plan](docs/zenith-code-rust-plan.md), [verification](docs/zenith-code/verification.md)).
- `crates/zenith-app` — zenith.app, a [Tauri](https://tauri.app) window around the interface.
- `code/` — zenith code, a fork of [T3 Code](https://github.com/pingdotgg/t3code) (MIT): the web interface (`code/apps/web`, React) and the TypeScript server the Rust one is tested against. See [code/ZENITH.md](code/ZENITH.md).

```bash
cargo test --workspace                  # the Rust server and app
cargo clippy --workspace --all-targets
```

## Privacy

- The server listens on `127.0.0.1` only; the interface signs in with a one-time pairing token the app mints itself.
- Agents run with your own CLIs and subscriptions (`claude`, `codex`).
- `npm run privacy -- --install` checks every push for secrets.

## License

MIT. Based on T3 Code by T3 Tools Inc. (MIT).
