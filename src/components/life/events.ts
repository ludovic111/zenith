import { config } from "@/lib/config";
import { tr } from "@/lib/i18n";
import type { Life, LifeEvent } from "@/lib/sources/life";
import { upcomingBirthdays, type AppleSnapshot } from "@/lib/sources/apple";
import type { Holiday } from "@/lib/sources/environment";
import { dayKey } from "./time";

export type AgendaEvent = LifeEvent & { kind: "event" | "birthday" | "holiday" };

/**
 * One agenda: Google (Claude's snapshot) + Apple (zenith.app) + public holidays + birthdays,
 * over the next `days` days, without duplicate titles at the same time, soonest first.
 */
export function agendaEvents(l: Life | null, a: AppleSnapshot | null, holidays: Holiday[], days = 14): AgendaEvent[] {
  const loc = config().location;
  const allDay = (title: string, day: string, calendar: string, kind: AgendaEvent["kind"]): AgendaEvent => ({ title, start: day, end: null, allDay: true, location: null, calendar, link: null, kind });
  const holidayLabel = loc ? tr(`Férié · ${loc.name}`, `Public holiday · ${loc.name}`) : tr("Jour férié", "Public holiday");
  const until = dayKey(Date.now() + days * 864e5);
  // Events of a birthdays calendar read as birthdays.
  const kindOf = (calendar: string): AgendaEvent["kind"] => (/birthday|anniversaire|geburtstag|cumple/i.test(calendar) ? "birthday" : "event");
  const events: AgendaEvent[] = [
    ...(l?.agenda ?? []).map((e) => ({ ...e, kind: kindOf(e.calendar) })),
    ...(a?.calendar.events ?? []).map((e) => ({ ...e, calendar: `${e.calendar} · Apple`, link: null, kind: kindOf(e.calendar) })),
  ];
  // A birthday already in a calendar that day (the Birthdays calendar) is not listed twice.
  const known = (name: string, day: string) => events.some((e) => (e.start.length <= 10 ? e.start : dayKey(e.start)) === day && e.title.toLowerCase().includes(name.split(" ")[0].toLowerCase()));
  return [
    ...events,
    ...holidays.filter((h) => h.date <= until).map((h) => allDay(h.name, h.date, holidayLabel, "holiday")),
    ...upcomingBirthdays(a, days).filter((b) => !known(b.name, b.date)).map((b) => allDay(`${b.name}${b.age ? tr(` · ${b.age} ans`, ` · turns ${b.age}`) : ""}`, b.date, tr("Anniversaire", "Birthday"), "birthday")),
  ]
    .filter((e, i, all) => all.findIndex((x) => x.title === e.title && x.start.slice(0, 16) === e.start.slice(0, 16)) === i)
    .sort((x, y) => x.start.localeCompare(y.start));
}

/** Still ahead (or ended less than an hour ago). */
export const upcoming = (events: AgendaEvent[], now = Date.now()) => events.filter((e) => new Date(e.end ?? e.start).getTime() >= now - 3600e3);

/** Sales grouped by item: the same thing sold on several platforms is one row. */
export function salesByItem(l: Life | null) {
  const map = new Map<string, { item: string; platforms: string[]; messages: number; last: string; links: string[] }>();
  for (const s of l?.sales ?? []) {
    const g = map.get(s.item) ?? { item: s.item, platforms: [], messages: 0, last: s.lastMessageAt, links: [] };
    if (!g.platforms.includes(s.platform)) g.platforms.push(s.platform);
    g.messages += s.messages;
    if (s.lastMessageAt > g.last) g.last = s.lastMessageAt;
    if (s.link) g.links.push(s.link);
    map.set(s.item, g);
  }
  return [...map.values()].sort((x, y) => y.last.localeCompare(x.last));
}
