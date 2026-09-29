import "server-only";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import path from "node:path";
import { PROJECTS, type ProjectId } from "../projects";

export type Sample = { t: number; ms: number | null; status: number };
export type ProbeState = { project: ProjectId; label: string; url: string; samples: Sample[] };

const KEEP = 24 * 60; // one day at one sample per minute
const FILE = path.join(process.cwd(), ".data", "uptime.json");

type Store = { probes: Map<string, ProbeState>; timer?: NodeJS.Timeout; loaded?: Promise<void> };
const g = globalThis as { __zenithUptime?: Store };
const store: Store = (g.__zenithUptime ??= { probes: new Map() });

for (const p of PROJECTS)
  for (const probe of p.probes)
    if (!store.probes.has(probe.url)) store.probes.set(probe.url, { project: p.id, ...probe, samples: [] });

function load() {
  return (store.loaded ??= readFile(FILE, "utf8")
    .then((raw) => {
      const saved = JSON.parse(raw) as Record<string, Sample[]>;
      for (const [url, samples] of Object.entries(saved)) {
        const probe = store.probes.get(url);
        if (probe && !probe.samples.length) probe.samples = samples.slice(-KEEP);
      }
    })
    .catch(() => {}));
}

async function save() {
  await mkdir(path.dirname(FILE), { recursive: true });
  const out = Object.fromEntries([...store.probes.values()].map((p) => [p.url, p.samples]));
  await writeFile(FILE, JSON.stringify(out));
}

async function ping(url: string): Promise<Sample> {
  const t = Date.now();
  try {
    const res = await fetch(url, { redirect: "manual", cache: "no-store", signal: AbortSignal.timeout(10000) });
    await res.body?.cancel();
    return { t, ms: Date.now() - t, status: res.status };
  } catch {
    return { t, ms: null, status: 0 };
  }
}

/** A service is up: 2xx, a redirect, or a 401 from a protected endpoint (Supabase). */
export const isUp = (s: Sample) => s.status > 0 && (s.status < 400 || s.status === 401);

export async function probeAll() {
  await load();
  await Promise.all(
    [...store.probes.values()].map(async (p) => {
      p.samples.push(await ping(p.url));
      if (p.samples.length > KEEP) p.samples.splice(0, p.samples.length - KEEP);
    }),
  );
  await save().catch(() => {});
}

export function startMonitor() {
  if (store.timer) return;
  probeAll();
  store.timer = setInterval(probeAll, 60_000);
}

export async function uptime() {
  await load();
  if (![...store.probes.values()].some((p) => p.samples.length)) await probeAll();
  return [...store.probes.values()].map((p) => {
    const last = p.samples.at(-1) ?? null;
    const up = p.samples.filter(isUp).length;
    const ok = p.samples.filter((s) => s.ms != null).map((s) => s.ms!);
    return {
      ...p,
      samples: p.samples.slice(-90),
      last,
      up: last ? isUp(last) : null,
      ratio: p.samples.length ? up / p.samples.length : null,
      avg: ok.length ? Math.round(ok.reduce((a, b) => a + b, 0) / ok.length) : null,
    };
  });
}

export type Uptime = Awaited<ReturnType<typeof uptime>>[number];
