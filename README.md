# zenith

Part of [lsuite](https://lsuite.xyz), the free, open-source creative suite your AI can drive.
Website: **[lsuite.xyz/zenith](https://lsuite.xyz/zenith)** · Support: [lsuite.xyz/zenith/support](https://lsuite.xyz/zenith/support).

**An app for coding with agents.** zenith runs Claude Code, Codex and other coding agents in your projects: threads you can follow and steer, approvals, plans, diffs, worktrees, git and pull requests — and what it all costs. [Version française](README.fr.md).

It runs on your machine: the server answers only on `127.0.0.1`, uses your own Claude and ChatGPT subscriptions, and sends your code nowhere else.

## What you get

| | |
| --- | --- |
| **A native window** | Written in Rust with [GPUI](https://gpui.rs): the sidebar on the macOS material, every thread by status (pinned, active, snoozed, settled), the conversation with the agent's work grouped into one line per turn, images in your messages (paste or attach), approvals and questions answered in place, plans to implement in one click, the files each turn changed and their diffs, git (changes, commit, push, pull requests) and terminals (⌘J) where the thread works, the project's scripts, a command palette (⌘K), light and dark, on the lsuite design system |
| **Threads** | One conversation per task, with Claude Code or Codex, in the project's folder or a new worktree. Approval modes from "ask for everything" to "full access", plan mode, interrupts, checkpoints you can revert to |
| **Sessions & costs** | Every Claude Code and Codex session on this Mac, in zenith or in your terminal: cost and tokens per day, per project, and a resume command for each |
| **Everything is a command** | Every action is a named command (`thread.new`, `thread.approve`, `git.commit`, `terminal.run`…): the window, `zenith-cli` and `zenith-mcp` use the same registry. See [docs/COMMANDS.md](docs/COMMANDS.md) and [docs/AI_CONTROL.md](docs/AI_CONTROL.md) |
| **Agents can drive it** | `claude mcp add zenith -- /Applications/zenith.app/Contents/MacOS/zenith-mcp --live` lets any agent start threads, follow them and answer them (Settings › Agents decides how much). Agents in zenith's threads get the MCP servers of the other lsuite apps you have installed (music, video) |
| **In a browser too** | The server still serves zenith's web interface (File › Open in Browser): file search and pull request review live there for now |
| **Updates** | Signed releases from GitHub, installed in one click (Settings › Updates; `ZENITH_NO_UPDATE=1` turns it off) |

## Install

From the first signed release on, download zenith from [lsuite.xyz/zenith](https://lsuite.xyz/zenith) (macOS arm64 and Intel, Linux x86_64) and open it: it sets up its server (a LaunchAgent on `127.0.0.1:4747`) and keeps itself up to date.

From source (macOS): [Rust](https://rustup.rs), Xcode's command line tools, git; Node.js 22.16+ to build the web interface (optional). Optional: the [GitHub CLI](https://cli.github.com) logged in, Claude Code and/or Codex.

```bash
git clone https://github.com/ludovic111/zenith.git
cd zenith
npm run mac:install
```

This builds zenith, installs **zenith.app**, links `zenith-cli` and `zenith-mcp` into `~/.local/bin` when it exists, and keeps the server running in the background, so agents keep working when the window is closed. Run it again after pulling changes; `npm run mac:uninstall` removes it. Your threads and settings live in `~/.zenith/code`.

The server is the Rust one. `ZENITH_SERVER=node npm run mac:install` installs the original TypeScript server instead (some providers, such as Cursor or OpenCode, only exist there for now); both read and write the same data.

## How it is built

- `crates/zenith-app` — the window, in Rust with GPUI 0.2: theme from the lsuite tokens, native vibrancy, its own text field (input methods, selection, undo), Markdown, menus, the palette.
- `crates/zenith-client` — the connection to the server every client shares: WebSocket RPC with a bearer session (kept 0600 in `~/.zenith/app`), reconnection.
- `crates/zenith-model` — what clients derive from the server's data (sidebar sections and order, the work log, pending requests, the timeline), ported from the web interface and tested.
- `crates/zenith-commands` — the command registry, `zenith-cli`, `zenith-mcp`, lsuite discovery (`~/.lsuite/apps/zenith.json`) and the signed updater.
- `crates/zenith-code` — the server, in Rust: the WebSocket RPC and HTTP API, the event-sourced SQLite store, the Claude Code and Codex drivers, git, worktrees, checkpoints, terminals, pull requests, usage, MCP ([plan](docs/zenith-code-rust-plan.md), [verification](docs/zenith-code/verification.md)).
- `code/` — zenith code, a fork of [T3 Code](https://github.com/pingdotgg/t3code) (MIT): the web interface (`code/apps/web`, React) and the TypeScript server the Rust one is tested against. See [code/ZENITH.md](code/ZENITH.md).

```bash
cargo test --workspace
cargo clippy --workspace --all-targets
zenith-cli list                          # every command
```

Releases: push a `vX.Y.Z` tag matching `Cargo.toml`'s version; [.github/workflows/release.yml](.github/workflows/release.yml) builds, signs (Developer ID and notarization with the lsuite `APPLE_*` secrets) and publishes `zenith-macos-arm64.zip`, `zenith-macos-x86_64.zip`, `zenith-linux-x86_64.tar.gz`, `SHA256SUMS` and its signature (secret `ZENITH_UPDATE_SIGNING_KEY`).

## Privacy

- The server listens on `127.0.0.1` only. The window, `zenith-cli` and `zenith-mcp` sign in with a session the server's own command line issues; the browser gets a one-time pairing token.
- Agents run with your own CLIs and subscriptions (`claude`, `codex`).
- No account, no telemetry. Update checks ask GitHub for the latest release, and can be turned off.
- `npm run privacy -- --install` checks every push for secrets.

## License

MIT. Based on T3 Code by T3 Tools Inc. (MIT). Fonts: Manrope and IBM Plex Mono (SIL OFL). Icons: Lucide (ISC).
