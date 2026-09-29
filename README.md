# zenith

**A private sky over your projects and your day.** zenith is a local dashboard for people who build many things at once: every project's health, deploys, code and AI agents, your calendar, mail, money and news — plus **zenith code**, a full coding workspace for Claude Code and Codex, built in.

And it acts. **Ask zenith** anything in one sentence (⌘J): an AI agent starts on it at once, in the right project or across your whole life, with everything zenith knows. The **Now** list shows what is waiting for you — a failing payment, buyers to answer, a broken CI, a birthday — and hands each one to an agent in one tap. It prepares; you approve.

It runs on your machine, answers only on `127.0.0.1`, and never sends your data anywhere. [Version française](README.fr.md).

## What you get

| Page | What it shows |
| --- | --- |
| **Ask zenith** | One box, everywhere (overview, project pages, ⌘J, ⌘K): say what you want, an agent does it and opens as a thread. Routes to the project you name, else to your life agent |
| **Now** | What is waiting for you, most pressing first, each with a one-tap *Hand off* to an agent, *done* and *later*. Plus routines: agents that run on their own every morning |
| Overview | An orbit of your projects with live status, key numbers, project cards, six months of commits, a live feed of everything that happens |
| My day | Weather, air quality, UV and pollen, calendar (14 days, holidays, birthdays), what's waiting for you, now playing, screen time, spending, work rhythm |
| Projects | One page per project: uptime, latency, HTTP traffic and deploys (Railway), CI, issues and PRs (GitHub), releases and downloads, App Store rating and reviews, RevenueCat MRR, agent sessions, Obsidian notes, identity card |
| **Code** | **zenith code**: chat with Claude Code, Codex and other agents inside any of your projects — diffs, terminals, worktrees, approvals. Its threads live in zenith's sidebar, under their project |
| **Claude, ChatGPT** | Their desktop apps docked right in zenith's window (zenith.app), plugins and connectors included |
| Watch | Who talks about your projects (Hacker News, GitHub), notifications, new stars, contributions, news, markets, the state of your Mac |
| AI agents | Every Claude Code and Codex session: live, cost, lines written, PRs, the command to resume it |
| Subscriptions | Everything you pay for, monthly total in your currency, next charges, failing payments, Claude / ChatGPT plan gauges |
| Directory | Every project's names, handles, domains (registrar, renewal, certificate, email), stores and services |

Press **⌘J** to ask zenith, **⌘K** to jump (pages, projects, threads — or type a sentence to ask), **⌘B** to fold the sidebar.

## Quick start

Requirements: macOS or Linux, Node.js 22.16+ (24+ recommended), git. Optional: the [GitHub CLI](https://cli.github.com) logged in, Claude Code and/or Codex.

```bash
git clone https://github.com/ludovic111/zenith.git
cd zenith
npm install
cp zenith.config.example.json zenith.config.json   # then edit it: your projects, city, language
npm run dev                                         # http://127.0.0.1:4748
```

To build zenith code (the coding workspace) once:

```bash
npm run code:build
```

### Mac app

```bash
npm run mac:install
```

Builds everything, installs **zenith.app** (a native window) and keeps the server running in the background (a LaunchAgent on `127.0.0.1:4747`). Run it again after pulling changes; `npm run mac:uninstall` removes it. The app also reads Calendar, Reminders, Contacts (birthdays only), Mail, Music/Spotify and screen time — read-only, after macOS asks you.

## Configuration

Everything about you lives in **`zenith.config.json`** — at the root or in `perso/`, both git-ignored. It's validated on start; `zenith.schema.json` gives your editor autocompletion. See [docs/configuration.md](docs/configuration.md) for every field.

```jsonc
{
  "locale": "en-US",            // or "fr-FR", "fr-CH"… — French or English interface
  "currency": "USD",            // totals are converted to it
  "location": { "name": "New York", "latitude": 40.71, "longitude": -74.0, "country": "US" },
  "projects": [
    {
      "id": "my-app",
      "name": "My App",
      "dir": "my-app",                      // folder next to zenith/, or an absolute path
      "repo": "me/my-app",                  // GitHub
      "site": "https://my-app.com",
      "probes": [{ "label": "API", "url": "https://api.my-app.com/health" }],
      "releases": true,                     // count GitHub release downloads
      "appStore": { "id": "1234567890" },   // ratings and reviews
      "revenuecat": { "projectId": "1a2b3c4d" }
    }
  ]
}
```

API keys are pasted from **Data sources** (`/reglages`): they're written to `.env.local` (git-ignored) and applied without a restart. See `.env.example`. Without any key, zenith still shows your local repos, agent sessions, weather, news and more.

## For AI agents

Every 10 minutes zenith writes a Markdown brief of everything it knows (`context/brief.md`, one file per project, `vie.md`, `argent.md`…), also into your Obsidian vault. Give it to your agents with:

- **MCP**: `claude mcp add zenith --scope user -- node /path/to/zenith/scripts/mcp/zenith-mcp.mjs` (Codex: `codex mcp add zenith -- node …`). To read: `zenith_brief`, `zenith_project`, `zenith_document`, `zenith_search_notes`, `zenith_read_note`. To act (zenith running): `zenith_now`, `zenith_delegate` (start another agent in a project), `zenith_agent`, `zenith_done`.
- **HTTP**: `http://127.0.0.1:4747/api/context[/<doc>]` and `/llms.txt`.

## The zenith agent

Ask zenith, Now and routines run through zenith code, with your Claude Code or Codex subscription and your usual permissions. Life requests run in the agent's own folder (`~/.zenith/life`), where zenith writes its instructions (who you are, where everything is, what it must ask before doing) and plugs its MCP server in; it reaches your mail and calendar through your Claude connectors. It drafts, branches and proposes; it asks before sending, paying, deleting or deploying. See [docs/agent.md](docs/agent.md).

## zenith code

`code/` holds zenith code, a fork of [T3 Code](https://github.com/pingdotgg/t3code) (MIT) rebranded, themed and wired into zenith: it starts with zenith, knows your projects, pairs itself inside the dashboard, and shares zenith's sidebar, ⌘K and URLs (`/code/<environment>/<thread>`): one app, not an app in an app. It keeps its own state in `~/.zenith/code`. See [code/ZENITH.md](code/ZENITH.md) for what changed and how to sync with upstream.

## Extending

Built-in integrations (GitHub, Railway, RevenueCat, App Store, Obsidian, Claude Code, Codex…) switch on from the config. For anything specific to your projects — your own database, your own API, whole pages — write an extension in `perso/` (git-ignored, plugged in automatically when it exists): see [docs/extensions.md](docs/extensions.md). Keep `perso/` in its own private repository if you want it backed up.

## Privacy

- The server listens on `127.0.0.1` only and rejects any other `Host` header (DNS rebinding).
- Keys stay server-side in `.env.local`; nothing is sent to a third party except the API calls you configured.
- `perso/`, `zenith.config.json`, `.env.local`, `.data/` and `context/` are git-ignored.
- `npm run privacy` scans every tracked file for your config's identifying values (names, emails, domains, ids…) and for secrets; `npm run privacy -- --install` runs it before each `git push`. Useful when you contribute back.

## Stack

Next.js 16, React 19, Tailwind 4, Motion, cmdk, NumberFlow, Magic UI components. Fonts: Unbounded, Instrument Serif, Geist, JetBrains Mono. zenith code: Effect, Vite, TanStack Router.

## License

MIT. zenith code is based on T3 Code by T3 Tools Inc. (MIT).
