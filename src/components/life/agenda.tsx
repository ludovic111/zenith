import { Cake, Flag, MapPin } from "lucide-react";
import { source } from "@/lib/source";
import { tr } from "@/lib/i18n";
import { life } from "@/lib/sources/life";
import { apple, upcomingBirthdays } from "@/lib/sources/apple";
import { nextHolidays } from "@/lib/sources/environment";
import { AskButton } from "@/components/agent/ask-button";
import { Empty, Panel } from "@/components/z/panel";
import { cn } from "@/lib/utils";
import { agendaEvents, upcoming, type AgendaEvent } from "./events";
import { Counted, Row, Rows } from "./rows";
import { giftPrompt } from "./prompts";
import { dayKey, dayLabel, hm, inDays, nowMs, relativeDay } from "./time";

/** The next two weeks, grouped by day. */
export async function Agenda() {
  const [snap, ap, hol] = await Promise.all([source(life), source(apple), source(() => nextHolidays(4))]);
  const l = snap.ok ? snap.data : null;
  const a = ap.ok ? ap.data : null;
  const now = nowMs();
  const list = upcoming(agendaEvents(l, a, hol.ok ? hol.data : []), now);
  const groups = new Map<string, AgendaEvent[]>();
  for (const e of list) {
    const k = dayKey(e.start.length === 10 ? `${e.start}T12:00:00` : e.start);
    groups.set(k, [...(groups.get(k) ?? []), e]);
  }
  const todayKey = dayKey(now);
  return (
    <Panel title={<Counted count={list.length}>{tr("Agenda", "Calendar")}</Counted>} action={tr("14 jours", "14 days")} bodyClassName="p-0 pb-1.5 pt-1">
      {!l && !a && !list.length ? (
        <div className="px-4 pb-3">
          <Empty>{tr("Agenda pas encore relevé : ouvre zenith.app ou actualise ta vie.", "Calendar not captured yet: open zenith.app or refresh your life.")}</Empty>
        </div>
      ) : !list.length ? (
        <div className="px-4 pb-3">
          <Empty>{tr("Rien au programme sur deux semaines.", "Nothing planned for two weeks.")}</Empty>
        </div>
      ) : (
        [...groups.entries()].slice(0, 8).map(([k, events]) => (
          <div key={k}>
            <div className={cn("flex h-7 items-end px-4 pb-1 text-xs font-medium", k === todayKey ? "text-ink" : "text-ink-3")}>{relativeDay(k, now)}</div>
            <ul>
              {events.map((e) => {
                const past = new Date(e.end ?? e.start).getTime() < now && !e.allDay;
                const Icon = e.kind === "birthday" ? Cake : e.kind === "holiday" ? Flag : null;
                const title = e.link ? (
                  <a href={e.link} target="_blank" rel="noopener noreferrer" className="underline-offset-2 hover:underline">
                    {e.title}
                  </a>
                ) : (
                  e.title
                );
                return (
                  <li key={e.title + e.start} className={cn("flex min-h-9 items-center gap-3 px-4 py-1.5 transition-colors hover:bg-hover", past && "opacity-50")}>
                    <span className="w-11 shrink-0 text-xs text-ink-2 tabular">{e.allDay ? tr("journée", "all day") : hm(e.start)}</span>
                    {Icon ? <Icon className="size-3.5 shrink-0 text-ink-3" /> : <span className="mx-[3px] size-2 shrink-0 rounded-full bg-primary" />}
                    <div className="min-w-0 flex-1">
                      <div className="truncate text-[13px] text-ink">{title}</div>
                      {(e.location || (!e.allDay && e.end)) && (
                        <div className="flex gap-3 truncate text-xs text-ink-3">
                          {!e.allDay && e.end && <span className="tabular">{tr("jusqu'à", "until")} {hm(e.end)}</span>}
                          {e.location && (
                            <span className="inline-flex min-w-0 items-center gap-1 truncate">
                              <MapPin className="size-3 shrink-0" />
                              {e.location}
                            </span>
                          )}
                        </div>
                      )}
                    </div>
                    <span className="max-w-[40%] shrink-0 truncate text-2xs text-ink-3 max-sm:hidden">{e.calendar}</span>
                  </li>
                );
              })}
            </ul>
          </div>
        ))
      )}
    </Panel>
  );
}

/** Birthdays (30 days) and public holidays. */
export async function Birthdays({ agent }: { agent: boolean }) {
  const [ap, hol] = await Promise.all([source(apple), source(() => nextHolidays(4))]);
  const a = ap.ok ? ap.data : null;
  const list = upcomingBirthdays(a, 30);
  const holidays = hol.ok ? hol.data : [];
  return (
    <Panel title={<Counted count={list.length}>{tr("Anniversaires", "Birthdays")}</Counted>} action={tr("30 jours", "30 days")} bodyClassName="p-0 pb-1.5 pt-1">
      {!a?.birthdays ? (
        <p className="px-4 pb-3 text-[13px] text-ink-3">{tr("zenith.app les lira dans Contacts après sa prochaine installation.", "zenith.app reads them from Contacts once installed.")}</p>
      ) : !a.birthdays.authorized ? (
        <p className="px-4 pb-3 text-[13px] text-ink-3">{tr("Contacts refusés : Réglages Système → Confidentialité → Contacts → zenith.", "Contacts denied: System Settings → Privacy → Contacts → zenith.")}</p>
      ) : list.length ? (
        <Rows>
          {list.slice(0, 6).map((b) => (
            <Row
              key={b.name + b.date}
              icon={Cake}
              tone={b.inDays === 0 ? "text-ink" : undefined}
              title={<span className={b.inDays === 0 ? "font-medium" : undefined}>{b.name}</span>}
              meta={b.age ? tr(`${b.age} ans`, `turns ${b.age}`) : undefined}
              aside={<span className={b.inDays === 0 ? "font-medium text-ink" : undefined}>{inDays(b.inDays)}</span>}
              action={agent && b.inDays <= 7 && <AskButton prompt={giftPrompt({ name: b.name, age: b.age, when: inDays(b.inDays) })} target="life" label={tr("Trouve une idée de cadeau", "Find a gift idea")} />}
            />
          ))}
        </Rows>
      ) : (
        <p className="px-4 pb-3 text-[13px] text-ink-3">{tr("Aucun anniversaire dans les 30 jours.", "No birthday in the next 30 days.")}</p>
      )}
      {holidays.length > 0 && (
        <>
          <div className="flex h-8 items-end border-t border-line px-4 pb-1 text-xs font-medium text-ink-3">{tr("Jours fériés", "Public holidays")}</div>
          <Rows>
            {holidays.map((h) => (
              <Row key={h.date} icon={Flag} title={<span className="text-ink-2">{h.name}</span>} aside={dayLabel(h.date, { weekday: "short", day: "numeric", month: "short" })} />
            ))}
          </Rows>
        </>
      )}
    </Panel>
  );
}
