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

## What the agent knows and may do

zenith writes the agent's instructions in its folder: `AGENTS.md` (read by Codex, imported by `CLAUDE.md` for Claude Code). They tell it who you are, where your brief and documents are, your projects and their folders, and the rules:

1. **Do, don't describe**, then sum up in one to three lines.
2. **Ask before anything that leaves the Mac or can't be undone**: sending an email or a message, posting, paying, buying, answering an invitation, deleting, pushing to a main branch, deploying to production. It prepares (a draft, a branch, a PR), shows the exact content, and asks.
3. No card numbers, passwords, addresses or phone numbers in files; your data never goes into zenith's (public) repository.

`MEMORY.md`, next to it, is the agent's memory: it adds what should last (preferences, people, decisions); you can edit it too. The folder also plugs in zenith's MCP server for both Claude Code (`.mcp.json`) and Codex (`.codex/config.toml`).

What it can reach: zenith's brief and documents, your Obsidian notes, the shell (`git`, `gh`…), the web, and **your Claude connectors** (Gmail, Google Calendar, Drive… whatever you connected on claude.ai). When one is missing, it says which.

## Agents asking agents

The [MCP server](../README.md#for-ai-agents) lets any agent act through zenith, not only read it:

| Tool | |
| --- | --- |
| `zenith_now` | What is waiting, with ids. |
| `zenith_delegate` | Start another agent in a project (or in `life`, or in `zenith`) with a self-contained brief. It runs in parallel and appears in the sidebar. |
| `zenith_agent` | The state and last messages of an agent started that way. |
| `zenith_done` | File a Now item away, or snooze it. |

So the life agent can split *"get my-app ready for the App Store review"* into a code task in My App and an email to Apple, and check on both. A ceiling of 12 delegations an hour keeps a loop from running away.

## Routines

Agents that run on their own, once a day at a set time, listed in **AI agents → Routines** with their last run and a **Run** button. In `zenith.config.json`:

```json
"agent": {
  "routines": [
    { "id": "morning", "at": "07:30", "task": "refresh-life" },
    { "id": "friday", "at": "18:00", "days": [5], "prompt": "Review my week: what shipped, what slipped, what to do Monday." }
  ]
}
```

`refresh-life` is built in: it captures Gmail and Google Calendar into My life, so the overview, Now and the brief are fresh when you wake up. A Mac asleep at that time catches up within three hours; each routine runs at most once a day, even with two zenith servers running. See [configuration](configuration.md#agent) for every field.

## Safety

- Only zenith's own pages (same origin, JSON) and local programs holding `.data/agent-token` (created with mode 600) can start an agent. A web page can't, even one on this Mac.
- Agents run with zenith code's permissions. For more caution, set its default runtime mode to *approval required* in zenith code's settings; the agent then asks before each command and edit.
- Every request, its destination and its thread are logged in `.data/agent.json`.

## Under the hood

zenith talks to zenith code's HTTP API with a bearer session it issues itself (`auth session issue`, renewed before it expires): `thread.create`, then `thread.turn.start`. `src/lib/agent/` holds it all: `ask.ts` (routing, model, launch), `now.ts`, `routines.ts`, `workspace.ts` (the agent's folder), `tasks.ts` (built-in requests), `target.ts` (routing rules, shared with the browser).
