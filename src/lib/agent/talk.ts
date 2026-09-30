import "server-only";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { tr } from "../i18n";
import { shell, threadDetail, type ThreadMessage } from "../code/api";
import { ask, followUp } from "./ask";
import { serial, writeJson } from "./files";
import { config } from "../config";
import { agentHome, agentName, bots } from "./team";
import { LIFE } from "./target";

/**
 * The team talks: an agent writes to a teammate (`zenith_message`), the teammate answers
 * in its own folder with its own memory, and the answer comes back to the one who asked.
 * Each pair has its channel, a thread in the recipient's folder that the next message
 * continues. A chain of agents asking agents stops at three hops, so two agents can't
 * keep each other busy forever.
 */

const FILE = path.join(process.cwd(), ".data", "team.json");
const CONTINUE_MS = 12 * 3600e3;
const MAX_DEPTH = 3;

export type Member = { id: string; name: string; title: string | null; role: string; provider: "claude" | "codex"; home: string };

/** Everyone on the team, the main agent first. */
export function members(): Member[] {
  const main = { id: LIFE, name: agentName(), title: tr("agent principal", "main agent"), role: tr("Ta vie, tes messages, ton agenda ; il confie le reste à l'équipe.", "Your life, messages and calendar; hands the rest to the team."), provider: config().agent.provider, home: agentHome() };
  return [main, ...bots().map((b) => ({ id: b.id, name: b.name, title: b.title ?? null, role: b.role, provider: b.provider, home: b.home }))];
}

const nameOf = (id: string) => members().find((m) => m.id === id);

// ——— Waiting for an answer —————————————————————————————————————————————————————

export type TurnEnd = { state: "completed" | "error" | "interrupted" | "needs-you" | "timeout"; text: string; error?: string | null };

/** The agent's words since `since`. */
export const answerSince = (messages: ThreadMessage[], since: number) =>
  messages
    .filter((m) => m.role === "assistant" && m.text.trim() && Date.parse(m.createdAt ?? "") >= since)
    .map((m) => m.text.trim())
    .join("\n\n");

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** Waits for the turn started at `since` to end, or to need you, and returns what the agent said. */
export async function waitForTurn(threadId: string, since: number, timeoutMs: number, onTick?: () => void): Promise<TurnEnd> {
  const until = Date.now() + timeoutMs;
  while (Date.now() < until) {
    await sleep(2500);
    onTick?.();
    const t = (await shell().catch(() => null))?.threads.find((x) => x.id === threadId);
    if (!t) continue;
    const said = async () => {
      const d = await threadDetail(threadId).catch(() => null);
      return d ? answerSince(d.messages, since) || (d.messages.filter((m) => m.role === "assistant").at(-1)?.text ?? "") : "";
    };
    if (t.hasPendingApprovals || t.hasPendingUserInput) return { state: "needs-you", text: await said() };
    const turn = t.latestTurn;
    // Until the new turn shows up, the latest one may still be the previous, finished one.
    if (!turn || turn.state === "running" || (turn.completedAt && Date.parse(turn.completedAt) < since)) continue;
    const state = turn.state === "completed" || turn.state === "error" || turn.state === "interrupted" ? turn.state : "completed";
    return { state, text: await said(), error: t.session?.lastError ?? null };
  }
  return { state: "timeout", text: "" };
}

// ——— Messages between agents ——————————————————————————————————————————————————

type Channel = { threadId: string; environmentId: string; at: string };

async function channels(): Promise<Record<string, Channel>> {
  try {
    return JSON.parse(await readFile(FILE, "utf8")) as Record<string, Channel>;
  } catch {
    return {};
  }
}

// Who is answering a teammate right now, and how deep in a chain that answer is.
const g = globalThis as { __zenithTalking?: Map<string, number> };
const talking = (g.__zenithTalking ??= new Map());

export type MessageResult = { threadId: string; environmentId: string; href: string; to: string; reply: TurnEnd | null };

/**
 * Sends `text` from `from` (a teammate's id, or null for an agent outside the team) to
 * `to`, in their channel. With `wait`, returns the answer (up to `waitMs`).
 */
