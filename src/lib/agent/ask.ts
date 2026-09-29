import "server-only";
import { randomUUID } from "node:crypto";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { config } from "../config";
import { PROJECTS, projectDir } from "../projects";
import { tr } from "../i18n";
import { addProject } from "../code/cli";
import { codeStatus } from "../code/manager";
import { codeSettings, dispatch, environmentId, shell, type ModelSelection, type Shell } from "../code/api";
import { LIFE, aliasesOf, guessTarget, type AgentTarget, type Provider } from "./target";
import { ensureWorkspace } from "./workspace";
import { serial, writeJson } from "./files";

/**
 * "Ask zenith": one sentence becomes an agent at work. The request goes to the project it
 * names or to the agent's own folder (your life), runs in zenith code with your usual
 * model and permissions, and opens as a thread you can follow, answer and approve.
 */

export type AskSource = "bar" | "command" | "now" | "routine" | "mcp";

export type AskInput = {
  prompt: string;
  /** A target id; default: guessed from the prompt. */
  target?: string;
  provider?: Provider;
  source?: AskSource;
  /** The Now item this request handles, if any. */
  nowId?: string;
  /** Thread title; default: the prompt's first line. */
  title?: string;
};

export type AskResult = { environmentId: string; threadId: string; href: string; target: string };

const ROOT = process.cwd();
const LOG = path.join(ROOT, ".data", "agent.json");
const INSTANCE: Record<Provider, string> = { claude: "claudeAgent", codex: "codex" };
// zenith code's own defaults (code/packages/contracts/src/model.ts), when nothing else says.
const FALLBACK_MODEL: Record<Provider, string> = { claude: "claude-fable-5-1", codex: "gpt-6-astra" };

/** Everywhere a request can go: your life first, your projects, then zenith itself. */
export function targets(): AgentTarget[] {
  const c = config();
  return [
    { id: LIFE, name: tr("Ma vie", "My life"), glow: "#FFD166", emoji: "✦", aliases: [] },
    ...PROJECTS.filter((p) => projectDir(p)).map((p) => ({
      id: p.id,
      name: p.name,
      glow: p.glow,
      emoji: p.emoji,
      aliases: aliasesOf(
        p.id,
        p.name,
        p.dir && path.basename(p.dir),
        p.repo?.split("/")[1],
        ...(p.identity?.names ?? []).map((n) => n.value),
      ),
    })),
    ...(c.code.enabled ? [{ id: "zenith", name: "zenith", glow: "#B18CFF", emoji: "☀︎", aliases: [] }] : []),
  ];
}

function folderOf(target: string): { dir: string; title: string } | null {
  if (target === "zenith") return { dir: ROOT, title: "zenith" };
  const p = PROJECTS.find((x) => x.id === target);
  const dir = p ? projectDir(p) : null;
  return p && dir ? { dir, title: p.name } : null;
}

const same = (a: string, b: string) => a.replace(/[\\/]+$/, "").toLowerCase() === b.replace(/[\\/]+$/, "").toLowerCase();

async function codeProject(dir: string, title: string): Promise<{ id: string; shell: Shell }> {
  let s = await shell();
  let hit = s.projects.find((p) => !p.deletedAt && same(p.workspaceRoot, dir));
  if (!hit) {
    await addProject(dir, title);
    s = await shell();
    hit = s.projects.find((p) => !p.deletedAt && same(p.workspaceRoot, dir));
  }
  if (!hit) throw new Error(tr(`zenith code n'a pas pu ouvrir ${dir}.`, `zenith code could not open ${dir}.`));
  return { id: hit.id, shell: s };
}

/** Your model: the config's, else zenith code's default, else your latest thread's, else zenith code's fallback. */
async function modelFor(provider: Provider, s: Shell): Promise<{ provider: Provider; selection: ModelSelection }> {
  const settings = await codeSettings();
  const enabled = (p: Provider) => settings.providerInstances?.[INSTANCE[p]]?.enabled !== false;
  const chosen: Provider = enabled(provider) ? provider : provider === "claude" ? "codex" : "claude";
  const instanceId = INSTANCE[chosen];
  const configured = config().agent.model;
  if (configured && chosen === config().agent.provider) return { provider: chosen, selection: { instanceId, model: configured } };
  if (settings.defaultModelSelection?.instanceId === instanceId) return { provider: chosen, selection: settings.defaultModelSelection };
  const latest = s.threads
    .filter((t) => t.modelSelection?.instanceId === instanceId)
    .sort((a, b) => b.updatedAt.localeCompare(a.updatedAt))[0];
  if (latest?.modelSelection) return { provider: chosen, selection: latest.modelSelection };
  return { provider: chosen, selection: { instanceId, model: FALLBACK_MODEL[chosen] } };
}

