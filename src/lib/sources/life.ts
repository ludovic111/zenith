import "server-only";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { cached } from "../source";
import { l10n } from "../i18n";
import { sessions } from "./agents";
import { allLocalRepos } from "./git";

export type LifeEvent = { title: string; start: string; end: string | null; allDay: boolean; location: string | null; calendar: string; link: string | null };
export type Life = {
  capturedAt: string;
  agenda: LifeEvent[];
  inbox: { unread: number; unreadImportant: number; needsReply: { from: string; subject: string; date: string; why: string; link: string }[] };
  deliveries: { merchant: string; status: string; eta: string | null; link: string }[];
  sales: { item: string; platform: string; messages: number; lastMessageAt: string; link: string }[];
  spending: { currency: string; months: { month: string; categories: { label: string; amount: number; count: number }[] }[] };
  civic: { title: string; date: string | null; note: string; link: string }[];
  notes: string[];
};

/**
 * Google Calendar and Gmail go through Claude's connectors: zenith has no direct access.
 * Claude writes a snapshot to .data/life.json when asked ("update my life in zenith");
 * docs/releves.md describes the format. Null until then.
 */
export const life = () =>
  cached("life", 30, async (): Promise<Life | null> => {
    const raw = await readFile(path.join(process.cwd(), ".data", "life.json"), "utf8").catch(() => null);
    return raw ? (JSON.parse(raw) as Life) : null;
  });

const DAY = 864e5;
/** Day (YYYY-MM-DD) and hour of an instant, in your time zone. */
const localDay = (t: number) => new Intl.DateTimeFormat("en-CA", { timeZone: l10n().timeZone }).format(t);
const localHour = (t: number) => Number(new Intl.DateTimeFormat("en-GB", { timeZone: l10n().timeZone, hour: "numeric", hourCycle: "h23" }).format(t));

/**
 * Work rhythm, rebuilt without asking anything: 15-minute slots where an agent really
 * worked (a session left open does not count) and commit times.
 */
export const rhythm = () =>
  cached("rhythm", 120, async () => {
    const [agents, repos] = await Promise.all([sessions().catch(() => []), allLocalRepos()]);
    const start = Date.now() - 14 * DAY;
    const SLOT = 15 * 60e3;
    // Active 15-min slots across all sessions (two agents in parallel count once).
    const active = new Set<number>();
    for (const s of agents) for (const slot of s.slots) if (slot * SLOT >= start) active.add(slot);
    const days = new Map<string, { hours: number; commits: number; night: number }>();
    for (let t = start; t <= Date.now(); t += DAY) days.set(localDay(t), { hours: 0, commits: 0, night: 0 });
    for (const slot of active) {
      const t = slot * SLOT;
      const d = days.get(localDay(t));
      if (d) {
        d.hours += 0.25;
        if (localHour(t) < 6) d.night += 0.25;
      }
    }
    const commits = repos.flatMap((r) => r.commits).filter((c) => c.at > start);
    for (const c of commits) {
      const d = days.get(localDay(c.at));
      if (d) d.commits++;
    }
    const lateCommits = commits.filter((c) => localHour(c.at) < 6);
    const list = [...days.entries()].map(([date, v]) => ({ date, ...v, hours: Math.round(v.hours * 10) / 10 }));
    const week = list.slice(-7);
    return {
      days: list,
      weekHours: Math.round(week.reduce((a, d) => a + d.hours, 0)),
      weekCommits: week.reduce((a, d) => a + d.commits, 0),
      nightHours: Math.round(week.reduce((a, d) => a + d.night, 0) * 10) / 10,
      lastLate: lateCommits.length ? Math.max(...lateCommits.map((c) => c.at)) : null,
      daysOff: week.filter((d) => d.hours < 0.5 && d.commits === 0).length,
    };
  });
