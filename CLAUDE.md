@AGENTS.md

## lsuite: bring zenith up to the suite standard (notes merged 2026-10-01, updated 2026-10-02)

zenith is part of **lsuite** (lowercase), the free open-source creative suite with a music app
and a video app. Since 2026-10-01 zenith is the suite's **app for coding with agents**; since
2026-10-02 its window is **native Rust with GPUI** (`crates/zenith-app`; Tauri is gone), a client
of zenith's server (`crates/zenith-code`). The web interface (`code/apps/web`) still serves zenith
in a browser. Its page is lsuite.xyz/zenith (`../lsuite/zenith/index.html`). Two documents in the
lsuite repository (locally `../lsuite/`, which names the other apps and their repositories) are
the contract: `STANDARD.md` and `design/DESIGN.md` (the shared design system, live at
lsuite.xyz/design). The owner wants every lsuite app **100 % drivable by MCP, CLI and its
built-in agent**, written in Rust, with automatic updates, compatible with the others, and with
one common look. The suite's music app is the reference implementation (its folder and files are
named in STANDARD.md).

- [x] **Rust**: the server is `crates/zenith-code`, the window `crates/zenith-app` (GPUI 0.2.2 from
      crates.io, `runtime_shaders`), with `zenith-client` (WebSocket RPC, bearer session 0600 in
      `~/.zenith/app`) and `zenith-model` (what clients derive from the server's data). Still in
      TypeScript only: the Cursor, Grok, OpenCode and Antigravity drivers, provider sign-in and
      installers, devices.
- [x] **Command registry**: `crates/zenith-commands` (52 `family.verb` commands, validated in one
      place, `docs/COMMANDS.md` generated); the window runs every action through it (no raw
      orchestration command left in `crates/zenith-app`), like the CLI and MCP.
  - [x] git, pull requests, terminals and project scripts as commands (`git.*`, `pr.*`,
        `terminal.*`, `project.scripts` / `project.runScript`) and in the native window (git bar
        and menu in the thread's title bar, Git menu, terminal panel ⌘J drawn with `vt100`,
        scripts menu). Images in messages too (`images` on `thread.new` / `thread.send`, the
        composer's paperclip and paste).
- [x] **CLI**: `zenith-cli` (same registry, talks to the running server, wakes it up); MCP:
      `zenith-mcp --live`; agent permissions (off/read/full) in Settings › Agents.
      `docs/AI_CONTROL.md` explains how to drive zenith.
- [x] **Releases and auto-update** (code): `.github/workflows/release.yml` (macOS arm64 and x86_64
      signed and notarized when the six `APPLE_*` secrets exist, Linux x86_64, `SHA256SUMS` signed
      with Ed25519), `scripts/mac/package.sh`, the in-app updater (`zenith-commands/src/update.rs`,
      public key `crates/zenith-commands/assets/update-signing.pub`, `ZENITH_NO_UPDATE=1`,
      `app.checkUpdates`), and the app installs its own server LaunchAgent from the bundle.
  - [x] Secrets set on ludovic111/zenith (2026-10-02): App Store Connect key "zenith
        notarization", Developer ID certificate, `ZENITH_UPDATE_SIGNING_KEY` (private key in
        `~/.lsuite/keys/zenith-update-signing.key`). A release: bump `Cargo.toml`'s version, push
        the `vX.Y.Z` tag. First release: 0.2.0.
- [x] **Design system** (`../lsuite/design/`): the native window reads the tokens
      (`crates/zenith-app/assets/lsuite-tokens.json`, zenith blue, hue 262), Manrope and IBM Plex
      Mono bundled, sidebar on the macOS material (NSVisualEffectView, Sidebar) with glass tier 1,
      floating surfaces on tier 2's opaque fallback (GPUI cannot blur inside the window), work
      solid, "Reduce transparency" honored, contrast tested on every tier (`cargo test -p
      zenith-app`). Filled accents use step 700 in light mode (white on 600 is under 4.5:1). Icon
      redrawn from the template (`crates/zenith-app/assets/icon/zenith.svg`, rendered by
      `examples/render_icon.rs`).
  - [x] The web interface (`code/apps/web`): `src/lsuite-tokens.css` (zenith's copy) and
        `src/lsuite.css` make the lsuite look its default theme (tier 1 sidebar and top bars,
        tier 2 popovers, palette and composer, tier 3 dialogs, work solid), Manrope and IBM Plex
        Mono in `public/fonts`, `data-app="zenith"`, contrast tested (`src/lsuiteTheme.test.ts`);
        imported VS Code / Open VSX themes still apply on top; the Tauri integration is gone.
- [x] **Suite**: `~/.lsuite/apps/zenith.json` (format 1, as the suite's video app first wrote it, plus `bridge`);
      decided with the owner: zenith lists the installed lsuite apps in Settings and hands their
      MCP servers to the agents of its threads (`zc_core::lsuite`, `ZENITH_NO_LSUITE_MCP=1`).
- [x] Support links to `https://lsuite.xyz/zenith/support` (README, Settings); the lsuite page and
      `/zenith/download/<platform>` (`../lsuite/server.js`) are updated.
  - [ ] Keep the lsuite page up to date (version, what's new) with every release (0.2.0 done).

When done, tick these, and update the status table at the end of `../lsuite/STANDARD.md`.
