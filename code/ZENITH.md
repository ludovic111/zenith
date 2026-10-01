# zenith (code/)

`code/` is **zenith**: the coding workspace for Claude Code, Codex and other agents, its
server and web app, shown in zenith.app (`crates/zenith-app`). It is a fork of
[T3 Code](https://github.com/pingdotgg/t3code) (MIT, © T3 Tools Inc.), rebranded and
themed. The upstream commit it is based on is in [`UPSTREAM`](UPSTREAM). Everything else
in this folder is upstream's code and docs. (It used to be "zenith code", one part of a
personal dashboard; the dashboard is gone and this is the whole product.)

The server is being rewritten in Rust (`crates/zenith-code`, plan in
`docs/zenith-code-rust-plan.md`). Until it is ready, this TypeScript server runs.

## Build and run

Requirements: Node 22.16+ or 24, network access for the first install. pnpm is not
needed globally: the scripts call `npx -y pnpm@11.10.0`.

```bash
npm run code:build     # from zenith's root: install + build (≈1 min the first time)
```

This produces `apps/server/dist/bin.mjs` with the web client in `apps/server/dist/client`.

On a Mac, `npm run mac:install` (`scripts/mac/install.sh`) builds it, builds zenith.app,
and runs the server as a LaunchAgent (label `dev.zenith.app`, or `ZENITH_BUNDLE_ID`):

```
node code/apps/server/dist/bin.mjs serve --host 127.0.0.1 --port 4747 --base-dir ~/.zenith/code
```

with `ZENITH_NO_STARTUP_TOKEN=1` (see below). Logs: `~/Library/Logs/Zenith/server.log`.
State (database, settings, worktrees) lives in `~/.zenith/code` (`ZENITH_CODE_HOME` at
install), never `~/.t3`, so an upstream T3 Code install is left alone.

The command line is spelled in two places only: `SERVER` in `scripts/mac/install.sh` and
`server_command()` in `crates/zenith-app/src/server.rs` (the app runs the CLI to mint
pairing tokens). The Rust server takes the same arguments, so switching to it is one line
in each: `SERVER=("$ROOT/target/release/zenith-code")` and
`Command::new(root().join("target/release/zenith-code"))`.

Development: `npm run code:dev` runs upstream's dev runner (Vite + server) with its
own state in `~/.zenith/code-dev`.

## zenith.app

zenith.app (Tauri) shows `http://127.0.0.1:4747` (`ZENITH_URL` and `ZENITH_CODE_HOME`
override the server and its state dir for a development build), a waiting page while the
server starts, and reconnects when it restarts.

**Pairing.** The server requires pairing even on loopback. zenith.app does it without
user action:

