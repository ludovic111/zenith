import { l10n, tr } from "@/lib/i18n";

/** Day (YYYY-MM-DD) of an instant, in your time zone. */
export const dayKey = (t: string | number) => new Intl.DateTimeFormat("en-CA", { timeZone: l10n().timeZone }).format(new Date(t));

/** Hour and minute of an instant, in your time zone. */
export const hm = (t: string) => new Intl.DateTimeFormat(l10n().locale, { timeZone: l10n().timeZone, hour: "2-digit", minute: "2-digit" }).format(new Date(t));

/** Formats a calendar day (YYYY-MM-DD) without time zone drift. */
export const dayLabel = (ymd: string, opts: Intl.DateTimeFormatOptions) =>
  new Intl.DateTimeFormat(l10n().locale, { timeZone: "UTC", ...opts }).format(new Date(`${ymd.slice(0, 10)}T12:00:00Z`));

/** Formats a local wall-clock time without offset ("2026-01-31T07:24", as Open-Meteo gives it). */
export const clock = (local: string) =>
  new Intl.DateTimeFormat(l10n().locale, { timeZone: "UTC", hour: "2-digit", minute: "2-digit" }).format(new Date(`${local.slice(0, 16)}Z`));

/** "Today", "Tomorrow", else "Thursday 2 October". */
export function relativeDay(ymd: string, now = Date.now()) {
  if (ymd === dayKey(now)) return tr("Aujourd'hui", "Today");
  if (ymd === dayKey(now + 864e5)) return tr("Demain", "Tomorrow");
  return dayLabel(ymd, { weekday: "long", day: "numeric", month: "long" }).replace(/^./, (c) => c.toUpperCase());
}

/** "today", "tomorrow", "in 3 d". */
export const inDays = (n: number) => (n === 0 ? tr("aujourd'hui", "today") : n === 1 ? tr("demain", "tomorrow") : tr(`dans ${n} j`, `in ${n} d`));

/** Seconds as "2 h 05" or "35 min". */
export const hours = (s: number) => (s >= 3600 ? `${Math.floor(s / 3600)} h ${String(Math.round((s % 3600) / 60)).padStart(2, "0")}` : `${Math.round(s / 60)} min`);

/** The current instant (server components render once per request). */
export const nowMs = () => Date.now();
