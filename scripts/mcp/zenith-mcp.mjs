#!/usr/bin/env node
// "zenith" MCP server: gives any AI agent (Claude Code, Claude Desktop, Codex, Cursor…) the full
// context zenith keeps about you (projects, life, money, directory, Obsidian notes), and lets it
// act through zenith: see what is waiting (Now), hand work to other agents, file things away.
//
//   claude mcp add zenith --scope user -- node /path/to/zenith/scripts/mcp/zenith-mcp.mjs
//
// It asks the local zenith server (ZENITH_URL, default http://127.0.0.1:4747) and, when it does not
// answer, reads the files already written to zenith/context/. Reading never needs the server;
// acting does.

import { readFile, readdir } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { z } from "zod";

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const BASE = process.env.ZENITH_URL ?? "http://127.0.0.1:4747";
const home = (p) => p.replace(/^~(?=$|\/)/, os.homedir());

/** The config, found like zenith does (ZENITH_CONFIG, perso/, then the root); an empty object when missing or invalid. */
async function readConfig() {
  const files = process.env.ZENITH_CONFIG
    ? [path.resolve(process.env.ZENITH_CONFIG)]
    : [path.join(ROOT, "perso", "zenith.config.json"), path.join(ROOT, "zenith.config.json")];
  for (const file of files) {
    try {
      return JSON.parse(await readFile(file, "utf8"));
    } catch (e) {
      if (e?.code !== "ENOENT") return {};
    }
  }
  return {};
}

const CONFIG = await readConfig();
const FR = String(CONFIG.locale ?? "en").toLowerCase().startsWith("fr");
const tr = (fr, en) => (FR ? fr : en);
const PROJECTS = [...new Set((Array.isArray(CONFIG.projects) ? CONFIG.projects : []).map((p) => p?.id).filter((id) => typeof id === "string" && id))];
const NAMES = Object.fromEntries((CONFIG.projects ?? []).filter((p) => p?.id).map((p) => [p.id, p.name ?? p.id]));
const OBSIDIAN = CONFIG.obsidian ?? {};
const EXPORT_DIR = OBSIDIAN.exportDir ?? "zenith";
const DOCS = ["brief", "vie", "argent", "annuaire", "veille", ...PROJECTS.map((p) => `projets/${p}`)];

async function doc(name) {
  try {
    const res = await fetch(`${BASE}/api/context/${name}`, { signal: AbortSignal.timeout(60_000) });
    if (res.ok) return await res.text();
  } catch {}
  try {
    const body = await readFile(path.join(ROOT, "context", `${name}.md`), "utf8");
    return `${body}\n\n${tr("_(copie écrite sur disque : le serveur zenith ne répond pas en ce moment)_", "_(copy from disk: the zenith server is not answering right now)_")}`;
  } catch {
    return tr(`Document « ${name} » inaccessible : lance zenith (npm run dev, ou npm run mac:install) puis réessaie.`, `Document "${name}" unavailable: start zenith (npm run dev, or npm run mac:install) and try again.`);
  }
}

/** Obsidian vault: OBSIDIAN_VAULT, else obsidian.vault in the config, else the vault open in Obsidian, else obsidian.fallback. */
async function vaultRoot() {
  if (process.env.OBSIDIAN_VAULT) return path.resolve(home(process.env.OBSIDIAN_VAULT));
  if (OBSIDIAN.vault) return path.resolve(home(OBSIDIAN.vault));
  try {
    const cfg = JSON.parse(await readFile(path.join(os.homedir(), "Library/Application Support/obsidian/obsidian.json"), "utf8"));
    const open = Object.values(cfg.vaults ?? {}).find((v) => v.open);
    if (open) return path.resolve(open.path);
  } catch {}
  if (OBSIDIAN.fallback) return path.resolve(home(OBSIDIAN.fallback));
  return null;
}

const NO_VAULT = tr(
  "Aucun vault Obsidian trouvé : ouvre-en un dans Obsidian, ou renseigne obsidian.vault dans zenith.config.json (ou OBSIDIAN_VAULT).",
  "No Obsidian vault found: open one in Obsidian, or set obsidian.vault in zenith.config.json (or OBSIDIAN_VAULT).",
);

async function markdownFiles(dir, root = dir, out = []) {
  for (const e of await readdir(dir, { withFileTypes: true }).catch(() => [])) {
    if (e.name.startsWith(".")) continue;
    const p = path.join(dir, e.name);
    // The export folder is zenith's own summary, not one of your notes.
    if (e.isDirectory() && path.relative(root, p) === EXPORT_DIR) continue;
    if (e.isDirectory()) await markdownFiles(p, root, out);
    else if (e.name.endsWith(".md")) out.push(path.relative(root, p));
  }
  return out;
}

