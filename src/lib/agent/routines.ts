import "server-only";
import { mkdir, open, readdir, readFile, unlink } from "node:fs/promises";
import path from "node:path";
import { config, type RoutineConfig } from "../config";
import { l10n, tr } from "../i18n";
import { codeStatus } from "../code/manager";
import { ask } from "./ask";
import { underLimit } from "./auth";
import { writeJson } from "./files";
import { now, type NowItem, type NowKind } from "./now";
import { skillsDir } from "./skills";
import { LIFE } from "./target";
import { TASKS } from "./tasks";

/**
 * Routines: requests that run on their own (`agent.routines`), by the main agent or a bot.
 *
 * - At a set time (`at`), once a day. A Mac asleep at that time catches up within three
 *   hours, even past midnight. Each run takes a lock file first, so two zenith servers (dev
 *   and the app) never run the same routine twice; a run that fails to start frees it and
 *   is retried, three times at most.
 * - On an event (`on`): each new Now item of those kinds (a CI failure, an email to answer…)
 *   is handed to the agent as it appears, once, with the item's own request. What was
 *   already waiting when the routine was added is left to you.
 */

const DIR = path.join(process.cwd(), ".data", "routines");
const CATCH_UP_MS = 3 * 3600e3;
const MAX_ATTEMPTS = 3;

export type RoutineRun = { id: string; day: string; at: string; manual?: boolean; threadId?: string; environmentId?: string; error?: string; item?: string };

