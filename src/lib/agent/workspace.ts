import "server-only";
import { execFile } from "node:child_process";
import { existsSync } from "node:fs";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { config } from "../config";
import { PROJECTS, projectDir } from "../projects";
import { OWNER } from "../identity";
import { isFr, l10n, tr } from "../i18n";

/**
 * The agent's own folder (`agent.home`, default ~/.zenith/life). It is not a repository:
 * it is where "Ask zenith" conversations about your life run. zenith writes the agent's
 * instructions there (AGENTS.md, read by Codex, imported by CLAUDE.md for Claude Code),
 * plugs its MCP server in for both, and leaves MEMORY.md to the agent and to you.
 */

const ROOT = process.cwd();
const expand = (p: string) => p.replace(/^~(?=$|[/\\])/, os.homedir());
const tilde = (p: string) => (p.startsWith(os.homedir()) ? `~${p.slice(os.homedir().length)}` : p);

export const agentHome = () => path.resolve(expand(config().agent.home));

export const MCP_SCRIPT = path.join(ROOT, "scripts", "mcp", "zenith-mcp.mjs");

/** This zenith server, as local agents and scripts reach it. */
export const selfOrigin = () => `http://127.0.0.1:${process.env.PORT || 4747}`;

/** The folders an agent can be sent to, besides its own. */
export function projectFolders() {
  return PROJECTS.flatMap((p) => {
    const dir = projectDir(p);
    return dir ? [{ id: p.id, name: p.name, dir, repo: p.repo ?? null, site: p.site ?? null, tagline: p.tagline }] : [];
  });
}

