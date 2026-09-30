import "server-only";
import { l10n } from "@/lib/i18n";
import { KIND_NAMES, routines } from "@/lib/agent/routines";
import { agentUi } from "@/lib/agent/ui";
import type { RoutineView } from "./routines-panel";

/** Weekday (1 = Monday) and minutes since midnight, now, in your time zone. */
function localNow() {
  const parts = Object.fromEntries(
    new Intl.DateTimeFormat("en-GB", { timeZone: l10n().timeZone, weekday: "short", hour: "2-digit", minute: "2-digit", hourCycle: "h23" })
      .formatToParts(new Date())
      .map((p) => [p.type, p.value]),
  );
  return { weekday: ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"].indexOf(parts.weekday) + 1, minutes: Number(parts.hour) * 60 + Number(parts.minute) };
}

/** Next scheduled run of a routine (ISO), or null when paused or on no day. */
export function nextRun(r: { at?: string; days: number[]; enabled: boolean }): string | null {
  if (!r.enabled || !r.at || !r.days.length) return null;
  const [h, m] = r.at.split(":").map(Number);
  const now = localNow();
  for (let k = 0; k <= 7; k++) {
    const weekday = ((now.weekday - 1 + k) % 7) + 1;
    const offset = k * 1440 + h * 60 + m - now.minutes;
    if (r.days.includes(weekday) && offset > 0) return new Date(Math.floor(Date.now() / 60e3) * 60e3 + offset * 60e3).toISOString();
  }
  return null;
}

/** zenith's routines as the list shows them: where each runs, its last run and its next one. */
export async function routineViews(): Promise<RoutineView[]> {
  const list = await routines();
  const names = Object.fromEntries(agentUi().targets.map((t) => [t.id, t.name]));
  return list.map((r) => ({
    id: r.id,
    title: r.title,
    at: r.at ?? null,
    on: r.on ? r.on.map((k) => KIND_NAMES()[k]) : null,
    days: r.days,
    enabled: r.enabled,
    target: names[r.bot ?? r.project ?? "life"] ?? r.bot ?? r.project ?? "",
    last: r.last,
    next: nextRun(r),
  }));
}