const text = (t) => ({ content: [{ type: "text", text: t }] });
const server = new McpServer({ name: "zenith", version: "1.2.0" });
const projectList = PROJECTS.length ? PROJECTS.map((id) => (NAMES[id] && NAMES[id] !== id ? `${id} (${NAMES[id]})` : id)).join(", ") : "none configured";

server.registerTool(
  "zenith_brief",
  {
    title: "zenith brief",
    description:
      `Call this first. Up-to-date one-page summary of the user: urgent items, status and key numbers of each project (${projectList}), calendar, birthdays, to-dos, emails waiting for a reply, mentions, money, running AI agents.`,
    annotations: { readOnlyHint: true },
  },
  async () => text(await doc("brief")),
);

server.registerTool(
  "zenith_project",
  {
    title: "Project details",
    description:
      "Detailed state of one project: numbers, to-dos, identity (domains, socials, stores), costs, latest commits, open PRs and issues, CI, deployments, agent sessions, related Obsidian notes.",
    inputSchema: {
      project: (PROJECTS.length ? z.enum(PROJECTS) : z.string()).describe(`Project id: ${projectList}`),
    },
    annotations: { readOnlyHint: true },
  },
  async ({ project }) => text(PROJECTS.includes(project) ? await doc(`projets/${project}`) : tr(`Projet inconnu : ${project}. Projets : ${projectList}.`, `Unknown project: ${project}. Projects: ${projectList}.`)),
);

server.registerTool(
  "zenith_document",
  {
    title: "zenith document",
    description:
      "vie (life): Google and Apple calendars, to-dos (Obsidian, Reminders), emails, sales, paperwork, spending, rhythm, air, pollen and water, public holidays, birthdays, transit, screen time, music, notebook. argent (money): income and subscriptions. annuaire (directory): names, domains, emails, socials, services. veille (watch): who mentions the projects (Hacker News, GitHub), GitHub notifications and stars, news, markets, state of this Mac.",
    inputSchema: { name: z.enum(["vie", "argent", "annuaire", "veille"]).describe("vie (life), argent (money), annuaire (directory) or veille (watch)") },
    annotations: { readOnlyHint: true },
  },
  async ({ name }) => text(await doc(name)),
);

server.registerTool(
  "zenith_search_notes",
  {
    title: "Search the Obsidian vault",
    description: "Full-text search in the user's Obsidian notes. Returns the notes containing the query, with an excerpt.",
    inputSchema: { query: z.string().min(2).describe("words to look for, case-insensitive") },
    annotations: { readOnlyHint: true },
  },
  async ({ query }) => {
    const root = await vaultRoot();
    if (!root) return text(NO_VAULT);
    const needle = query.toLowerCase();
    const hits = [];
    for (const rel of await markdownFiles(root)) {
      const body = await readFile(path.join(root, rel), "utf8").catch(() => "");
      const i = body.toLowerCase().indexOf(needle);
      if (i >= 0 || rel.toLowerCase().includes(needle)) hits.push(`## ${rel}\n\n…${body.slice(Math.max(0, i - 200), i + 400).trim()}…`);
      if (hits.length >= 10) break;
    }
    return text(hits.length ? hits.join("\n\n") : tr(`Aucune note ne contient « ${query} ».`, `No note contains "${query}".`));
  },
);

server.registerTool(
  "zenith_read_note",
  {
    title: "Read an Obsidian note",
    description: 'Full content of a vault note, by relative path (e.g. "Todo.md", "Projects/My App.md").',
    inputSchema: { path: z.string().describe("path relative to the vault") },
    annotations: { readOnlyHint: true },
  },
  async ({ path: rel }) => {
    const root = await vaultRoot();
    if (!root) return text(NO_VAULT);
    const full = path.resolve(root, rel.endsWith(".md") ? rel : `${rel}.md`);
    if (!full.startsWith(root + path.sep)) return text(tr("Chemin refusé : hors du vault.", "Path refused: outside the vault."));
    return text(await readFile(full, "utf8").catch(() => tr(`Note introuvable : ${rel}`, `Note not found: ${rel}`)));
  },
);

// ——— Acting: Now, delegation to other agents ———————————————————————————————
// These go through the running zenith server, authenticated by the token it keeps in
// .data/agent-token (readable by local programs only, never by a web page).

const NOT_RUNNING = tr(
  "zenith ne répond pas : lance-le (npm run dev, ou l'app zenith) pour agir.",
  "zenith is not answering: start it (npm run dev, or the zenith app) to act.",
);

