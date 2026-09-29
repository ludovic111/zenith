# zenith code

`code/` is **zenith code**, the coding workspace of zenith: a fork of
[T3 Code](https://github.com/pingdotgg/t3code) (MIT, © T3 Tools Inc.), rebranded,
themed and wired into the dashboard. The upstream commit it is based on is in
[`UPSTREAM`](UPSTREAM). Everything else in this folder is upstream's code and docs.

## Build and run

Requirements: Node 22.16+ or 24 (the version zenith runs on), network access for the
first install. pnpm is not needed globally: the scripts call `npx -y pnpm@11.10.0`.

```bash
npm run code:build     # from zenith's root: install + build (≈1 min the first time)
```

This produces `apps/server/dist/bin.mjs` with the web client in `apps/server/dist/client`.
`scripts/mac/install.sh` runs it too (and skips it gracefully if it fails).

You never start it by hand: when zenith starts (`src/instrumentation.ts`) it spawns

```
node code/apps/server/dist/bin.mjs serve --host 127.0.0.1 --port <code.port> --base-dir <code.home>
```

restarts it with backoff if it crashes, reuses one left over by a previous zenith
process, and stops it when zenith exits. The dashboard shows it at `/code`
(`/code?project=<id>` opens a project, `zenith` is the dashboard itself).

| `zenith.config.json` | default | |
| --- | --- | --- |
| `code.enabled` | `true` | start zenith code with zenith |
| `code.port` | `4749` | always bound to `127.0.0.1` |
| `code.home` | `~/.zenith/code` | state: database, settings, worktrees. Never `~/.t3`, so an upstream T3 Code install is left alone |

Logs: `.data/code.log` in zenith's folder (pairing tokens are redacted).
Status: `GET /api/code`. Restart: the button on `/code`.

On start, zenith registers the dashboard and every configured project folder as
zenith code projects, once each (`<code.home>/zenith-projects.json` remembers them, so
a project you remove in zenith code stays removed).

Development: `npm run code:dev` runs upstream's dev runner (Vite + server) with its
own state in `~/.zenith/code-dev`.

## Embedding and pairing

zenith code requires pairing even on loopback. Inside the dashboard it happens
without user action:

1. `/code` renders an iframe of `http://127.0.0.1:<port>/`. Unpaired, the app lands on
   `/pair` and posts `{ type: "zenith-code:pair-request" }` to its parent.
2. The `/code` page (only for messages from its own iframe and the code origin) calls
   `POST /api/code/pair`, a same-origin-only route that mints a one-time owner token
   with `bin.mjs auth pairing create --admin --ttl 2m`.
3. It answers `{ type: "zenith-code:pair-token", token }` (or `zenith-code:pair-error`);
   the app submits it and gets its 30-day session cookie. `127.0.0.1:4747` and
   `127.0.0.1:<port>` are same-site, so the cookie works in the iframe.

The app only talks to parents listed by `ZENITH_CODE_PARENT_ORIGINS` (server env,
default `http://127.0.0.1:4747,http://127.0.0.1:4748`), which it reads from
`GET /zenith/embed.json`. The same list feeds a `Content-Security-Policy:
frame-ancestors 'self' …` header on every HTML page, so no other site can frame it.

Project focus: `?zenithProject=<absolute path>` on first load (kept in sessionStorage
across the pairing redirect), or `{ type: "zenith-code:open-project", path }` from the
parent later, opens that project's latest thread, or a new draft. The app posts
`{ type: "zenith-code:ready" }` once signed in.

"Open in its own window" goes through `GET /api/code/open`, which mints a token and
redirects to `/pair#token=…`, only for navigations started by the user or zenith
(`Sec-Fetch-Site: none | same-origin`).

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
(`"zenith code"`). Upstream spells "T3 Code" in ~150 user-facing strings, so rather
than rewrite them (and conflict on every sync), a build plugin swaps the name in
first-party modules and `index.html` for both the web and server builds. Direct edits
are limited to: `apps/web/src/branding.ts` (no "Alpha" suffix), `index.html` (title),
`public/manifest.webmanifest`, the icons (`apps/web/public/*`, `assets/*/…-web-*`,
generated from zenith's icon), `components/T3Wordmark.tsx` (now zenith's sun mark,
export name kept), and the sidebar/welcome wordmarks (`SidebarChrome.tsx`,
`WelcomeWizard.tsx`).

**Kept upstream names on purpose:** package names (`@t3tools/*`, `t3`), the `t3`
binary, `T3CODE_*` environment variables, `t3code:*` storage keys, cookie names, the
`t3.json` project file, the `t3-code` MCP server id. They are invisible or technical,
renaming them would touch hundreds of files and conflict on every sync, and zenith
never installs the `t3` binary on your PATH (it runs `bin.mjs` directly), so it
cannot shadow an upstream install.

**Theme.** A built-in dark theme `zenith` (`packages/shared/src/zenithTheme.ts`,
registered in `themePalettes.ts`) with zenith's deep-space canvas and sun accent,
plus Geist / JetBrains Mono / Unbounded (self-hosted via `@fontsource-variable/*`,
`apps/web/src/index.css`). It is the default when nothing is stored
(`hooks/useTheme.ts`, mirrored in the `index.html` boot script); other themes stay
selectable in Settings → Appearance.

**Nothing phones home to T3's infrastructure by default.**
- Telemetry (PostHog) is off unless `T3CODE_TELEMETRY_ENABLED=true` *and*
  `T3CODE_POSTHOG_KEY` are set; the anonymous id is not even computed when off
  (`telemetry/AnalyticsService.ts`).
- The `update`, `service`, `uninstall`, `app` and `triage` commands are removed from
  the CLI (`bin.ts`): they install upstream npm/GitHub releases, drive the upstream
  desktop app or file upstream issues. zenith builds, runs and updates zenith code.
- T3 Connect (relay, Clerk sign-in) only exists in builds made with its public keys;
  ours has none, so it stays inert.
- Still reaching the network, on purpose: the provider model manifest (refreshed from
  upstream's GitHub, keeps model lists current), provider installers (Codex from
  OpenAI's releases), and, only if you add an SSH remote environment, the upstream
  `t3` runtime on that remote host.

**Server additions:** `apps/server/src/zenith/embed.ts` (`/zenith/embed.json`, the
frame-ancestors policy, registered in `server.ts` and `http.ts`); `auth pairing create
--admin` (`cli/auth.ts`); default state dir `~/.zenith/code` (`os-jank.ts`, and
`~/.zenith/code-dev` in `scripts/dev-runner.ts`).

**Web additions:** `apps/web/src/zenith/` (embed messaging, embedded pairing hook used
by `components/auth/PairingRouteSurface.tsx`, project focus coordinator mounted in
`routes/__root.tsx`, `?zenithProject=` captured in `main.tsx`).

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
the lockfile. Then `npm run code:build`, check `/code`, and commit.

Where conflicts usually land: the files listed above, and binary icons if upstream
redraws them (keep ours). If upstream adds user-facing text with "T3" alone (not
"T3 Code"), the brand plugin won't catch it; adjust it where it shows.

## License

MIT. `LICENSE` keeps T3 Tools Inc.'s notice and adds one for the zenith code
modifications; `NOTICE` credits T3 Code as the base.
