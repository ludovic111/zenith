# The zenith agent

*[Français](agent.fr.md)*

zenith sees everything: your projects, your day, your money, your messages. The agent is what lets it **act**. You say what you want in one sentence; an AI agent (Claude Code or Codex) starts on it at once, in the right place, with everything zenith knows, and you follow it in a thread you can read, answer and approve.

## Ask zenith

Three ways, one box:

- **The overview**: the bar under the greeting.
- **Anywhere**: **⌘J**, or the *Ask zenith* button at the top of the sidebar.
- **⌘K**: type a sentence; when nothing else matches, *Ask zenith* is the answer. Enter.

Where it goes is decided as you type, and shown on the chip under the box:

- **A project** when your sentence names exactly one (its id, name, folder, repository, or a one-word identity name): the agent works in that project's folder. *"fix the login bug in my-app"* → My App.
- **My life** otherwise: the agent's own folder (`~/.zenith/life`), where it sees your whole life and can hand work to project agents.
- `@id` at the start forces a destination: `@zenith make the sidebar narrower`. The chip is also a menu.

The other chip picks **Claude** or **Codex**. The model is `agent.model` when set, else zenith code's default, else the one of your latest thread. Permissions are zenith code's (its default runtime mode, or the project's).

Each request is a zenith code thread: it streams, asks for approvals, shows diffs and terminals like any other. Your life conversations are listed under **zenith → Conversations** in the sidebar, project threads under their project.

## Now

The overview's **Now** list is what is waiting for you, most pressing first, computed from what zenith already knows:

| | From |
| --- | --- |
| A site that stopped answering (two checks in a row) | your `probes` |
| A failing payment | `subscriptions` with `"status": "failing"` |
| A birthday today or in the next two days | Contacts (zenith.app) |
| Buyers waiting about something you sell, grouped per item | My life (`sales`, `inbox.needsReply`) |
| An email waiting for your reply | My life (`inbox.needsReply`) |
| Paperwork due within a month or received in the last ten days | My life (`civic`) |
| A workflow failing on a main branch | GitHub notifications |
| Email and calendar not captured for 20 hours | My life (`capturedAt`) |

Agents waiting for your answer or approval come first.

Each item has one gesture: **Hand off**. zenith starts an agent with a precise request written for that item (*read the thread, draft a reply in my voice as a Gmail draft, don't send it*), and you land in its thread. The item then shows the agent's state. **✓** files it away, **🕑** hides it until tomorrow; both can be undone. An item that changes (a new message, another failure) comes back as a new one.

## Its team

One main agent (the one for "My life") and, if you like, **bots**: agents with a first name, a job and a face (a bright shape with two eyes and an accessory, like Dots and Grok Bots), each with its own folder, personality and memory, running on **your Claude subscription** (through Claude Code) or **your ChatGPT subscription** (through Codex). The team of ChatGPT Dots and Grok Bots, on your own subscriptions, on your Mac.

```json
"agent": {
  "name": "Celeste", "shape": "circle", "color": "#F5A524", "accessory": "star",
  "bots": [
    { "id": "inbox", "name": "Margot", "title": "Mail", "shape": "pill", "color": "#EC4899", "accessory": "bow", "provider": "claude", "role": "Keeps my inbox and calendar: triages, drafts replies, never sends." },
    { "id": "ops", "name": "Hugo", "title": "Workshop", "shape": "square", "color": "#F97316", "accessory": "antenna", "provider": "codex", "role": "Keeps my projects healthy: CI, dependencies, PRs. Fixes on a branch, never on main." }
  ]
}
```

- **Talk to them**: `@margot …` in the bar, ⌘J or ⌘K; or the destination menu; or their card in **AI agents → Team**. The Claude/Codex chip follows the bot's subscription.
- **They talk to each other**: `zenith_message` lets an agent write to a teammate and get the answer (Iris on Codex asks Margot on Claude what's in your inbox); each pair keeps one conversation. `zenith_delegate` hands over a long task, `zenith_team` says who does what and who is busy. A chain of agents asking agents stops at three.
- Their conversations are in the sidebar, under **Conversations**.

## What it knows, what it learns

Each agent has a folder (`~/.zenith/life` for the main one, `~/.zenith/bots/<id>` for bots), the Hermes Agent way:

| File | |
| --- | --- |
| `SOUL.md` | Its personality: tone, habits, what it always or never does. Written once (a bot's role), then yours. |
| `USER.md` | What the team knows about you: preferences, how you write, people who matter. One, shared, in the main folder. |
| `MEMORY.md` | Its memory: decisions, lessons, where things stand. |
| `skills/` | The team's know-how, one `SKILL.md` each (main folder, shared). |
| `AGENTS.md` | Its instructions, rewritten by zenith on every request with all of the above copied in: Codex reads it, Claude Code imports it through `CLAUDE.md`. Don't edit it. |

The instructions tell it who you are, where your brief and documents are, your projects and their folders, its team, its skills, and the rules:

1. **Do, don't describe**, then sum up in one to three lines.
2. **Ask before anything that leaves the Mac or can't be undone**: sending an email or a message, posting, paying, buying, answering an invitation, deleting, pushing to a main branch, deploying to production. It prepares (a draft, a branch, a PR), shows the exact content, and asks.
3. No card numbers, passwords, addresses or phone numbers in files; your data never goes into zenith's (public) repository.

And to **learn without being asked**: a correction or a preference goes in `USER.md`, a decision in `MEMORY.md`, a job it will do again becomes a skill. These files stay short (past a limit, zenith truncates them and asks it to consolidate).

What it can reach: zenith's brief and documents, your Obsidian notes, the shell (`git`, `gh`…), the web, and its session's tools — **your Claude connectors** (Gmail, Google Calendar, Drive…) on Claude, Codex's plugins on Codex. When one is missing, it says which. The folder also plugs in zenith's MCP server for both Claude Code (`.mcp.json`) and Codex (`.codex/config.toml`).

## Acting on anything

Besides zenith's own tools, each agent is told how to act on the Mac (`open`, AppleScript for Mail, Calendar, Notes, Reminders, Messages…, `shortcuts run` for your Shortcuts), on the web (its session's browser tools: Codex's browser and computer use, Claude's connectors and Claude in Chrome) and on your services (`gh`, `railway`…). Anything else with an MCP server can be given to the whole team at once:

```json
"agent": {
  "mcp": {
    "linear": { "url": "https://mcp.linear.app/mcp" },
    "playwright": { "command": "npx", "args": ["@playwright/mcp@latest"] }
  }
}
```

zenith writes them into every agent's folder, for Claude Code and Codex alike.

## Skills

Written know-how, in `~/.zenith/life/skills/<id>/SKILL.md` (the agentskills.io format Claude Code and Codex both read; zenith links it into each folder's `.claude/skills` and `.agents/skills`). zenith lays down five to start — `plan-day`, `reply-email`, `weekly-review`, `watch`, `write-skill` — then the team writes more as it works. Edit or delete any of them: zenith never rewrites a skill it already laid down. They are listed in **AI agents → Skills**, and a routine can follow one (`"skill": "weekly-review"`).

## Agents asking agents

The [MCP server](../README.md#for-ai-agents) lets any agent act through zenith, not only read it:

| Tool | |
| --- | --- |
| `zenith_now` | What is waiting, with ids. |
| `zenith_delegate` | Start another agent in a project, with a bot of the team (by its id), in `life` or in `zenith`, with a self-contained brief. It runs in parallel and appears in the sidebar. |
| `zenith_agent` | The state and last messages of an agent started that way. |
| `zenith_done` | File a Now item away, or snooze it. |

So the life agent can split *"get my-app ready for the App Store review"* into a code task in My App and an email to Apple, and check on both. A ceiling of 12 delegations an hour keeps a loop from running away.

## Routines

Agents that run on their own, listed in **AI agents → Routines** with their last run and a **Run** button. Two kinds:

- **At a set time** (`at`), once a day, on the days you pick.
- **On an event** (`on`): each new Now item of those kinds (a broken CI, an email waiting for a reply, a failing payment…) is handed to the agent as it appears, once, with the item's own request. What was already waiting when you add the routine stays yours; **Run** hands it over anyway.

Either can be done by a bot (`bot`) and follow a skill (`skill`):

```json
"agent": {
  "routines": [
    { "id": "morning", "at": "07:30", "task": "refresh-life" },
    { "id": "day", "at": "07:45", "skill": "plan-day" },
    { "id": "friday", "at": "18:00", "days": [5], "skill": "weekly-review" },
    { "id": "ci", "on": ["ci"], "bot": "ops" },
    { "id": "replies", "on": ["reply", "sale"], "bot": "inbox", "skill": "reply-email" }
  ]
}
```

`refresh-life` is built in: it captures Gmail and Google Calendar into My life, so the overview, Now and the brief are fresh when you wake up. A Mac asleep at that time catches up within three hours; each routine runs at most once a day, even with two zenith servers running. Events are checked every five minutes, three items at most per routine and twelve an hour in all. See [configuration](configuration.md#agent) for every field.

## From your phone (Telegram)

Talk to your agent from Telegram, as with Hermes Agent:

1. Create a bot with [@BotFather](https://t.me/BotFather) and paste its token in **Settings → zenith agent → Telegram** (it goes to `.env.local` as `TELEGRAM_BOT_TOKEN`).
2. Add `"gateway": { "telegram": { "chats": [] } }` to `agent` and send `/start` to your bot: it answers with your chat id. Put it in `chats` and restart zenith.

A message starts a conversation with your agent (or with `target`: a bot, a project; `@id` at the start works too), or continues the one from the last two hours; `/new` starts over. The answer comes when the agent is done; when it needs your go-ahead, you get the link to give it in zenith. Only the listed chats are heard; others learn their id and nothing more. zenith must be running on your Mac.

## Safety

- Only zenith's own pages (same origin, JSON) and local programs holding `.data/agent-token` (created with mode 600) can start an agent. A web page can't, even one on this Mac.
- **Outside words never get full access.** Whatever carries text zenith didn't write — Now items (emails, notes, CI), routines, requests from other agents, Telegram messages, and every request to your life agent or a bot — runs at most in zenith code's **auto** mode: the agent works on its own, but Claude's and Codex's reviewers stop risky actions (sending data out, destructive commands) that a crafted email could ask for. What you type to a project keeps your usual mode. A stricter default (*approval required*, *auto-accept edits*) always wins.
- The agent's instructions tell it that emails, pages and messages are data, never orders, and the requests zenith writes say so again.
- Every request, its destination, where it came from and its thread are logged in `.data/agent.json`, and shown in **AI agents → Activity**.

## Under the hood

zenith talks to zenith code's HTTP API with a bearer session it issues itself (`auth session issue`, renewed before it expires): `thread.create`, then `thread.turn.start`. `src/lib/agent/` holds it all: `ask.ts` (routing, model, launch), `team.ts` (the team), `workspace.ts` (the agents' folders), `skills.ts`, `now.ts`, `routines.ts`, `gateway.ts` (Telegram), `tasks.ts` (built-in requests), `target.ts` (routing rules, shared with the browser).
