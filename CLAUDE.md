@AGENTS.md

## lsuite: bring zenith up to the suite standard (next session, decided 2026-10-01)

zenith is part of **lsuite** (lowercase), the free open-source creative suite with a music app
and a video app; zenith is its hub. Its page is lsuite.xyz/zenith
(`../lsuite/zenith/index.html`). The contract is `STANDARD.md` in the lsuite repository (locally
`../lsuite/STANDARD.md`, which names the other apps and their repositories). The owner wants every lsuite app **100 % drivable by MCP, CLI and its
built-in agent**, written in Rust, with automatic updates, and compatible with the others.
The suite's music app is the reference implementation (its folder and files are named in
STANDARD.md). What zenith is missing:

- [ ] **Command registry**: every action in the interface (pages, Now items, routines, team,
      settings, projects) as a named command, served to the UI, a CLI, the MCP server and Ask
      zenith alike. Today the MCP covers reads and a few actions; make it complete and generated.
- [ ] **CLI**: a `zenith-cli` on the same registry (talks to the running server, token-protected,
      127.0.0.1 only).
- [ ] **Rust**: the port started on the `claude/rends-lapp-native-99a256` branch (server and Mac
      app in `crates/`). Continue it so the core is Rust like the rest of the suite.
- [ ] **Releases and auto-update**: today zenith runs from source and updates itself from GitHub.
      Ship signed release builds (macOS notarized, Linux) with a signed in-app updater,
      `ZENITH_NO_UPDATE=1`, and "check for updates" as a command.
- [ ] **Hub of the suite**: read `~/.lsuite/apps/*.json` (format in STANDARD.md) to show each
      installed lsuite app (version, update available, running or not, open documents), and let
      Ask zenith / agents delegate to the other lsuite apps through their MCP servers. Write
      `~/.lsuite/apps/zenith.json` too.
- [ ] Support links to `https://lsuite.xyz/zenith/support`; keep the lsuite page up to date
      (version, what's new) with every release.

When done, tick these, and update the status table at the end of `../lsuite/STANDARD.md`.
