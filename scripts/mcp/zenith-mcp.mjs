#!/usr/bin/env node
// "zenith" MCP server: gives any AI agent (Claude Code, Claude Desktop, Codex, Cursor…) the full
// context zenith keeps about you (projects, life, money, directory, Obsidian notes), read-only.
//
//   claude mcp add zenith --scope user -- node /path/to/zenith/scripts/mcp/zenith-mcp.mjs
//
// It asks the local zenith server (ZENITH_URL, default http://127.0.0.1:4747) and, when it does not
// answer, reads the files already written to zenith/context/. It never needs the server to start.

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
const server = new McpServer({ name: "zenith", version: "1.1.0" });
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

for (const name of DOCS) {
  server.registerResource(
    name.replace("/", "-"),
    `zenith://${name}`,
    { title: `zenith · ${name}`, description: `${name}.md, up to date`, mimeType: "text/markdown" },
    async (uri) => ({ contents: [{ uri: uri.href, mimeType: "text/markdown", text: await doc(name) }] }),
  );
}

await server.connect(new StdioServerTransport());