1. Unpaired, the web app redirects to `/pair`. The app's init script (`PAIRING` in
   `crates/zenith-app/src/main.rs`) notices it and invokes the app's `pairing_token`
   command (allowed for `http://127.0.0.1:*` by `capabilities/main.json`; the command
   also checks the page is the server's own origin).
2. The app runs `auth pairing create --ttl 2m --admin --label zenith --json --base-dir
   <state dir>` and returns `credential`, never logged.
3. The script puts it in the URL (`location.replace(…#token=…)`); the pairing screen takes
   it on `hashchange` (`zenith/useEmbeddedPairing.ts`), or on mount if it was already
   there, submits it and gets its 30-day session cookie.

Open in Browser (⌥⌘O) mints a token the same way and opens `/pair#token=…`.

**Title bar.** The traffic lights sit over the web app's top bar. `zenith/app.ts`
recognizes zenith.app (`window.__TAURI_INTERNALS__`), marks `<html data-zenith-app>` so
`index.css` gives `--workspace-controls-left` room for the lights (none when the app marks
`data-fullscreen`), and sends presses in the top bar, off its controls, to the app's
`shell_drag` / `shell_zoom` commands: the window moves, or zooms on a double-click, as
with any title bar. The app's menu opens pages with a `zenith:navigate` event
(Settings… → `/settings/general`).

**Startup token.** `serve` normally prints a startup pairing token, URL and QR code. With
`ZENITH_NO_STARTUP_TOKEN=1` (the LaunchAgent sets it) it prints only that the server is
ready: its output is a log file, and zenith.app mints its own tokens.

## Embedding (dormant)

When zenith was a dashboard, it showed this app in an iframe, and the app talked to its
parent page: pairing tokens, project focus, navigation requests, a sidebar snapshot,
title-bar presses (`zenith/embed.ts`, `ZenithEmbedCoordinator.tsx`, `/zenith/embed.json`,
`ZENITH_CODE_PARENT_ORIGINS`, the `frame-ancestors` policy). Nothing frames it any more;
the code only acts when the app is framed by an allowed parent and can be removed.

## What changed from upstream, and why

Kept small and in few files so upstream merges stay easy.

**Removed** (not shipped, excluded from syncs): `apps/mobile`, `apps/marketing`,
`apps/desktop`, `infra/`, `.repos/`. Follow-ups: `pnpm-workspace.yaml` drops `infra/*`
and sets `allowUnusedPatches` (the mobile/desktop patches stay for easy merges);
root `package.json` loses the desktop/mobile/marketing/release scripts and its
`prepare` no longer runs `vp config` (it would point zenith's git hooks at `code/`);
`apps/web/vite.config.ts` stops reading `apps/desktop/package.json` for licenses;
`pnpm-lock.yaml` is regenerated.

**Brand.** The product name lives in one place, `scripts/lib/zenith-brand.ts`
(`"zenith"`). Upstream spells "T3 Code" in ~150 user-facing strings, so rather
than rewrite them (and conflict on every sync), a build plugin swaps the name in
first-party modules and `index.html` for both the web and server builds. Direct edits
are limited to: `apps/web/src/branding.ts` (no "Alpha" suffix), `index.html` (title),
`public/manifest.webmanifest`, the icons (`apps/web/public/*`, `assets/*/…-web-*`,
generated from zenith's icon), `components/T3Wordmark.tsx` (now zenith's mark in the
current color, export name kept), and the sidebar/welcome wordmarks (`SidebarChrome.tsx`,
`WelcomeWizard.tsx`).

**Kept upstream names on purpose:** package names (`@t3tools/*`, `t3`), the `t3`
binary, `T3CODE_*` environment variables, `t3code:*` storage keys, cookie names, the
`t3.json` project file, the `t3-code` MCP server id. They are invisible or technical,
renaming them would touch hundreds of files and conflict on every sync, and zenith
never installs the `t3` binary on your PATH (it runs `bin.mjs` directly), so it
cannot shadow an upstream install.

**Theme.** The default is upstream's stock palette following the system's light or dark
appearance (`hooks/useTheme.ts`, mirrored in the `index.html` boot script). The boot script drops a stored
`zenith` theme once (it used to be the default). That older dark theme stays selectable
(`packages/shared/src/zenithTheme.ts`, registered in `themePalettes.ts`; its Geist /
JetBrains Mono fonts are self-hosted via `@fontsource-variable/*` in `apps/web/src/index.css`).

**Nothing phones home to T3's infrastructure by default.**
- Telemetry (PostHog) is off unless `T3CODE_TELEMETRY_ENABLED=true` *and*
  `T3CODE_POSTHOG_KEY` are set; the anonymous id is not even computed when off
  (`telemetry/AnalyticsService.ts`).
- The `update`, `service`, `uninstall`, `app` and `triage` commands are removed from
  the CLI (`bin.ts`): they install upstream npm/GitHub releases, drive the upstream
  desktop app or file upstream issues. zenith builds, runs and updates itself.
- T3 Connect (relay, Clerk sign-in) only exists in builds made with its public keys;
  ours has none, so it stays inert.
- Still reaching the network, on purpose: the provider model manifest (refreshed from
  upstream's GitHub, keeps model lists current), provider installers (Codex from
  OpenAI's releases), and, only if you add an SSH remote environment, the upstream
  `t3` runtime on that remote host.

**Server additions:** `apps/server/src/zenith/embed.ts` (`/zenith/embed.json`, the
frame-ancestors policy, registered in `server.ts` and `http.ts`); `auth pairing create
--admin` (`cli/auth.ts`); default state dir `~/.zenith/code` (`os-jank.ts`, and
`~/.zenith/code-dev` in `scripts/dev-runner.ts`); `ZENITH_NO_STARTUP_TOKEN`
(`serverRuntimeStartup.ts`).

**Web additions:** `apps/web/src/zenith/`: `app.ts` (zenith.app's title bar and menu
navigation, installed in `main.tsx`, its traffic-light room in `index.css`), the pairing
hook used by `components/auth/PairingRouteSurface.tsx` (zenith.app's `#token=`, and the
dormant iframe pairing), and the dormant embedding (the coordinator mounted in
`routes/__root.tsx`, `?zenithProject=` / `?zenithChrome=` captured in `main.tsx`,
`components/AppSidebarLayout.tsx` skipping its sidebar when a parent draws it).

**Sessions page** (`/sessions`, next to Usage): every Claude Code and Codex session of
the Mac (live state, cost, tokens, lines, PRs, subagents, model, branch, project, the
command that resumes it), with totals for today, the week, per day and per project.
Costs and limits stay on Usage. The data comes from `GET /api/zenith/sessions`, which
only zenith code's Rust server has (`crates/zenith-code/crates/zc-sessions`); against
the TypeScript server the page says "Sessions are available with zenith's Rust server".
Files: `apps/web/src/zenith/SessionsPage.tsx` and `sessions.ts` (fetch, types, tests),
`routes/sessions.tsx` (and the regenerated `routeTree.gen.ts`); one-line edits in
`components/sidebar/SidebarChrome.tsx` (the sidebar button after Usage),
`components/sidebar/mainAppLocation.ts` (a utility page, like Usage) and
`components/CommandPalette.tsx` ("Open sessions").

Edits inside upstream files are marked with a `zenith:` comment where the format
allows (`grep -rn "zenith:" code`); `git log -p -- code` after the import commit shows
all of them.

## Syncing with upstream

```bash
npm run code:sync            # or: bash scripts/code-sync.sh [branch-or-commit]
```

The script (zenith's `scripts/code-sync.sh`) needs a clean `code/`. It clones
pingdotgg/t3code, diffs `UPSTREAM..main` without the removed folders and the
lockfile, applies it with `git apply -3 --directory=code`, updates `UPSTREAM`, lists
conflicts (fix the `<<<<<<<` markers), and on a clean apply reinstalls to regenerate
the lockfile. Then `npm run code:build`, check the app, and commit.

Where conflicts usually land: the files listed above, and binary icons if upstream
redraws them (keep ours). If upstream adds user-facing text with "T3" alone (not
"T3 Code"), the brand plugin won't catch it; adjust it where it shows.

## License

MIT. `LICENSE` keeps T3 Tools Inc.'s notice and adds one for zenith's
modifications; `NOTICE` credits T3 Code as the base.