async function act(pathname, body, timeoutMs = 60_000) {
  const token = await readFile(path.join(ROOT, ".data", "agent-token"), "utf8").catch(() => "");
  const res = await fetch(`${BASE}${pathname}`, {
    method: body === undefined ? "GET" : "POST",
    headers: { "x-zenith-token": token.trim(), ...(body === undefined ? {} : { "content-type": "application/json" }) },
    body: body === undefined ? undefined : JSON.stringify(body),
    signal: AbortSignal.timeout(timeoutMs),
  });
  const data = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(data.error ?? `HTTP ${res.status}`);
  return data;
}

const attempt = async (fn) => {
  try {
    return text(await fn());
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    if (/timeout|abort/i.test(msg))
      return text(tr("zenith n'a pas répondu à temps : la demande a peut-être abouti. Vérifie avec zenith_now avant de réessayer.", "zenith didn't answer in time: the request may have gone through. Check with zenith_now before retrying."));
    return text(/fetch failed|ECONNREFUSED/i.test(msg) ? NOT_RUNNING : `${tr("Échec", "Failed")}: ${msg}`);
  }
};

// Only projects with a folder can host an agent; bots of the team have their own.
const BOTS = (Array.isArray(CONFIG.agent?.bots) ? CONFIG.agent.bots : []).filter(
  (b) => typeof b?.id === "string" && b.enabled !== false && !["life", "zenith", "all", ...PROJECTS].includes(b.id),
);
const TARGETS = ["life", ...BOTS.map((b) => b.id), ...PROJECTS.filter((id) => (CONFIG.projects ?? []).some((p) => p?.id === id && p.dir)), "zenith"];
const team = BOTS.length
  ? ` The user's team of bots (each with its own memory, on the user's Claude or ChatGPT/Codex subscription): ${BOTS.map((b) => `"${b.id}" (${b.name ?? b.id}${b.title ? `, ${b.title}` : ""}, ${b.provider ?? CONFIG.agent?.provider ?? "claude"}: ${String(b.role ?? "").replace(/\s+/g, " ").slice(0, 140)})`).join("; ")}.`
  : "";