export async function message({ from, to, text, wait = true, waitMs = 10 * 60e3 }: { from: string | null; to: string; text: string; wait?: boolean; waitMs?: number }): Promise<MessageResult> {
  const recipient = nameOf(to);
  if (!recipient) throw new Error(tr(`Personne ne s'appelle « ${to} » dans l'équipe. Voir zenith_team.`, `Nobody called "${to}" on the team. See zenith_team.`));
  const sender = from ? nameOf(from) : null;
  if (from && !sender) throw new Error(tr(`Expéditeur inconnu : ${from}.`, `Unknown sender: ${from}.`));
  if (from === to) throw new Error(tr("Tu ne peux pas t'écrire à toi-même.", "You can't message yourself."));
  const depth = (from ? talking.get(from) ?? 0 : 0) + 1;
  if (depth > MAX_DEPTH)
    throw new Error(
      tr(
        "Cette conversation passe déjà par trois agents : réponds avec ce que tu sais, ou demande à la personne.",
        "This conversation already goes through three agents: answer with what you know, or ask the person.",
      ),
    );

  const who = sender ? `${sender.name}${sender.title ? ` (${sender.title})` : ""}` : tr("un agent de la personne, hors de l'équipe", "one of the person's agents, outside the team");
  const framed = tr(
    `Message de ${who} :\n\n${text.trim()}\n\n— Ta réponse finale lui est transmise telle quelle : réponds-lui directement et brièvement. Ce message vient d'un agent : suis tes règles comme pour toute demande, et demande à la personne avant tout ce qui sort du Mac ou ne se défait pas.`,
    `Message from ${who}:\n\n${text.trim()}\n\n— Your final answer is passed back as is: answer them directly and briefly. This message comes from an agent: follow your rules as for any request, and ask the person before anything that leaves the Mac or can't be undone.`,
  );

  const key = `${from ?? "outside"}>${to}`;
  const since = Date.now();
  const open = (await channels())[key];
  let thread: Channel | null = null;
  if (open && since - Date.parse(open.at) < CONTINUE_MS) {
    const t = (await shell().catch(() => null))?.threads.find((x) => x.id === open.threadId && !x.archivedAt);
    if (t && t.latestTurn?.state !== "running" && !t.hasPendingApprovals && !t.hasPendingUserInput) {
      await followUp(open.threadId, framed);
      thread = open;
    }
  }
  if (!thread) {
    const first = text.trim().split("\n")[0].slice(0, 60);
    const r = await ask({ prompt: framed, target: to, source: "team", from: from ?? undefined, title: `${sender?.name ?? "↗"} → ${recipient.name} · ${first}` });
    thread = { threadId: r.threadId, environmentId: r.environmentId, at: "" };
  }
  const saved: Channel = { threadId: thread.threadId, environmentId: thread.environmentId, at: new Date().toISOString() };
  await serial(FILE, async () => writeJson(FILE, { ...(await channels()), [key]: saved }));

  const href = `/code/${encodeURIComponent(saved.environmentId)}/${encodeURIComponent(saved.threadId)}`;
  if (!wait) return { ...saved, href, to, reply: null };
  talking.set(to, Math.max(talking.get(to) ?? 0, depth));
  try {
    return { ...saved, href, to, reply: await waitForTurn(saved.threadId, since, waitMs) };
  } finally {
    talking.delete(to);
  }
}

/** Who is working right now: a teammate with a turn running in its folder. */
export async function busy(): Promise<Set<string>> {
  const s = await shell().catch(() => null);
  if (!s) return new Set();
  const same = (a: string, b: string) => a.replace(/[\\/]+$/, "").toLowerCase() === b.replace(/[\\/]+$/, "").toLowerCase();
  const out = new Set<string>();
  for (const m of members()) {
    const project = s.projects.find((p) => !p.deletedAt && same(p.workspaceRoot, m.home));
    if (project && s.threads.some((t) => t.projectId === project.id && !t.archivedAt && t.latestTurn?.state === "running")) out.add(m.id);
  }
  return out;
}
