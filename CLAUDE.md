@AGENTS.md

## lsuite: bring zenith up to the suite standard (next session; notes updated 2026-10-01)

zenith is part of **lsuite** (lowercase), the free open-source creative suite with a music app
and a video app; zenith is its hub. Its page is lsuite.xyz/zenith (`../lsuite/zenith/index.html`).
Two documents in the lsuite repository (locally `../lsuite/`, which names the other apps and their
repositories) are the contract: `STANDARD.md` and `design/DESIGN.md` (the shared design system,
live at lsuite.xyz/design). The owner wants every lsuite app **100 % drivable by MCP, CLI and its
built-in agent**, written in Rust, with automatic updates, compatible with the others, and with one
common look. The suite's music app is the reference implementation (named in STANDARD.md).

Still to do:

- [ ] **Command registry**: every action in the interface (pages, Now items, routines, team,
      settings, projects) as a named command, served to the UI, a CLI, the MCP server and Ask
      zenith alike. Today the MCP covers reads and a few actions; make it complete and generated.
- [ ] **CLI**: a `zenith-cli` on the same registry (talks to the running server, token-protected,
      127.0.0.1 only).
- [ ] **Rust**: the port started on the `claude/rends-lapp-native-99a256` branch (server and Mac
      app in `crates/`). Continue it so the core is Rust like the rest of the suite.
- [ ] **Releases and auto-update**: today zenith runs from source and updates itself from GitHub.
      Ship signed release builds (macOS notarized: same six `APPLE_*` secrets as the other apps,
      set with `../lsuite/scripts/set-apple-secrets.sh` and a new App Store Connect key "zenith
      notarization"; Linux) with a signed in-app updater, `ZENITH_NO_UPDATE=1`, and "check for
      updates" as a command.
- [ ] **Design system** (`../lsuite/design/`): zenith's signature color is **blue, hue 262**
      (`--ls-zenith-*`, accent `#72a6ff` dark / `#4777d2` light), close to its current primary.
      Map `src/app/globals.css` (shadcn/Tailwind variables) and zenith code's theme onto
      `tokens.css` (`data-app="zenith"`), move from the system font to Manrope + IBM Plex Mono, put
      the sidebar, top bar, ⌘K / ⌘J, Now panel, popovers and dialogs on the glass tiers over
      `.ls-backdrop` (the Mac app: `NSVisualEffectView`), keep tables, charts and code solid, keep
      light and dark, add a contrast test, and redraw the icon from the lsuite template.
- [ ] **Hub of the suite**: read `~/.lsuite/apps/*.json` (format in STANDARD.md) to show each
      installed lsuite app (version, update available, running or not, open documents) with its
      signature color, and let Ask zenith / agents delegate to the other lsuite apps through their
      MCP servers. Write `~/.lsuite/apps/zenith.json` too.
- [ ] Support links to `https://lsuite.xyz/zenith/support`; keep the lsuite page up to date
      (version, what's new) with every release.

When done, tick these, and update the status table at the end of `../lsuite/STANDARD.md`.