// From the most careful to the most free (zenith code's runtime modes).
const MODES = ["approval-required", "auto-accept-edits", "auto", "full-access"];

/**
 * Requests that carry outside words (emails, web pages, CI logs, another agent's brief)
 * run at most in "auto": the agent works freely, but Claude's and Codex's reviewers stop
 * risky actions (exfiltration, destructive commands) a crafted message could ask for.
 * Your own words to a project keep your usual mode.
 */
function runtimeFor(usual: string, untrusted: boolean): string {
  if (!untrusted) return usual;
  const i = MODES.indexOf(usual);
  return i >= 0 && i < MODES.indexOf("auto") ? usual : "auto";
}

export type AskLogEntry = { at: string; target: string; threadId: string; environmentId: string; source: AskSource; nowId?: string; title: string };

export async function askLog(): Promise<AskLogEntry[]> {
  try {
    return JSON.parse(await readFile(LOG, "utf8")) as AskLogEntry[];
  } catch {
    return [];
  }
}

const remember = (entry: AskLogEntry) =>
  serial(LOG, async () => {
    const list = [entry, ...(await askLog())].slice(0, 200);
    await writeJson(LOG, list);
  });

const titleOf = (prompt: string) => {
  const line = prompt.split("\n").find((l) => l.trim())?.trim() ?? prompt.trim();
  return line.length > 72 ? `${line.slice(0, 70).trimEnd()}…` : line;
};

export async function ask(input: AskInput): Promise<AskResult> {
  const c = config();
  if (!c.agent.enabled) throw new Error(tr("L'agent zenith est désactivé (agent.enabled).", "The zenith agent is disabled (agent.enabled)."));
  if (!c.code.enabled || !codeStatus().running)
    throw new Error(tr("zenith code ne tourne pas : c'est lui qui fait travailler les agents.", "zenith code isn't running: it is what runs the agents."));

  let text = input.prompt.trim();
  if (!text) throw new Error(tr("Demande vide.", "Empty request."));
  const all = targets();
  if (input.target && !all.some((t) => t.id === input.target)) throw new Error(tr(`Destination inconnue : ${input.target}.`, `Unknown destination: ${input.target}.`));
  let target = input.target ?? guessTarget(text, all);
  // "@my-app fix the login": the mention chose the target, the rest is the request.
  const forced = /^\s*@([\w-]+)\s+/.exec(text);
  const hit = forced && all.find((t) => t.id === forced[1].toLowerCase() || t.aliases.includes(forced[1].toLowerCase()));
  if (forced && hit) {
    if (!input.target) target = hit.id;
    text = text.slice(forced[0].length).trim();
  }

  const folder = target === LIFE ? { dir: await ensureWorkspace(), title: tr("Ma vie", "My life") } : folderOf(target);
  if (!folder) throw new Error(tr(`Destination inconnue : ${target}.`, `Unknown destination: ${target}.`));
  const { id: projectId, shell: s } = await codeProject(folder.dir, folder.title);
  const { selection } = await modelFor(input.provider ?? c.agent.provider, s);
  const settings = await codeSettings();
  const runtimeMode = runtimeFor(
    settings.projectSettingsOverrides?.[projectId]?.defaultRuntimeMode ?? settings.defaultRuntimeMode ?? "full-access",
    target === LIFE || (input.source !== undefined && input.source !== "bar" && input.source !== "command"),
  );
  const title = input.title?.trim() || titleOf(text);
  const threadId = randomUUID();
  const createdAt = new Date().toISOString();

  await dispatch({
    type: "thread.create",
    threadId,
    projectId,
    title,
    modelSelection: selection,
    runtimeMode,
    interactionMode: "default",
    branch: null,
    worktreePath: null,
    createdAt,
  });
  try {
    await dispatch({
      type: "thread.turn.start",
      threadId,
      message: { messageId: randomUUID(), role: "user", text, attachments: [] },
      modelSelection: selection,
      titleSeed: title,
      runtimeMode,
      interactionMode: "default",
      createdAt,
    });
  } catch (e) {
    await dispatch({ type: "thread.delete", threadId }).catch(() => {});
    throw e;
  }

  const env = await environmentId();
  await remember({ at: createdAt, target, threadId, environmentId: env, source: input.source ?? "bar", nowId: input.nowId, title });
  return { environmentId: env, threadId, target, href: `/code/${encodeURIComponent(env)}/${encodeURIComponent(threadId)}` };
}