/** Local date, weekday (1 = Monday) and minutes since midnight of an instant, in your time zone. */
function local(t: number) {
  const parts = Object.fromEntries(
    new Intl.DateTimeFormat("en-GB", { timeZone: l10n().timeZone, year: "numeric", month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit", weekday: "short", hourCycle: "h23" })
      .formatToParts(new Date(t))
      .map((p) => [p.type, p.value]),
  );
  const weekday = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"].indexOf(parts.weekday) + 1;
  return { day: `${parts.year}-${parts.month}-${parts.day}`, weekday, minutes: Number(parts.hour) * 60 + Number(parts.minute) };
}

/** What each kind of Now item is called, for routines that watch them. */
export const KIND_NAMES = (): Record<NowKind, string> => ({
  down: tr("site en panne", "site down"),
  payment: tr("paiement en échec", "failing payment"),
  birthday: tr("anniversaire", "birthday"),
  sale: tr("acheteur en attente", "buyer waiting"),
  reply: tr("e-mail à répondre", "email to answer"),
  civic: tr("administratif", "paperwork"),
  ci: tr("CI cassée", "broken CI"),
  refresh: tr("relevé en retard", "stale capture"),
});

export const routineName = (r: RoutineConfig) =>
  r.name ?? (r.task ? TASKS[r.task].name() : r.skill ? r.skill : r.prompt ? r.prompt.slice(0, 48) : (r.on ?? []).map((k) => KIND_NAMES()[k]).join(", "));

const skillLine = (skill: string) => {
  const file = path.join(skillsDir(), skill, "SKILL.md");
  return tr(`Suis le skill « ${skill} » (${file}).`, `Follow the "${skill}" skill (${file}).`);
};

/** The request: the task, the skill or the Now item's own, then your words. */
export function routinePrompt(r: RoutineConfig, item?: NowItem): string {
  const parts = [item?.prompt ?? (r.task ? TASKS[r.task].prompt() : null), r.skill ? skillLine(r.skill) : null, r.prompt ?? null];
  return parts.filter(Boolean).join("\n\n");
}

/** Who runs it: the bot, else the project, else (for a Now item) where the item belongs, else the main agent. */
const targetOf = (r: RoutineConfig, item?: NowItem) => r.bot ?? r.project ?? item?.target ?? LIFE;

const lockFile = (id: string, day: string) => path.join(DIR, `${id}-${day}.json`);
const RUN_FILE = (id: string) => new RegExp(`^${id.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}-\\d{4}-\\d{2}-\\d{2}(-(manual|watch)-\\d+)?\\.json$`);

const g = globalThis as { __zenithRoutineAttempts?: Map<string, number>; __zenithRoutines?: NodeJS.Timeout };
const attempts = (g.__zenithRoutineAttempts ??= new Map());

const start = (r: RoutineConfig) =>
  ask({ prompt: routinePrompt(r), target: targetOf(r), source: "routine", title: `${tr("Routine", "Routine")} · ${routineName(r)}` });

/** The slot due now, today's or (past midnight) yesterday's, if any. */
function dueSlot(r: RoutineConfig & { at: string }): string | null {
  const [h, m] = r.at.split(":").map(Number);
  const now = Date.now();
  for (const back of [0, 1]) {
    const d = local(now - back * 864e5);
    const late = (local(now).minutes + back * 1440 - (h * 60 + m)) * 60e3;
    if (r.days.includes(d.weekday) && late >= 0 && late <= CATCH_UP_MS) return d.day;
  }
  return null;
}

async function runIfDue(r: RoutineConfig) {
  if (!r.enabled || !r.at) return;
  const day = dueSlot({ ...r, at: r.at });
  if (!day) return;
  const key = `${r.id}:${day}`;
  if ((attempts.get(key) ?? 0) >= MAX_ATTEMPTS) return;
  await mkdir(DIR, { recursive: true });
  let handle;
  try {
    handle = await open(lockFile(r.id, day), "wx");
  } catch {
    return; // already ran (or is running) for that day
  }
  try {
    const res = await start(r);
    await handle.writeFile(JSON.stringify({ id: r.id, day, at: new Date().toISOString(), threadId: res.threadId, environmentId: res.environmentId } satisfies RoutineRun));
  } catch (e) {
    attempts.set(key, (attempts.get(key) ?? 0) + 1);
    const error = e instanceof Error ? e.message : String(e);
    console.error(`[zenith] routine ${r.id}:`, error);
    // Free the slot for the next minute's attempt; keep the last error to show.
    await unlink(lockFile(r.id, day)).catch(() => {});
    if ((attempts.get(key) ?? 0) >= MAX_ATTEMPTS) await writeJson(path.join(DIR, `${r.id}-${day}-manual-0.json`), { id: r.id, day, at: new Date().toISOString(), error } satisfies RoutineRun);
  } finally {
    await handle.close().catch(() => {});
  }
}

// ——— On an event ——————————————————————————————————————————————————————————————

const WATCH_EVERY = 5 * 60e3;
/** At most this many hand-offs per routine per check, and per hour for all of them. */
const PER_CHECK = 3;
const PER_HOUR = 12;

type WatchState = { seen: string[]; since: string };
const watchFile = (id: string) => path.join(DIR, `watch-${id}.json`);

async function watchState(id: string): Promise<WatchState | null> {
  try {
    return JSON.parse(await readFile(watchFile(id), "utf8")) as WatchState;
  } catch {
    return null;
  }
}

/** Hands one Now item to the routine's agent, once, whichever zenith server gets there first. */
async function handOff(r: RoutineConfig, item: NowItem, manual = false): Promise<RoutineRun | null> {
  await mkdir(path.join(DIR, "watch"), { recursive: true });
  let handle;
  try {
    handle = await open(path.join(DIR, "watch", `${r.id}-${item.id.replace(/[^\w-]/g, "_")}`), "wx");
  } catch {
    return null; // the other server took it
  }
  await handle.close().catch(() => {});
  const { day } = local(Date.now());
  const run: RoutineRun = { id: r.id, day, at: new Date().toISOString(), manual, item: item.title };
  try {
    const res = await ask({ prompt: routinePrompt(r, item), target: targetOf(r, item), source: "watch", nowId: item.id, title: `${routineName(r)} · ${item.title}` });
    Object.assign(run, { threadId: res.threadId, environmentId: res.environmentId });
  } catch (e) {
    run.error = e instanceof Error ? e.message : String(e);
    console.error(`[zenith] routine ${r.id}:`, run.error);
  }
  await writeJson(path.join(DIR, `${r.id}-${day}-watch-${Date.now()}.json`), run);
  return run;
}

/** New items of the kinds a routine watches, not handled, snoozed or already given to an agent. */
const fresh = (r: RoutineConfig, items: NowItem[], seen: Set<string>) => items.filter((i) => r.on!.includes(i.kind) && !i.delegated && !seen.has(i.id));

async function watch(r: RoutineConfig, items: NowItem[]) {
  const state = await watchState(r.id);
  const current = items.filter((i) => r.on!.includes(i.kind)).map((i) => i.id);
  // First look: what is already waiting stays yours.
  if (!state) return writeJson(watchFile(r.id), { seen: current, since: new Date().toISOString() } satisfies WatchState);
  const seen = new Set(state.seen);
  for (const item of fresh(r, items, seen).slice(0, PER_CHECK)) {
    if (!underLimit("watch", PER_HOUR)) break;
    seen.add(item.id);
    await handOff(r, item);
  }
  // Remember what is still around (and what we just handed off), forget what left the list.
  const keep = new Set(current);
  await writeJson(watchFile(r.id), { seen: [...seen].filter((id) => keep.has(id)), since: state.since } satisfies WatchState);
}

let lastWatch = 0;

async function tick() {
  if (!codeStatus().running) return;
  const list = config().agent.routines;
  for (const r of list) await runIfDue(r).catch((e) => console.error(`[zenith] routine ${r.id}:`, e));
  const watchers = list.filter((r) => r.enabled && r.on?.length);
  if (!watchers.length || Date.now() - lastWatch < WATCH_EVERY) return;
  lastWatch = Date.now();
  const items = await now().catch(() => null);
  if (items) for (const r of watchers) await watch(r, items).catch((e) => console.error(`[zenith] routine ${r.id}:`, e));
}

export function startRoutines() {
  if (g.__zenithRoutines || !config().agent.enabled || !config().agent.routines.length) return;
  g.__zenithRoutines = setInterval(tick, 60_000);
  setTimeout(tick, 90_000);
}

/** Each routine with its latest run, scheduled or manual. */
export async function routines(): Promise<(RoutineConfig & { title: string; last: RoutineRun | null })[]> {
  const files = await readdir(DIR).catch(() => [] as string[]);
  return Promise.all(
    config().agent.routines.map(async (r) => {
      const runs = await Promise.all(
        files
          .filter((f) => RUN_FILE(r.id).test(f))
          .map((f) =>
            readFile(path.join(DIR, f), "utf8")
              .then((s) => (s ? (JSON.parse(s) as RoutineRun) : null))
              .catch(() => null),
          ),
      );
      const last = runs.filter((x): x is RoutineRun => !!x?.at).sort((a, b) => b.at.localeCompare(a.at))[0] ?? null;
      return { ...r, title: routineName(r), last };
    }),
  );
}

/**
 * Runs a routine now, whatever the time (the "Run" button). Today's scheduled run still
 * happens. A routine on an event takes what is waiting now, even what it left to you.
 */
export async function runNow(id: string) {
  const r = config().agent.routines.find((x) => x.id === id);
  if (!r) throw new Error(tr(`Routine inconnue : ${id}`, `Unknown routine: ${id}`));
  if (r.on?.length) {
    const items = fresh(r, await now(), new Set()).slice(0, PER_CHECK);
    if (!items.length) throw new Error(tr("Rien de ce genre n'attend en ce moment.", "Nothing of that kind is waiting right now."));
    const runs = [];
    for (const item of items) runs.push(await handOff(r, item, true));
    const first = runs.find((x) => x?.threadId && x.environmentId);
    if (!first) throw new Error(runs.find((x) => x?.error)?.error ?? tr("Déjà pris en charge.", "Already taken care of."));
    return { threadId: first.threadId!, environmentId: first.environmentId!, target: targetOf(r, items[0]), href: `/code/${encodeURIComponent(first.environmentId!)}/${encodeURIComponent(first.threadId!)}` };
  }
  const res = await start(r);
  const { day } = local(Date.now());
  await writeJson(path.join(DIR, `${r.id}-${day}-manual-${Date.now()}.json`), {
    id: r.id,
    day,
    at: new Date().toISOString(),
    manual: true,
    threadId: res.threadId,
    environmentId: res.environmentId,
  } satisfies RoutineRun);
  return res;
}
