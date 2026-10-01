@AGENTS.md

## lsuite: bring zenith up to the suite standard (notes merged 2026-10-01)

zenith is part of **lsuite** (lowercase), the free open-source creative suite with a music app
and a video app. Since 2026-10-01 zenith is the suite's **app for coding with agents** (zenith
code in a Tauri window, plus Sessions & costs); the life dashboard and personal agents are gone.
Its page is lsuite.xyz/zenith (`../lsuite/zenith/index.html`). Two documents in the lsuite
repository (locally `../lsuite/`, which names the other apps and their repositories) are the
contract: `STANDARD.md` and `design/DESIGN.md` (the shared design system, live at
lsuite.xyz/design). The owner wants every lsuite app **100 % drivable by MCP, CLI and its
built-in agent**, written in Rust, with automatic updates, compatible with the others, and with
one common look. The suite's music app is the reference implementation (its folder and files are
named in STANDARD.md).

- [x] **Rust**: the server is `crates/zenith-code` (a port of zenith code's TypeScript server,
      checked against it), the Mac app `crates/zenith-app`. Still in TypeScript only: the
      Cursor, Grok, OpenCode and Antigravity drivers, provider sign-in and installers, devices.
- [ ] **Command registry**: every action in the interface (threads, projects, git, pull
      requests, settings) as a named command, served to the UI, a CLI and the MCP server alike.
      Today the MCP server (`/mcp`, per thread) covers pull request links and the preview.
- [ ] **CLI**: a `zenith-cli` on the same registry (talks to the running server, token-protected,
      127.0.0.1 only). `zenith-code` has `auth` and `project` commands so far.
- [ ] **Releases and auto-update**: today zenith runs from source (`npm run mac:install`).
      Ship signed release builds (macOS notarized: same six `APPLE_*` secrets as the other apps,
      set with `../lsuite/scripts/set-apple-secrets.sh` and a new App Store Connect key "zenith
      notarization"; Linux) with a signed in-app updater, `ZENITH_NO_UPDATE=1`, and "check for
      updates" as a command.
- [ ] **Design system** (`../lsuite/design/`): zenith's signature color is **blue, hue 262**
      (`--ls-zenith-*`, accent `#72a6ff` dark / `#4777d2` light). The interface is
      `code/apps/web`: map its theme (`src/index.css`, `src/themePalette.ts`) onto `tokens.css`
      (`data-app="zenith"`) as the default theme, keep imported VS Code / Open VSX themes working
      on top, move the UI font to Manrope (code stays monospace), put the sidebar, title bar,
      composer, popovers, command palette and dialogs on the glass tiers over `.ls-backdrop` (the
      Mac app: window vibrancy), keep the thread log, diffs and terminals solid, keep
      `appearanceContrast.test.ts` passing with the glass tiers, and redraw the icon from the
      lsuite template.
- [ ] **Suite**: write `~/.lsuite/apps/zenith.json` (format in STANDARD.md); decide with the
      owner whether zenith still shows the other installed lsuite apps now that it is a coding app.
- [ ] Support links to `https://lsuite.xyz/zenith/support`; keep the lsuite page up to date
      (version, what's new) with every release.

When done, tick these, and update the status table at the end of `../lsuite/STANDARD.md`.
