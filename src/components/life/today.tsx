import { Sunrise, Sunset } from "lucide-react";
import { source } from "@/lib/source";
import { config } from "@/lib/config";
import { plural, tr } from "@/lib/i18n";
import { ago, date, nf } from "@/lib/format";
import { describe, weather } from "@/lib/sources/weather";
import { life, rhythm } from "@/lib/sources/life";
import { urgent } from "@/lib/subscriptions";
import { apple, screenToday, upcomingBirthdays } from "@/lib/sources/apple";
import { air, aqiLabel, nextHolidays, pollenLevel, water } from "@/lib/sources/environment";
import { Gate } from "@/components/z/gate";
import { cn } from "@/lib/utils";
import { WeatherIcon } from "./weather-icon";
import { agendaEvents, salesByItem, upcoming } from "./events";
import { clock, dayKey, dayLabel, hm, hours, nowMs } from "./time";

/** "Today" at a glance: weather, air, next event, birthdays and what is waiting. */
export async function Today() {
  const [w, aq, wt, snap, ap, hol, rh] = await Promise.all([source(weather), source(air), source(water), source(life), source(apple), source(() => nextHolidays(4)), source(rhythm)]);
  const l = snap.ok ? snap.data : null;
  const a = ap.ok ? ap.data : null;
  const holidays = hol.ok ? hol.data : [];
  const now = nowMs();
  const todayKey = dayKey(now);
  const events = upcoming(agendaEvents(l, a, holidays));
  const next = events.find((e) => e.kind === "event" && new Date(e.end ?? e.start).getTime() > now);
  const birthdays = upcomingBirthdays(a, 0);
  const holiday = holidays.find((h) => h.date === todayKey);
  const current = a?.music?.current;
  // The snapshot is written every 5 minutes: after 10, the track is surely over.
  const playing = current && now - new Date(current.at).getTime() < 10 * 60e3 ? current : null;

  const waiting = [
    { n: urgent().length, label: plural(urgent().length, ["paiement en échec", "paiements en échec"], ["failed payment", "failed payments"]), bad: true },
    { n: l?.inbox.needsReply.length ?? 0, label: plural(l?.inbox.needsReply.length ?? 0, ["e-mail à répondre", "e-mails à répondre"], ["email to answer", "emails to answer"]) },
    { n: salesByItem(l).filter((s) => s.messages > 0).length, label: plural(salesByItem(l).filter((s) => s.messages > 0).length, ["vente", "ventes"], ["sale", "sales"]) },
    { n: l?.deliveries.length ?? 0, label: plural(l?.deliveries.length ?? 0, ["colis", "colis"], ["parcel", "parcels"]) },
    { n: l?.civic.length ?? 0, label: plural(l?.civic.length ?? 0, ["démarche", "démarches"], ["paperwork item", "paperwork items"]) },
  ].filter((x) => x.n > 0);

  // Your day so far: time in front of the Mac, agents at work, commits.
  const screen = screenToday(a);
  const work = rh.ok ? rh.data.days.find((d) => d.date === todayKey) : null;
  const day = [
    ...(screen && screen.total >= 60 ? [tr(`${hours(screen.total)} d'écran`, `${hours(screen.total)} of screen`)] : []),
    ...(work && work.hours > 0 ? [tr(`${nf(work.hours, 1)} h d'agents`, `${nf(work.hours, 1)} h of agents`)] : []),
    ...(work && work.commits > 0 ? [`${work.commits} ${plural(work.commits, ["commit", "commits"], ["commit", "commits"])}`] : []),
  ];

  const when = (e: NonNullable<typeof next>) => {
    const k = dayKey(e.start);
    const day = k === todayKey ? tr("aujourd'hui", "today") : k === dayKey(now + 864e5) ? tr("demain", "tomorrow") : date(e.start, { weekday: "long", day: "numeric", month: "short" });
    return e.allDay ? day : tr(`${day} à ${hm(e.start)}`, `${day} at ${hm(e.start)}`);
  };

  const facts: { label: string; value: React.ReactNode }[] = [
    {
      label: tr("Prochain", "Next up"),
      value: next ? (
        <>
          <span className="text-ink">{next.title}</span> <span className="text-ink-3">· {when(next)}</span>
        </>
      ) : (
        <span className="text-ink-3">{l || a ? tr("Rien au programme sur 14 jours", "Nothing planned for 14 days") : tr("Agenda pas encore relevé", "Calendar not captured yet")}</span>
      ),
    },
    ...(birthdays.length
      ? [
          {
            label: tr("Anniversaire", "Birthday"),
            value: (
              <span className="text-ink">
                {birthdays.map((b, i) => (
                  <span key={b.name}>
                    {i > 0 && ", "}
                    {b.name}
                    {b.age ? <span className="text-ink-3"> · {tr(`${b.age} ans`, `turns ${b.age}`)}</span> : null}
                  </span>
                ))}
              </span>
            ),
          },
        ]
      : []),
    ...(holiday ? [{ label: tr("Férié", "Holiday"), value: <span className="text-ink">{holiday.name}</span> }] : []),
    {
      label: tr("T'attend", "Waiting"),
      value: waiting.length ? (
        <span className="text-ink">
          {waiting.map((x, i) => (
            <span key={x.label}>
              {i > 0 && <span className="text-ink-3"> · </span>}
              <span className={cn("font-medium tabular", x.bad && "text-bad")}>{x.n}</span> {x.label}
            </span>
          ))}
        </span>
      ) : (
        <span className="text-ink-3">{l ? tr("Rien, profite", "Nothing, enjoy") : tr("Boîte mail pas encore relevée", "Inbox not captured yet")}</span>
      ),
    },
    ...(day.length ? [{ label: tr("Ta journée", "Your day"), value: <span className="text-ink-2">{day.join(" · ")}</span> }] : []),
    ...(playing
      ? [
          {
            label: tr("En écoute", "Playing"),
            value: (
              <>
                <span className="text-ink">{playing.title}</span> <span className="text-ink-3">· {playing.artist}</span>
              </>
            ),
          },
        ]
      : []),
  ];

  const loc = config().location;
  const q = aq.ok ? aqiLabel(aq.data.aqi) : null;

  return (
    <section className="grid overflow-hidden rounded-xl border border-line bg-surface lg:grid-cols-[1.1fr_1fr]">
      <div className="p-5">
        {!loc ? (
          <p className="text-[13px] text-ink-2">
            {tr("Pour la météo, l'air et les jours fériés, ajoute ", "For weather, air quality and public holidays, add ")}
            <code className="font-mono text-xs text-ink">location</code>
            {tr(" (nom, latitude, longitude, pays) dans ", " (name, latitude, longitude, country) to ")}
            <code className="font-mono text-xs">zenith.config.json</code>.
          </p>
        ) : (
          <Gate src={w}>
            {(wx) => {
              const d = describe(wx.now.code, wx.now.isDay);
              const t = wx.days[0];
              return (
                <>
                  <div className="flex items-center gap-4">
                    <WeatherIcon code={wx.now.code} day={wx.now.isDay} className="size-9 shrink-0 text-ink-2" strokeWidth={1.5} />
                    <div className="min-w-0">
                      <div className="flex items-baseline gap-2.5">
                        <span className="text-3xl font-semibold tracking-tight text-ink tabular">{Math.round(wx.now.temp)}°</span>
                        <span className="truncate text-[13px] text-ink-2">{d.label}</span>
                      </div>
                      <div className="mt-0.5 flex flex-wrap items-center gap-x-3 text-xs text-ink-3 tabular">
                        <span>{Math.round(t.min)}° / {Math.round(t.max)}°</span>
                        <span>{tr("ressenti", "feels")} {Math.round(wx.now.feels)}°</span>
                        <span>{tr("vent", "wind")} {Math.round(wx.now.wind)} km/h</span>
                        <span className="inline-flex items-center gap-1"><Sunrise className="size-3" />{clock(t.sunrise)}</span>
                        <span className="inline-flex items-center gap-1"><Sunset className="size-3" />{clock(t.sunset)}</span>
                      </div>
                    </div>
                  </div>
                  {(aq.ok || (wt.ok && wt.data.length > 0)) && (
                    <div className="mt-4 flex flex-wrap gap-x-4 gap-y-1.5 text-xs text-ink-2">
                      {aq.ok && q && (
                        <span className="inline-flex items-center gap-1.5" title={`PM2.5 ${nf(aq.data.pm25, 1)} µg/m³ · PM10 ${nf(aq.data.pm10, 1)} µg/m³ · ${tr("ozone", "ozone")} ${Math.round(aq.data.ozone)} µg/m³`}>
                          <span className="size-1.5 rounded-full" style={{ background: q.color }} />
                          {tr("Air", "Air")} {q.label.toLowerCase()} <span className="text-ink-3 tabular">{aq.data.aqi}</span>
                        </span>
                      )}
                      {aq.ok && (
                        <span className="inline-flex items-center gap-1.5">
                          <span className={cn("size-1.5 rounded-full", aq.data.uvMax >= 6 ? "bg-bad" : aq.data.uvMax >= 3 ? "bg-warn" : "bg-good")} />
                          UV max {Math.round(aq.data.uvMax)}
                          {aq.data.uvMax >= 6 && <span className="text-ink-3">{tr("· crème solaire", "· sunscreen")}</span>}
                        </span>
                      )}
                      {aq.ok &&
                        aq.data.pollen.map((p) => (
                          <span key={p.label} className="inline-flex items-center gap-1.5">
                            <span className={cn("size-1.5 rounded-full", p.value >= 70 ? "bg-bad" : p.value >= 20 ? "bg-warn" : "bg-good")} />
                            {p.label} {pollenLevel(p.value)}
                          </span>
                        ))}
                      {wt.ok &&
                        wt.data.map((x) => (
                          <span key={x.station} className="inline-flex items-center gap-1.5" title={`${x.water} · ${x.station} · ${tr("relevé", "measured")} ${ago(x.at)}`}>
                            <span className="size-1.5 rounded-full bg-ink-3" />
                            {x.water}
                            {x.temp != null && <span className="text-ink tabular">{nf(x.temp, 1)}°</span>}
                            {x.level && x.flow != null && <span className="text-ink-3 tabular">{nf(x.flow, 2)} m</span>}
                          </span>
                        ))}
                    </div>
                  )}
                  <div className="mt-4 grid grid-cols-5 gap-1 border-t border-line pt-3">
                    {wx.days.slice(1, 6).map((day) => (
                      <div key={day.date} className="flex flex-col items-center gap-1 text-center" title={`${describe(day.code).label} · ${tr("pluie", "rain")} ${day.rain}${tr(" %", "%")}`}>
                        <span className="text-2xs text-ink-3">{dayLabel(day.date, { weekday: "short" })}</span>
                        <WeatherIcon code={day.code} className="size-4 text-ink-2" strokeWidth={1.75} />
                        <span className="text-xs tabular">
                          <span className="font-medium text-ink">{Math.round(day.max)}°</span> <span className="text-ink-3">{Math.round(day.min)}°</span>
                        </span>
                        <span className={cn("h-3.5 text-2xs tabular", day.rain >= 40 ? "text-ink-2" : "text-transparent")}>{day.rain}{tr(" %", "%")}</span>
                      </div>
                    ))}
                  </div>
                </>
              );
            }}
          </Gate>
        )}
      </div>
      <dl className="divide-y divide-line border-line px-5 py-2 max-lg:border-t lg:border-l">
        {facts.map((f) => (
          <div key={f.label} className="flex min-h-10 items-baseline gap-4 py-2.5 text-[13px]">
            <dt className="w-24 shrink-0 text-xs text-ink-3">{f.label}</dt>
            <dd className="min-w-0 flex-1">{f.value}</dd>
          </div>
        ))}
      </dl>
    </section>
  );
}