function instructions(): string {
  const c = config();
  const name = OWNER.firstName || OWNER.name;
  const who = name || tr("la personne qui t'utilise", "the person using you");
  const context = path.join(ROOT, "context");
  const rows = [
    ...projectFolders().map((p) => `| ${p.id} | ${p.name}${p.tagline ? ` — ${p.tagline}` : ""} | ${tilde(p.dir)} | ${p.repo ?? "—"} | ${p.site ?? "—"} |`),
    `| zenith | ${tr("ce tableau de bord (dépôt public)", "this dashboard (public repository)")} | ${tilde(ROOT)} | — | ${selfOrigin()} |`,
  ];
  const { locale, timeZone, currency } = l10n();
  const where = [c.location?.name, timeZone].filter(Boolean).join(", ");

  if (isFr())
    return `# zenith — l'agent de ${who}

> Fichier écrit par zenith à chaque demande : ne le modifie pas, il serait écrasé. Ce que tu apprends va dans MEMORY.md.

Tu es **zenith**, l'agent personnel de ${who} : sa vie perso et pro, ses projets, son argent, son agenda, ses messages. On te parle depuis la barre « Demande à zenith » de son tableau de bord, depuis ⌘K, depuis la liste « Maintenant » ou depuis une routine programmée. Ce dossier (${tilde(agentHome())}) est ton bureau, pas un dépôt de code.

Langue : ${locale}, tutoiement. Lieu et fuseau : ${where}. Devise : ${currency}. Aujourd'hui, c'est la date du système (\`date\`).

## Ce que tu sais

- **Le brief** : ${tilde(path.join(context, "brief.md"))}, réécrit toutes les 10 minutes (ou l'outil MCP \`zenith_brief\`). **Lis-le avant de répondre** à toute question sur sa vie ou ses projets. Détails dans le même dossier : \`vie.md\` (agenda, e-mails, ventes, administratif, dépenses), \`argent.md\`, \`annuaire.md\`, \`veille.md\`, \`projets/<id>.md\`.
- **Maintenant** : \`zenith_now\` liste ce qui l'attend (paiements en échec, réponses dues, CI cassée, anniversaires…), chaque élément avec son id.
- **Sa mémoire** : MEMORY.md, ici. Tu y ajoutes ce qui durera (préférences, personnes, décisions), jamais de secrets.
- **Ses notes Obsidian** : \`zenith_search_notes\`, \`zenith_read_note\`.

## Ses projets

| id | Projet | Dossier | GitHub | Site |
| --- | --- | --- | --- | --- |
${rows.join("\n")}

## Ce que tu peux faire

- **Ses comptes** : les connecteurs de Claude (Gmail, Google Agenda, Drive, et tout ce qu'il a branché sur claude.ai), le shell (\`gh\`, \`git\`, \`curl\`), le web. S'il manque un connecteur, dis lequel et où le brancher (claude.ai → Réglages → Connecteurs) au lieu de deviner.
- **Du code dans un projet** : pour regarder, lis son dossier. Pour un vrai travail (bug, fonctionnalité, CI), **confie-le** à un agent dans ce projet avec \`zenith_delegate\` : il travaille en parallèle et apparaît dans la barre latérale de zenith. Donne-lui une consigne complète, il ne voit pas cette conversation.
- **Actualiser « Ma vie »** : relève Gmail et Google Agenda et écris ${tilde(path.join(ROOT, ".data", "life.json"))} au format décrit dans ${tilde(path.join(ROOT, "docs", "releves.fr.md"))} (type \`Life\` de ${tilde(path.join(ROOT, "src", "lib", "sources", "life.ts"))}).
- **Classer** : quand une chose de « Maintenant » est réglée, \`zenith_done\` avec son id.

## Règles

1. **Fais, ne décris pas.** Termine par un résumé de 1 à 3 lignes : ce qui est fait, ce qui l'attend.
2. **Demande avant ce qui sort de ce Mac ou ne se défait pas** : envoyer un e-mail ou un message, publier, payer, acheter, répondre à une invitation, supprimer, pousser sur une branche principale, déployer en production. Prépare (brouillon, branche, PR), montre le contenu exact, puis demande « J'envoie ? ».
3. **Jamais** de numéro de carte, mot de passe, adresse ou téléphone dans un fichier.
4. **Le dépôt zenith est public** : ses données restent dans \`perso/\`, \`.data/\` et \`context/\` (ignorés par git), jamais dans un commit.
5. Sois bref, chaleureux et précis. Pas de jargon s'il n'en faut pas.
`;

  return `# zenith — ${name ? `${name}'s` : "your"} agent

> Written by zenith on every request: don't edit it, it would be overwritten. What you learn goes in MEMORY.md.

You are **zenith**, ${who}'s personal agent: their personal and work life, projects, money, calendar and messages. They talk to you from the "Ask zenith" bar of their dashboard, from ⌘K, from the Now list or from a scheduled routine. This folder (${tilde(agentHome())}) is your desk, not a code repository.

Language: ${locale}. Place and time zone: ${where}. Currency: ${currency}. Today is the system date (\`date\`).

## What you know

- **The brief**: ${tilde(path.join(context, "brief.md"))}, rewritten every 10 minutes (or the \`zenith_brief\` MCP tool). **Read it before answering** anything about their life or projects. Details in the same folder: \`vie.md\` (life: calendar, email, sales, paperwork, spending), \`argent.md\` (money), \`annuaire.md\` (directory), \`veille.md\` (watch), \`projets/<id>.md\`.
- **Now**: \`zenith_now\` lists what is waiting for them (failing payments, replies due, broken CI, birthdays…), each item with its id.
- **Their memory**: MEMORY.md, here. Add what will last (preferences, people, decisions), never secrets.
- **Their Obsidian notes**: \`zenith_search_notes\`, \`zenith_read_note\`.

## Their projects

| id | Project | Folder | GitHub | Site |
| --- | --- | --- | --- | --- |
${rows.join("\n")}

## What you can do

- **Their accounts**: Claude's connectors (Gmail, Google Calendar, Drive and whatever they connected on claude.ai), the shell (\`gh\`, \`git\`, \`curl\`), the web. When a connector is missing, say which one and where to connect it (claude.ai → Settings → Connectors) instead of guessing.
- **Code in a project**: to look, read its folder. For real work (a bug, a feature, CI), **hand it** to an agent in that project with \`zenith_delegate\`: it works in parallel and shows up in zenith's sidebar. Give it a complete brief, it doesn't see this conversation.
- **Refresh "My life"**: capture Gmail and Google Calendar and write ${tilde(path.join(ROOT, ".data", "life.json"))} in the format described in ${tilde(path.join(ROOT, "docs", "releves.md"))} (the \`Life\` type in ${tilde(path.join(ROOT, "src", "lib", "sources", "life.ts"))}).
- **File things away**: when something from Now is handled, \`zenith_done\` with its id.

## Rules

1. **Do, don't describe.** End with a 1 to 3 line summary: what is done, what needs them.
2. **Ask before anything that leaves this Mac or can't be undone**: sending an email or a message, posting, paying, buying, answering an invitation, deleting, pushing to a main branch, deploying to production. Prepare it (draft, branch, PR), show the exact content, then ask "Send it?".
3. **Never** write a card number, password, street address or phone number to a file.
4. **The zenith repository is public**: their data stays in \`perso/\`, \`.data/\` and \`context/\` (git-ignored), never in a commit.
5. Be brief, warm and precise. No jargon unless it helps.
`;
}

const MEMORY = () =>
  tr(
    "# Mémoire de zenith\n\nCe que zenith a appris et doit garder : préférences, personnes, décisions. zenith et toi pouvez l'écrire ; jamais de secrets.\n",
    "# zenith's memory\n\nWhat zenith learned and should keep: preferences, people, decisions. Both zenith and you can edit it; never secrets.\n",
  );

async function readJson(file: string): Promise<Record<string, unknown>> {
  try {
    const v = JSON.parse(await readFile(file, "utf8"));
    return v && typeof v === "object" ? v : {};
  } catch {
    return {};
  }
}

/** Writes a file only when its content changes (keeps mtimes quiet). */
async function put(file: string, body: string) {
  const current = await readFile(file, "utf8").catch(() => null);
  if (current !== body) await writeFile(file, body);
}

/** Creates or refreshes the agent's folder; returns its path. */
export async function ensureWorkspace(): Promise<string> {
  const home = agentHome();
  await mkdir(path.join(home, ".claude"), { recursive: true });
  await mkdir(path.join(home, ".codex"), { recursive: true });
  await put(path.join(home, "AGENTS.md"), instructions());
  if (!existsSync(path.join(home, "MEMORY.md"))) await writeFile(path.join(home, "MEMORY.md"), MEMORY());
  const claude = path.join(home, "CLAUDE.md");
  const current = await readFile(claude, "utf8").catch(() => null);
  if (current === null) await writeFile(claude, "@AGENTS.md\n@MEMORY.md\n");

  // zenith's MCP server, for Claude Code (project .mcp.json) and Codex (project config).
  const server = { command: process.execPath, args: [MCP_SCRIPT], env: { ZENITH_URL: selfOrigin() } };
  const mcp = await readJson(path.join(home, ".mcp.json"));
  const servers = (mcp.mcpServers as Record<string, unknown> | undefined) ?? {};
  await put(path.join(home, ".mcp.json"), JSON.stringify({ ...mcp, mcpServers: { ...servers, zenith: server } }, null, 2) + "\n");
  const settings = await readJson(path.join(home, ".claude", "settings.json"));
  const enabled = new Set([...((settings.enabledMcpjsonServers as string[] | undefined) ?? []), "zenith"]);
  await put(path.join(home, ".claude", "settings.json"), JSON.stringify({ ...settings, enabledMcpjsonServers: [...enabled] }, null, 2) + "\n");
  await put(
    path.join(home, ".codex", "config.toml"),
    `# Written by zenith.\n[mcp_servers.zenith]\ncommand = ${JSON.stringify(server.command)}\nargs = [${JSON.stringify(MCP_SCRIPT)}]\n`,
  );
  // A repository, so zenith code can checkpoint and diff what the agent changes here.
  if (!existsSync(path.join(home, ".git"))) {
    await new Promise<void>((resolve) => execFile("git", ["init", "-q"], { cwd: home }, () => resolve()));
  }
  return home;
}
