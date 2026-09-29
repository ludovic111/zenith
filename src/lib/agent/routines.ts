import "server-only";
import { mkdir, open, readdir, readFile, unlink } from "node:fs/promises";
import path from "node:path";
import { config, type RoutineConfig } from "../config";
import { l10n, tr } from "../i18n";
import { codeStatus } from "../code/manager";
import { ask } from "./ask";
import { writeJson } from "./files";
import { LIFE } from "./target";
import { TASKS } from "./tasks";

/**
 * Routines: requests that run on their own, once a day at a set time (`agent.routines`).
 * A Mac asleep at that time catches up within three hours, even past midnight. Each run
 * takes a lock file first, so two zenith servers (dev and the app) never run the same
 * routine twice; a run that fails to start frees it and is retried, three times at most.
 */

const DIR = path.join(process.cwd(), ".data", "routines");
const CATCH_UP_MS = 3 * 3600e3;
const MAX_ATTEMPTS = 3;

export type RoutineRun = { id: string; day: string; at: string; manual?: boolean; threadId?: string; environmentId?: string; error?: string };

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

export const routineName = (r: RoutineConfig) => r.name ?? (r.task ? TASKS[r.task].name() : r.prompt!.slice(0, 48));
export const routinePrompt = (r: RoutineConfig) => (r.task ? TASKS[r.task].prompt() : r.prompt!);

const lockFile = (id: string, day: string) => path.join(DIR, `${id}-${day}.json`);
const RUN_FILE = (id: string) => new RegExp(`^${id.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}-\\d{4}-\\d{2}-\\d{2}(-manual-\\d+)?\\.json$`);

const g = globalThis as { __zenithRoutineAttempts?: Map<string, number>; __zenithRoutines?: NodeJS.Timeout };
const attempts = (g.__zenithRoutineAttempts ??= new Map());

const start = (r: RoutineConfig) =>
  ask({ prompt: routinePrompt(r), target: r.project ?? LIFE, source: "routine", title: `${tr("Routine", "Routine")} · ${routineName(r)}` });

/** The slot due now, today's or (past midnight) yesterday's, if any. */
function dueSlot(r: RoutineConfig): string | null {
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
  if (!r.enabled) return;
  const day = dueSlot(r);
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

async function tick() {
  if (!codeStatus().running) return;
  for (const r of config().agent.routines) await runIfDue(r).catch((e) => console.error(`[zenith] routine ${r.id}:`, e));
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

/** Runs a routine now, whatever the time (the "Run" button). Today's scheduled run still happens. */
export async function runNow(id: string) {
  const r = config().agent.routines.find((x) => x.id === id);
  if (!r) throw new Error(tr(`Routine inconnue : ${id}`, `Unknown routine: ${id}`));
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
