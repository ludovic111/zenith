import "server-only";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { cached } from "../source";
import { today } from "../format";

export type AppleEvent = { title: string; start: string; end: string | null; allDay: boolean; location: string | null; calendar: string };
export type Track = { title: string; artist: string; album: string; app: string; at: string };
export type AppleSnapshot = {
  capturedAt: string;
  calendar: { authorized: boolean; calendars: string[]; events: AppleEvent[] };
  reminders: { authorized: boolean; items: { title: string; due: string | null; list: string; priority: number }[] };
  mail: { running: boolean; authorized: boolean; error?: string; accounts: { name: string; unread: number }[]; recent: { account: string; from: string; subject: string; date: string }[] };
  // Added later: missing from snapshots written by an older zenith.app.
  birthdays?: { authorized: boolean; items: { name: string; month: number; day: number; year?: number }[] };
  music?: { current: Track | null; recent: Track[]; denied: string[]; running: string[] };
  screen?: { tracking: boolean; days: { date: string; apps: { name: string; seconds: number }[] }[] };
};

/**
 * Latest snapshot written by the Mac app, zenith.app, in .data/apple.json (Calendar,
 * Reminders, Mail, Contacts, music, screen time). Null until the app has run once.
 */
export const apple = () =>
  cached("apple", 30, async (): Promise<AppleSnapshot | null> => {
    const raw = await readFile(path.join(process.cwd(), ".data", "apple.json"), "utf8").catch(() => null);
    return raw ? (JSON.parse(raw) as AppleSnapshot) : null;
  });

export type Birthday = { name: string; date: string; inDays: number; age: number | null };

/** Birthdays in the next `days` days, soonest first. */
export function upcomingBirthdays(a: AppleSnapshot | null, days = 30): Birthday[] {
  if (!a?.birthdays?.authorized) return [];
  const t = today();
  const base = new Date(`${t}T12:00:00Z`).getTime();
  const year = Number(t.slice(0, 4));
  return a.birthdays.items
    .map((b) => {
      const pad = (n: number) => String(n).padStart(2, "0");
      let y = year;
      let date = `${y}-${pad(b.month)}-${pad(b.day)}`;
      if (date < t) date = `${++y}-${pad(b.month)}-${pad(b.day)}`;
      const inDays = Math.round((new Date(`${date}T12:00:00Z`).getTime() - base) / 864e5);
      // Apple stores 1604 as the year when it is unknown.
      return { name: b.name, date, inDays, age: b.year && b.year > 1900 ? y - b.year : null };
    })
    .filter((b) => b.inDays <= days)
    .sort((x, y) => x.inDays - y.inDays);
}

export type ScreenDay = { date: string; total: number; apps: { name: string; seconds: number }[] };

/** Screen time per day (active time in front of the Mac, per app), most recent last. */
export function screenDays(a: AppleSnapshot | null): ScreenDay[] {
  return (a?.screen?.days ?? []).map((d) => ({ ...d, total: d.apps.reduce((s, x) => s + x.seconds, 0) }));
}

export const screenToday = (a: AppleSnapshot | null) => screenDays(a).find((d) => d.date === today()) ?? null;