server.registerTool(
  "zenith_now",
  {
    title: "What is waiting now",
    description:
      "What is waiting for the user right now, most pressing first: sites down, failing payments, birthdays, emails to answer, paperwork, broken CI, stale email/calendar capture. Each item has an id (for zenith_done), where an agent would work on it, and whether an agent is already on it.",
    annotations: { readOnlyHint: true },
  },
  () =>
    attempt(async () => {
      const items = await act("/api/now");
      if (!items.length) return tr("Rien n'attend. Ciel dégagé.", "Nothing is waiting. Clear skies.");
      return items
        .map((i) => `- [${i.id}] ${i.title} — ${i.detail}${i.target !== "life" ? ` (→ ${i.target})` : ""}${i.delegated ? tr(` · un agent s'en occupe (thread ${i.delegated.threadId})`, ` · an agent is on it (thread ${i.delegated.threadId})`) : ""}${i.href ? ` · ${i.href}` : ""}`)
        .join("\n");
    }),
);

server.registerTool(
  "zenith_delegate",
  {
    title: "Hand work to an agent",
    description:
      `Starts another AI agent (Claude Code or Codex, in zenith code) that works in parallel: in a project's folder for code (${projectList}), in "zenith" for the dashboard itself, a bot of the team by its id for what fits its role, or in "life" for anything else.${team} It does not see your conversation: give it a complete, self-contained brief. Returns its thread id (follow up with zenith_agent) and a link the user can open.`,
    inputSchema: {
      prompt: z.string().min(8).describe("the complete brief for the agent"),
      project: z.enum(TARGETS).optional().describe(`where it works: ${TARGETS.join(", ")}; default: guessed from the brief`),
      provider: z.enum(["claude", "codex"]).optional().describe("Claude Code or Codex; default: the bot's, else the user's choice"),
      now_id: z.string().optional().describe("the zenith_now item this handles, if any"),
    },
  },
  ({ prompt, project, provider, now_id }) =>
    attempt(async () => {
      const r = await act("/api/agent", { prompt, target: project, provider, nowId: now_id });
      return tr(
        `Agent lancé dans « ${r.target} » (thread ${r.threadId}). Suivi : zenith_agent. Lien : ${BASE}${r.href}`,
        `Agent started in "${r.target}" (thread ${r.threadId}). Follow up: zenith_agent. Link: ${BASE}${r.href}`,
      );
    }),
);

server.registerTool(
  "zenith_agent",
  {
    title: "Check on an agent",
    description: "State of an agent started with zenith_delegate (working, completed, waiting for approval) and its latest messages.",
    inputSchema: { thread_id: z.string().describe("thread id returned by zenith_delegate") },
    annotations: { readOnlyHint: true },
  },
  ({ thread_id }) =>
    attempt(async () => {
      const t = await act(`/api/agent/thread/${encodeURIComponent(thread_id)}`);
      return [`# ${t.title}`, `turn: ${t.turn ?? "—"} · session: ${t.session ?? "—"}`, "", ...t.messages.map((m) => `## ${m.role}\n\n${m.text}`)].join("\n");
    }),
);

server.registerTool(
  "zenith_done",
  {
    title: "File a Now item away",
    description: "Marks a zenith_now item as handled (it leaves the list), or snoozes it for some hours.",
    inputSchema: {
      id: z.string().describe("item id from zenith_now"),
      snooze_hours: z.number().min(1).max(720).optional().describe("snooze instead of done"),
    },
  },
  ({ id, snooze_hours }) =>
    attempt(async () => {
      await act("/api/now", snooze_hours ? { id, action: "snooze", hours: snooze_hours } : { id, action: "done" });
      return snooze_hours ? tr(`Reporté de ${snooze_hours} h.`, `Snoozed for ${snooze_hours} h.`) : tr("Classé.", "Filed away.");
    }),
);

// ——— The team talks ————————————————————————————————————————————————————————————
// zenith writes ZENITH_AGENT (who this agent is) into each team folder's MCP config.

const ME = process.env.ZENITH_AGENT || null;

server.registerTool(
  "zenith_team",
  {
    title: "Who is on the team",
    description: "The user's team of agents: id, first name, job, what each is for, which subscription it runs on (Claude or Codex), and who is working right now. Use it before zenith_message.",
    annotations: { readOnlyHint: true },
  },
  () =>
    attempt(async () => {
      const list = await act("/api/agent/team");
      return list
        .map((m) => `- ${m.id}${m.id === ME ? tr(" (toi)", " (you)") : ""}: ${m.name}${m.title ? `, ${m.title}` : ""} · ${m.provider === "codex" ? "Codex" : "Claude"}${m.busy ? tr(" · occupé", " · busy") : ""} — ${m.role}`)
        .join("\n");
    }),
);

server.registerTool(
  "zenith_message",
  {
    title: "Talk to a teammate",
    description:
      "Send a message to another agent of the team (see zenith_team) and get their answer: ask a question, share a finding, or hand over something that fits their role. They answer from their own folder, memory and tools (e.g. a Claude agent can read Gmail when a Codex one can't). The same pair keeps one conversation, so follow-ups have context. Waits for the answer (up to 10 minutes) unless wait is false.",
    inputSchema: {
      to: z.string().describe("teammate id, e.g. \"life\" for the main agent or a bot id"),
      message: z.string().min(2).describe("what you want to tell or ask them; self-contained"),
      wait: z.boolean().optional().describe("wait for their answer (default true)"),
    },
  },
  ({ to, message, wait }) =>
    attempt(async () => {
      const r = await act("/api/agent/message", { from: ME, to, text: message, wait: wait !== false }, 11 * 60_000);
      if (!r.reply) return tr(`Message envoyé à « ${to} » (thread ${r.threadId}).`, `Message sent to "${to}" (thread ${r.threadId}).`);
      const { state, text: said } = r.reply;
      if (state === "completed") return said || tr("(réponse vide)", "(empty answer)");
      if (state === "needs-you") return `${said}\n\n${tr("⏸ Il attend l'accord de la personne pour continuer.", "⏸ They are waiting for the person's go-ahead.")} ${BASE}${r.href}`;
      if (state === "timeout") return tr(`Pas encore de réponse après 10 minutes ; suis-le avec zenith_agent (thread ${r.threadId}).`, `No answer after 10 minutes yet; follow up with zenith_agent (thread ${r.threadId}).`);
      return `${said}\n\n${tr("⚠︎ Il s'est arrêté en route.", "⚠︎ They stopped on the way.")}`;
    }),
);

for (const name of DOCS) {
  server.registerResource(
    name.replace("/", "-"),
    `zenith://${name}`,
    { title: `zenith · ${name}`, description: `${name}.md, up to date`, mimeType: "text/markdown" },
    async (uri) => ({ contents: [{ uri: uri.href, mimeType: "text/markdown", text: await doc(name) }] }),
  );
}

await server.connect(new StdioServerTransport());
