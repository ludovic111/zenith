import "server-only";
import { mkdir, open, readdir, readFile, writeFile } from "node:fs/promises";
import path from "node:path";
import { config, type RoutineConfig } from "../config";
import { l10n, tr } from "../i18n";
import { codeStatus } from "../code/manager";
import { ask } from "./ask";
import { LIFE } from "./target";
import { TASKS } from "./tasks";

/**
 * Routines: requests that run on their own, once a day at a set time (`agent.routines`).
 * A Mac asleep at that time catches up within three hours. Each run takes a lock file
 * first, so two zenith servers (dev and the app) never run the same routine twice.
 */

const DIR = path.join(process.cwd(), ".data", "routines");
const CATCH_UP_MS = 3 * 3600e3;

export type RoutineRun = { id: string; day: string; at: string; threadId?: string; environmentId?: string; error?: string };

/** Local date, weekday (1 = Monday) and minutes since midnight, in your time zone. */
function localNow() {
  const parts = Object.fromEntries(
    new Intl.DateTimeFormat("en-GB", { timeZone: l10n().timeZone, year: "numeric", month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit", weekday: "short", hourCycle: "h23" })
      .formatToParts(new Date())
      .map((p) => [p.type, p.value]),
  );
  const weekday = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"].indexOf(parts.weekday) + 1;
  return { day: `${parts.year}-${parts.month}-${parts.day}`, weekday, minutes: Number(parts.hour) * 60 + Number(parts.minute) };
}

export const routineName = (r: RoutineConfig) => r.name ?? (r.task ? TASKS[r.task].name() : r.prompt!.slice(0, 48));
export const routinePrompt = (r: RoutineConfig) => (r.task ? TASKS[r.task].prompt() : r.prompt!);

const lockFile = (id: string, day: string) => path.join(DIR, `${id}-${day}.json`);

async function runIfDue(r: RoutineConfig) {
  const { day, weekday, minutes } = localNow();
  if (!r.enabled || !r.days.includes(weekday)) return;
  const [h, m] = r.at.split(":").map(Number);
  const late = (minutes - (h * 60 + m)) * 60e3;
  if (late < 0 || late > CATCH_UP_MS) return;
  await mkdir(DIR, { recursive: true });
  let handle;
  try {
    handle = await open(lockFile(r.id, day), "wx");
  } catch {
    return; // already ran today (or is running)
  }
  const run: RoutineRun = { id: r.id, day, at: new Date().toISOString() };
  try {
    const res = await ask({ prompt: routinePrompt(r), target: r.project ?? LIFE, source: "routine", title: `${tr("Routine", "Routine")} · ${routineName(r)}` });
    Object.assign(run, { threadId: res.threadId, environmentId: res.environmentId });
  } catch (e) {
    run.error = e instanceof Error ? e.message : String(e);
  }
  await handle.writeFile(JSON.stringify(run));
  await handle.close();
}

async function tick() {
  if (!codeStatus().running) return;
  for (const r of config().agent.routines) await runIfDue(r).catch((e) => console.error(`[zenith] routine ${r.id}:`, e));
}

const g = globalThis as { __zenithRoutines?: NodeJS.Timeout };

export function startRoutines() {
  if (g.__zenithRoutines || !config().agent.enabled || !config().agent.routines.length) return;
  g.__zenithRoutines = setInterval(tick, 60_000);
  setTimeout(tick, 90_000);
}

/** Each routine with its latest run. */
export async function routines(): Promise<(RoutineConfig & { title: string; last: RoutineRun | null })[]> {
  const files = await readdir(DIR).catch(() => [] as string[]);
  return Promise.all(
    config().agent.routines.map(async (r) => {
      const mine = files.filter((f) => f.startsWith(`${r.id}-`) && f.endsWith(".json")).sort();
      const latest = mine.at(-1);
      const last = latest ? ((await readFile(path.join(DIR, latest), "utf8").then((s) => (s ? JSON.parse(s) : null)).catch(() => null)) as RoutineRun | null) : null;
      return { ...r, title: routineName(r), last };
    }),
  );
}

/** Runs a routine now, whatever the time (the "Run now" button). */
export async function runNow(id: string) {
  const r = config().agent.routines.find((x) => x.id === id);
  if (!r) throw new Error(tr(`Routine inconnue : ${id}`, `Unknown routine: ${id}`));
  const res = await ask({ prompt: routinePrompt(r), target: r.project ?? LIFE, source: "routine", title: `${tr("Routine", "Routine")} · ${routineName(r)}` });
  await mkdir(DIR, { recursive: true });
  const { day } = localNow();
  await writeFile(lockFile(r.id, day), JSON.stringify({ id: r.id, day, at: new Date().toISOString(), threadId: res.threadId, environmentId: res.environmentId } satisfies RoutineRun));
  return res;
}
