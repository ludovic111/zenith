import { Suspense } from "react";
import type { Metadata } from "next";
import { ArrowUpRight, BellRing, Cake, CalendarDays, CheckSquare, CircleAlert, Disc3, Droplets, Flower2, Landmark, Mail, MapPin, Monitor, Moon, NotebookPen, Package, Sun as SunIcon, Sunrise, Sunset, Tag, TrainFront, Wind } from "lucide-react";
import { source } from "@/lib/source";
import { config } from "@/lib/config";
import { l10n, plural, tr } from "@/lib/i18n";
import { ago, base, date, money, nf } from "@/lib/format";
import { describe, weather } from "@/lib/sources/weather";
import { life, rhythm, type Life, type LifeEvent } from "@/lib/sources/life";
import { urgent, SUBSCRIPTIONS } from "@/lib/subscriptions";
import { apple, screenDays, upcomingBirthdays, type AppleSnapshot } from "@/lib/sources/apple";
import { air, aqiLabel, nextHolidays, pollenLevel, water } from "@/lib/sources/environment";
import { departures, transitStop } from "@/lib/sources/transit";
import { notes, todo } from "@/lib/sources/obsidian";
import { Panel, Skeleton, Empty } from "@/components/z/panel";
import { Gate } from "@/components/z/gate";
import { Stat } from "@/components/z/stat";
import { Bars } from "@/components/charts/bars";
import { HBars } from "@/components/charts/hbars";
import { Meteors } from "@/components/ui/meteors";
import { cn } from "@/lib/utils";

export const dynamic = "force-dynamic";
export function generateMetadata(): Metadata {
  return { title: tr("Ma vie", "My day") };
}

/** Day (YYYY-MM-DD) of an instant, in your time zone. */
const dayKey = (t: string | number) => new Intl.DateTimeFormat("en-CA", { timeZone: l10n().timeZone }).format(new Date(t));
/** Hour and minute of an instant, in your time zone. */
const hm = (t: string) => new Intl.DateTimeFormat(l10n().locale, { timeZone: l10n().timeZone, hour: "2-digit", minute: "2-digit" }).format(new Date(t));
/** Formats a calendar day (YYYY-MM-DD) without time zone drift. */
const dayLabel = (ymd: string, opts: Intl.DateTimeFormatOptions) => new Intl.DateTimeFormat(l10n().locale, { timeZone: "UTC", ...opts }).format(new Date(`${ymd.slice(0, 10)}T12:00:00Z`));
/** Formats a local wall-clock time without offset ("2026-01-31T07:24", as Open-Meteo gives it). */
const clock = (local: string) => new Intl.DateTimeFormat(l10n().locale, { timeZone: "UTC", hour: "2-digit", minute: "2-digit" }).format(new Date(`${local.slice(0, 16)}Z`));
/** Flag emoji of an ISO country code. */
const flag = (cc: string) => cc.toUpperCase().replace(/[A-Z]/g, (c) => String.fromCodePoint(127397 + c.charCodeAt(0)));

export default function Vie() {
  return (
    <>
      <Suspense fallback={<Skeleton className="mb-8 h-72" />}>
        <Hero />
      </Suspense>
      <Suspense fallback={<Skeleton className="h-96" />}>
        <Body />
      </Suspense>
    </>
  );
}

async function Hero() {
  const [w, snap, aq, wt, ap] = await Promise.all([source(weather), source(life), source(air), source(water), source(apple)]);
  const loc = config().location;
  const current = ap.ok ? ap.data?.music?.current : null;
  // The snapshot is written every 5 minutes: after 10, the track is surely over.
  const playing = current && Date.now() - new Date(current.at).getTime() < 10 * 60e3 ? current : null;
  const today = new Date();
  const l = snap.ok ? snap.data : null;
  const next = l?.agenda.find((e) => new Date(e.end ?? e.start).getTime() > Date.now());
  return (
    <header className="relative mb-8 overflow-hidden rounded-[2rem] border border-line px-6 py-8 sm:px-10 sm:py-10">
      <div aria-hidden className="absolute inset-0" style={{ background: "radial-gradient(110% 140% at 0% 0%, #FFD16633 0%, transparent 55%), radial-gradient(90% 130% at 100% 100%, #00D2FF26 0%, transparent 60%)" }} />
      <div aria-hidden className="absolute inset-0 overflow-hidden opacity-50"><Meteors number={8} /></div>
      <div className="relative grid gap-8 lg:grid-cols-[1.2fr_1fr] lg:items-end">
        <div>
          <div className="font-mono text-xs uppercase tracking-[0.25em] text-ink-3">
            {date(today, { weekday: "long", day: "numeric", month: "long" })}
            {loc ? ` · ${loc.name}` : ""}
          </div>
          <h1 className="mt-3 font-display text-5xl font-black tracking-tight sm:text-7xl">{tr("Ma vie", "My day")}</h1>
          <p className="mt-2 max-w-xl font-serif text-xl italic text-ink-2 sm:text-2xl">
            {next
              ? next.allDay
                ? tr(`Prochain rendez-vous : ${next.title}, ${date(next.start, { weekday: "long", day: "numeric", month: "long" })}.`, `Next up: ${next.title}, ${date(next.start, { weekday: "long", day: "numeric", month: "long" })}.`)
                : tr(`Prochain rendez-vous : ${next.title}, ${date(next.start, { weekday: "long" })} à ${hm(next.start)}.`, `Next up: ${next.title}, ${date(next.start, { weekday: "long" })} at ${hm(next.start)}.`)
              : tr("Rien au programme, le ciel est à toi.", "Nothing planned, the sky is yours.")}
          </p>
          {playing && (
            <p className="mt-4 inline-flex max-w-full items-center gap-2 rounded-full border border-white/10 bg-black/30 px-3 py-1.5 text-sm text-ink-2 backdrop-blur">
              <Disc3 className="size-4 shrink-0 animate-spin text-[#B18CFF] [animation-duration:3s]" />
              <span className="truncate">{playing.title} — {playing.artist}</span>
              <span className="shrink-0 text-xs text-ink-3">{playing.app}</span>
            </p>
          )}
          <Environment air={aq.ok ? aq.data : null} water={wt.ok ? wt.data : []} />
        </div>
        {!loc ? (
          <div className="rounded-3xl border border-dashed border-white/15 bg-black/30 p-5 text-sm text-ink-2 backdrop-blur">
            {tr("Pour la météo, l'air et les jours fériés, ajoute ", "For weather, air quality and public holidays, add ")}
            <code className="font-mono text-xs text-sun">location</code>
            {tr(" (nom, latitude, longitude, pays) dans ", " (name, latitude, longitude, country) to ")}
            <code className="font-mono text-xs">zenith.config.json</code>.
          </div>
        ) : (
        <Gate src={w}>
          {(wx) => {
            const d = describe(wx.now.code, wx.now.isDay);
            const t = wx.days[0];
            return (
              <div className="rounded-3xl border border-white/10 bg-black/30 p-5 backdrop-blur">
                <div className="flex items-center gap-4">
                  <span className="text-6xl leading-none">{d.icon}</span>
                  <div>
                    <div className="font-display text-5xl tabular">{Math.round(wx.now.temp)}°</div>
                    <div className="text-sm text-ink-2">
                      {d.label} · {tr("ressenti", "feels like")} {Math.round(wx.now.feels)}° · {tr("vent", "wind")} {Math.round(wx.now.wind)} km/h
                    </div>
                  </div>
                </div>
                <div className="mt-4 flex gap-4 text-xs text-ink-3">
                  <span className="inline-flex items-center gap-1"><Sunrise className="size-3.5" /> {clock(t.sunrise)}</span>
                  <span className="inline-flex items-center gap-1"><Sunset className="size-3.5" /> {clock(t.sunset)}</span>
                  <span>{Math.round(t.min)}° / {Math.round(t.max)}°</span>
                </div>
                <div className="mt-4 grid grid-cols-5 gap-2 border-t border-line pt-4">
                  {wx.days.slice(1, 6).map((day) => {
                    const dd = describe(day.code);
                    return (
                      <div key={day.date} className="text-center" title={`${dd.label} · ${tr("pluie", "rain")} ${day.rain}${tr(" %", "%")}`}>
                        <div className="text-[11px] uppercase text-ink-3">{dayLabel(day.date, { weekday: "short" })}</div>
                        <div className="my-1 text-xl">{dd.icon}</div>
                        <div className="font-mono text-xs text-ink">{Math.round(day.max)}°</div>
                        <div className="font-mono text-[11px] text-ink-3">{Math.round(day.min)}°</div>
                        {day.rain >= 40 && <div className="font-mono text-[10px] text-sky-300">{day.rain}{tr(" %", "%")}</div>}
                      </div>
                    );
                  })}
                </div>
              </div>
            );
          }}
        </Gate>
        )}
      </div>
    </header>
  );
}

async function Body() {
  const [snap, rh, ap, td, nt, hol] = await Promise.all([source(life), source(rhythm), source(apple), source(todo), source(notes), source(() => nextHolidays(3))]);
  const l = snap.ok ? snap.data : null;
  const a = ap.ok ? ap.data : null;
  const loc = config().location;
  const allDay = (title: string, day: string, calendar: string): LifeEvent => ({ title, start: day, end: null, allDay: true, location: null, calendar, link: null });
  const holidayLabel = loc ? tr(`Férié à ${loc.name}`, `Public holiday in ${loc.name}`) : tr("Jour férié", "Public holiday");
  // Agenda: Google (Claude's snapshot) + Apple (zenith.app) + public holidays + birthdays, without duplicate titles at the same time.
  const events = [
    ...(l?.agenda ?? []),
    ...(a?.calendar.events ?? []).map((e) => ({ ...e, calendar: `${e.calendar} · Apple`, link: null })),
    ...(hol.ok ? hol.data.filter((h) => h.date <= dayKey(Date.now() + 14 * 864e5)).map((h) => allDay(`${loc?.country ? `${flag(loc.country)} ` : ""}${h.name}`, h.date, holidayLabel)) : []),
    ...upcomingBirthdays(a, 14).map((b) => allDay(`🎂 ${b.name}${b.age ? tr(` (${b.age} ans)`, ` (${b.age})`) : ""}`, b.date, tr("Anniversaire · Contacts", "Birthday · Contacts"))),
  ]
    .filter((e, i, all) => all.findIndex((x) => x.title === e.title && x.start.slice(0, 16) === e.start.slice(0, 16)) === i)
    .sort((x, y) => x.start.localeCompare(y.start));
  return (
    <>
      <div className="grid gap-5 xl:grid-cols-[1.3fr_1fr]">
        <Agenda events={l || a || events.length ? events : null} />
        <Todo l={l} a={a} obsidian={td.ok ? td.data : null} />
      </div>

      <div className={cn("mt-5 grid gap-5", transitStop() ? "xl:grid-cols-3" : "xl:grid-cols-2")}>
        {transitStop() && (
          <Suspense fallback={<Skeleton className="h-72" />}>
            <Departures />
          </Suspense>
        )}
        <ScreenTime a={a} />
        <Around a={a} holidays={hol.ok ? hol.data : []} />
      </div>

      {l ? (
        <div className="mt-5 grid gap-5 xl:grid-cols-3">
          <Panel kicker={tr("Boîte mail", "Inbox")} title={tr("Attend une réponse de toi", "Waiting for your reply")} accent="#FFD166">
            <div className="mb-4 flex gap-5 text-sm">
              <span><span className="font-display text-2xl tabular">{l.inbox.unread}</span> <span className="text-ink-3">{plural(l.inbox.unread, ["non lu", "non lus"], ["unread", "unread"])}</span></span>
              <span><span className="font-display text-2xl tabular">{l.inbox.unreadImportant}</span> <span className="text-ink-3">{plural(l.inbox.unreadImportant, ["important", "importants"], ["important", "important"])}</span></span>
            </div>
            {a?.mail.running && a.mail.accounts.length > 0 && (
              <div className="mb-4 flex flex-wrap gap-2 text-xs">
                {a.mail.accounts.map((m) => (
                  <span key={m.name} className="rounded-full border border-line px-2.5 py-1 text-ink-2">
                    {m.name} · <span className="font-mono text-ink">{m.unread < 0 ? "?" : m.unread}</span>
                  </span>
                ))}
              </div>
            )}
            {l.inbox.needsReply.length ? (
              <ul className="space-y-3">
                {l.inbox.needsReply.map((m) => (
                  <li key={m.link + m.subject}>
                    <a href={m.link} target="_blank" rel="noopener noreferrer" className="group flex gap-3 text-sm">
                      <Mail className="mt-0.5 size-4 shrink-0 text-sun" />
                      <div className="min-w-0 flex-1">
                        <div className="truncate text-ink group-hover:underline">{m.from}</div>
                        <div className="truncate text-xs text-ink-2">{m.subject}</div>
                        <div className="text-xs text-ink-3">{m.why} · {ago(m.date)}</div>
                      </div>
                    </a>
                  </li>
                ))}
              </ul>
            ) : (
              <Empty>{tr("Personne n'attend après toi.", "Nobody is waiting on you.")}</Empty>
            )}
          </Panel>

          <Panel kicker={tr("En route", "On the way")} title={tr("Colis & ventes", "Parcels & sales")} accent="#FFD166">
            <div className="space-y-5">
              <ul className="space-y-3">
                {l.deliveries.map((d) => (
                  <li key={d.link + d.merchant}>
                    <a href={d.link} target="_blank" rel="noopener noreferrer" className="flex gap-3 text-sm hover:underline">
                      <Package className="mt-0.5 size-4 shrink-0 text-sky-300" />
                      <div className="min-w-0">
                        <div className="text-ink">{d.merchant}</div>
                        <div className="text-xs text-ink-3">{d.status}{d.eta ? ` · ${date(d.eta, { weekday: "short", day: "numeric", month: "short" })}` : ""}</div>
                      </div>
                    </a>
                  </li>
                ))}
                {!l.deliveries.length && <li className="text-sm text-ink-3">{tr("Aucun colis en route.", "No parcel on the way.")}</li>}
              </ul>
              {l.sales.length > 0 && (
                <ul className="space-y-3 border-t border-line pt-4">
                  {l.sales.map((s) => (
                    <li key={s.link + s.item}>
                      <a href={s.link} target="_blank" rel="noopener noreferrer" className="flex gap-3 text-sm hover:underline">
                        <Tag className="mt-0.5 size-4 shrink-0 text-good" />
                        <div className="min-w-0">
                          <div className="truncate text-ink">{s.item}</div>
                          <div className="text-xs text-ink-3">
                            {tr(
                              `${s.platform} · ${s.messages} message${s.messages > 1 ? "s" : ""} d'acheteurs · dernier ${ago(s.lastMessageAt)}`,
                              `${s.platform} · ${s.messages} buyer message${s.messages === 1 ? "" : "s"} · last ${ago(s.lastMessageAt)}`,
                            )}
                          </div>
                        </div>
                      </a>
                    </li>
                  ))}
                </ul>
              )}
            </div>
          </Panel>

          <Spending l={l} />
        </div>
      ) : (
        <div className="mt-5">
          <Panel kicker="Gmail · Google Agenda" title={tr("Boîte mail, colis, dépenses & démarches", "Inbox, parcels, spending & paperwork")} accent="#FFD166">
            <NoSnapshot />
          </Panel>
        </div>
      )}

      <div className={cn("mt-5 grid gap-5", l && "xl:grid-cols-[1.6fr_1fr]")}>
        <Panel kicker={tr("Rythme", "Rhythm")} title={tr("Ton temps sur les projets", "Your time on projects")} accent="#B18CFF">
          <Gate src={rh}>
            {(r) => (
              <>
                <div className="mb-5 grid grid-cols-2 gap-5 sm:grid-cols-4">
                  <Stat label={tr("Cette semaine", "This week")} value={r.weekHours} suffix=" h" hint={tr("avec un agent au travail", "with an agent at work")} />
                  <Stat label="Commits" value={r.weekCommits} hint={tr("7 derniers jours", "last 7 days")} />
                  <Stat
                    label={tr("Après minuit", "After midnight")}
                    value={r.nightHours}
                    suffix=" h"
                    hint={r.lastLate ? tr(`dernier commit nocturne ${ago(r.lastLate)}`, `last late-night commit ${ago(r.lastLate)}`) : tr("aucune nuit blanche", "no all-nighter")}
                  />
                  <Stat label={tr("Jours off", "Days off")} value={r.daysOff} suffix=" / 7" hint={r.daysOff === 0 ? tr("pense à souffler", "take a breather") : tr("bien joué", "well done")} />
                </div>
                <Bars data={r.days.map((d) => ({ label: dayLabel(d.date, { weekday: "short", day: "numeric" }), value: d.hours }))} color="#8F5CFF" height={130} />
                <p className="mt-3 text-xs text-ink-3">
                  {tr(
                    "Tranches de 15 minutes où Claude Code ou Codex a réellement travaillé (sessions en parallèle comptées une fois), sur 14 jours.",
                    "15-minute slots where Claude Code or Codex really worked (parallel sessions counted once), over 14 days.",
                  )}
                </p>
              </>
            )}
          </Gate>
        </Panel>

        {l && (
          <Panel kicker={tr("Obligations", "Obligations")} title={tr("Administratif & engagements", "Paperwork & commitments")} accent="#FFD166">
            {l.civic.length ? (
              <ul className="space-y-3">
                {l.civic.map((c) => (
                  <li key={c.link + c.title}>
                    <a href={c.link} target="_blank" rel="noopener noreferrer" className="flex gap-3 text-sm hover:underline">
                      <Landmark className="mt-0.5 size-4 shrink-0 text-ink-3" />
                      <div className="min-w-0">
                        <div className="text-ink">{c.title}</div>
                        <div className="text-xs text-ink-3">{c.date ? `${date(c.date, { day: "numeric", month: "long" })} · ` : ""}{c.note}</div>
                      </div>
                    </a>
                  </li>
                ))}
              </ul>
            ) : (
              <Empty>{tr("Rien d'administratif en cours.", "No paperwork in progress.")}</Empty>
            )}
            {l.notes.length > 0 && (
              <ul className="mt-5 space-y-1.5 border-t border-line pt-4 text-sm text-ink-2">
                {l.notes.map((n) => <li key={n}>· {n}</li>)}
              </ul>
            )}
          </Panel>
        )}
      </div>

      <div className="mt-5">
        <Panel kicker="Obsidian" title={nt.ok ? `${tr("Carnet", "Notebook")} · ${nt.data.vault}` : tr("Carnet", "Notebook")} accent="#B18CFF">
          <Gate src={nt}>
            {(v) => (
              <div className="grid gap-x-8 gap-y-2 md:grid-cols-2">
                {v.notes.slice(0, 12).map((n) => (
                  <a key={n.path} href={n.url} className="group flex gap-3 rounded-xl px-2 py-2 text-sm hover:bg-white/[0.03]">
                    <NotebookPen className="mt-0.5 size-4 shrink-0 text-[#B18CFF]" />
                    <div className="min-w-0 flex-1">
                      <div className="truncate text-ink group-hover:underline">{n.title}</div>
                      <div className="truncate text-xs text-ink-3">{n.folder ? `${n.folder} · ` : ""}{ago(n.modified)}{n.excerpt ? ` · ${n.excerpt}` : ""}</div>
                    </div>
                  </a>
                ))}
              </div>
            )}
          </Gate>
          <p className="mt-4 text-xs text-ink-3">
            {tr(
              `Un clic ouvre la note dans Obsidian. zenith écrit aussi son propre résumé dans le dossier « ${config().obsidian.exportDir} » du vault, mis à jour toutes les 10 minutes.`,
              `A click opens the note in Obsidian. zenith also writes its own summary to the "${config().obsidian.exportDir}" folder of the vault, updated every 10 minutes.`,
            )}
          </p>
        </Panel>
      </div>

      <p className="mt-6 text-center text-xs text-ink-3">
        {l
          ? tr(`Agenda Google et Gmail relevés ${ago(l.capturedAt)} par Claude`, `Google Calendar and Gmail read ${ago(l.capturedAt)} by Claude`)
          : tr("Agenda Google et Gmail pas encore relevés", "Google Calendar and Gmail not read yet")}
        {tr(" — demande « mets à jour ma vie dans zenith ». ", ' — ask "update my life in zenith". ')}
        {a
          ? tr(`Calendrier, Rappels, Mail, Contacts, musique et temps d'écran relevés par zenith.app ${ago(a.capturedAt)}.`, `Calendar, Reminders, Mail, Contacts, music and screen time read by zenith.app ${ago(a.capturedAt)}.`)
          : tr("Ouvre zenith.app pour brancher Calendrier, Rappels, Mail, Contacts, musique et temps d'écran.", "Open zenith.app to connect Calendar, Reminders, Mail, Contacts, music and screen time.")}{" "}
        {liveList()}
      </p>
    </>
  );
}

/** What this page reads live, given the config. */
function liveList() {
  const c = config();
  const items = [
    ...(c.location ? [tr("Météo", "Weather"), tr("air", "air"), tr("pollens", "pollen")] : []),
    ...(c.water?.stations.length ? [tr("rivières et lacs", "rivers and lakes")] : []),
    ...(transitStop() ? [tr("transports", "transit")] : []),
    ...(c.location?.country ? [tr("fériés", "public holidays")] : []),
    "Obsidian",
    tr("rythme", "rhythm"),
  ];
  const list = items.length > 1 ? `${items.slice(0, -1).join(", ")}${tr(" et ", " and ")}${items.at(-1)}` : items[0];
  return tr(`${list[0].toUpperCase()}${list.slice(1)} en direct.`, `${list[0].toUpperCase()}${list.slice(1)} live.`);
}

function NoSnapshot() {
  return (
    <Empty>
      {tr("Pas encore de relevé : demande à Claude « mets à jour ma vie dans zenith ».", 'No snapshot yet: ask Claude "update my life in zenith" (see docs/releves.md).')}
    </Empty>
  );
}

function Agenda({ events }: { events: LifeEvent[] | null }) {
  const now = Date.now();
  const upcoming = (events ?? []).filter((e) => new Date(e.end ?? e.start).getTime() >= now - 3600e3);
  const groups = new Map<string, LifeEvent[]>();
  for (const e of upcoming) {
    const k = dayKey(e.start);
    groups.set(k, [...(groups.get(k) ?? []), e]);
  }
  const todayKey = dayKey(now);
  const tomorrowKey = dayKey(now + 864e5);
  return (
    <Panel kicker={tr("Agenda", "Calendar")} title={tr("Les deux prochaines semaines", "The next two weeks")} accent="#00D2FF">
      {events == null ? (
        <NoSnapshot />
      ) : !upcoming.length ? (
        <Empty>{tr("Agenda vide. Profite.", "Nothing on the calendar. Enjoy.")}</Empty>
      ) : (
        <div className="space-y-5">
          {[...groups.entries()].slice(0, 8).map(([k, list]) => (
            <div key={k}>
              <div className={cn("mb-2 text-[11px] uppercase tracking-[0.18em]", k === todayKey ? "text-sun" : "text-ink-3")}>
                {k === todayKey ? tr("Aujourd'hui", "Today") : k === tomorrowKey ? tr("Demain", "Tomorrow") : dayLabel(k, { weekday: "long", day: "numeric", month: "long" })}
              </div>
              <ul className="space-y-2">
                {list.map((e) => {
                  const past = new Date(e.end ?? e.start).getTime() < now;
                  return (
                    <li key={e.title + e.start} className={cn("flex gap-3 text-sm", past && "opacity-50")}>
                      <span className="w-12 shrink-0 font-mono text-xs text-ink-2">{e.allDay ? tr("jour", "all day") : hm(e.start)}</span>
                      <span className="mt-1.5 size-1.5 shrink-0 rounded-full bg-sky-300" />
                      <div className="min-w-0 flex-1">
                        {e.link ? (
                          <a href={e.link} target="_blank" rel="noopener noreferrer" className="text-ink hover:underline">{e.title}</a>
                        ) : (
                          <span className="text-ink">{e.title}</span>
                        )}
                        <div className="flex flex-wrap gap-x-3 text-xs text-ink-3">
                          {!e.allDay && e.end && <span>{tr("jusqu'à", "until")} {hm(e.end)}</span>}
                          {e.location && <span className="inline-flex items-center gap-1 truncate"><MapPin className="size-3" />{e.location}</span>}
                          <span>{e.calendar}</span>
                        </div>
                      </div>
                    </li>
                  );
                })}
              </ul>
            </div>
          ))}
        </div>
      )}
    </Panel>
  );
}

/** Everything that waits for an action, projects and life mixed, most urgent first. */
function Todo({ l, a, obsidian }: { l: Life | null; a: AppleSnapshot | null; obsidian: Awaited<ReturnType<typeof todo>> }) {
  const soon = (iso: string | null, days: number) => iso != null && new Date(iso).getTime() - Date.now() < days * 864e5 && new Date(iso).getTime() > Date.now();
  const items: { icon: typeof CircleAlert; tone: string; text: string; href?: string }[] = [
    ...urgent().map((s) => ({ icon: CircleAlert, tone: "text-bad", text: `${s.name}${tr(" : ", ": ")}${s.evidence}`, href: s.manage_url })),
    ...(a?.reminders.items ?? [])
      .filter((r) => !r.due || new Date(r.due).getTime() < Date.now() + 7 * 864e5)
      .slice(0, 8)
      .map((r) => ({ icon: BellRing, tone: r.due && new Date(r.due).getTime() < Date.now() ? "text-bad" : "text-sky-300", text: `${r.title}${r.due ? ` · ${date(r.due, { day: "numeric", month: "short" })}` : ""} (${tr("Rappels", "Reminders")})` })),
    ...(obsidian?.items.filter((i) => !i.done) ?? []).map((i) => ({ icon: CheckSquare, tone: "text-[#B18CFF]", text: i.text, href: obsidian!.note.url })),
    ...(l?.inbox.needsReply.length
      ? [
          {
            icon: Mail,
            tone: "text-sun",
            text: tr(
              `${l.inbox.needsReply.length} e-mail${l.inbox.needsReply.length > 1 ? "s" : ""} attendent ta réponse`,
              `${l.inbox.needsReply.length} email${l.inbox.needsReply.length > 1 ? "s" : ""} waiting for your reply`,
            ),
          },
        ]
      : []),
    ...(l?.sales ?? [])
      .filter((s) => s.messages > 0)
      .map((s) => ({ icon: Tag, tone: "text-good", text: tr(`${s.item} : ${s.messages} acheteur(s) sur ${s.platform}`, `${s.item}: ${s.messages} buyer(s) on ${s.platform}`), href: s.link })),
    ...(l?.civic ?? []).filter((c) => soon(c.date, 30)).map((c) => ({ icon: Landmark, tone: "text-ink-2", text: `${c.title} · ${c.note}`, href: c.link })),
    ...SUBSCRIPTIONS.filter((s) => s.status === "active" && soon(s.next_renewal, 10) && (s.amount ?? 0) > 0).map((s) => ({
      icon: CalendarDays,
      tone: "text-ink-3",
      text: tr(
        `Prélèvement ${s.name} le ${date(s.next_renewal!, { day: "numeric", month: "short" })} (${s.amount} ${s.currency})`,
        `${s.name} charged on ${date(s.next_renewal!, { day: "numeric", month: "short" })} (${money(s.amount!, s.currency)})`,
      ),
      href: "/abonnements",
    })),
  ];
  return (
    <Panel kicker={tr("À faire", "To do")} title={tr("Ce qui attend une action", "What needs an action")} accent="#fb5a6b">
      {items.length ? (
        <ul className="space-y-2.5">
          {items.map((it) => {
            const Icon = it.icon;
            const body = (
              <>
                <Icon className={cn("mt-0.5 size-4 shrink-0", it.tone)} />
                <span className="min-w-0 flex-1 text-ink-2">{it.text}</span>
                {it.href && <ArrowUpRight className="size-3.5 shrink-0 text-ink-3" />}
              </>
            );
            return (
              <li key={it.text} className="text-sm">
                {it.href ? (
                  <a href={it.href} target={it.href.startsWith("/") ? undefined : "_blank"} rel="noopener noreferrer" className="flex gap-3 hover:text-ink">{body}</a>
                ) : (
                  <div className="flex gap-3">{body}</div>
                )}
              </li>
            );
          })}
        </ul>
      ) : (
        <Empty>{tr("Rien à faire. Vraiment rien.", "Nothing to do. Really nothing.")}</Empty>
      )}
    </Panel>
  );
}

function Spending({ l }: { l: Life | null }) {
  const [cur, prev] = l?.spending.months ?? [];
  // The snapshot says which currency it counts in; else, yours.
  const currency = l?.spending.currency || l10n().currency;
  const fmt = (n: number) => (currency === l10n().currency ? base(n, 0) : money(n, currency, 0));
  const total = (m?: { categories: { amount: number }[] }) => (m ? m.categories.reduce((a, c) => a + c.amount, 0) : 0);
  const monthName = (m: string) => dayLabel(`${m}-15`, { month: "long" });
  return (
    <Panel
      kicker={tr("Dépenses perso", "Personal spending")}
      title={cur ? tr(`En ${monthName(cur.month)}, hors abonnements`, `In ${monthName(cur.month)}, subscriptions aside`) : tr("Hors abonnements", "Subscriptions aside")}
      accent="#FFD166"
    >
      {!l ? (
        <NoSnapshot />
      ) : !cur ? (
        <Empty>{tr("Aucun reçu ce mois-ci.", "No receipt this month.")}</Empty>
      ) : (
        <>
          <div className="mb-4 flex items-baseline gap-3">
            <span className="font-display text-3xl tabular">{fmt(total(cur))}</span>
            {prev && (
              <span className={cn("text-sm", total(cur) > total(prev) ? "text-warn" : "text-good")}>
                {total(cur) > total(prev) ? "+" : "−"}
                {fmt(Math.abs(total(cur) - total(prev)))} vs {monthName(prev.month)}
              </span>
            )}
          </div>
          <HBars
            rows={[...cur.categories].sort((a, b) => b.amount - a.amount).map((c) => ({ label: c.label, value: Math.round(c.amount), key: c.label, hint: `${c.count}×` }))}
            color="#FFD166"
            format={fmt}
          />
          <p className="mt-4 flex items-center gap-1.5 text-xs text-ink-3">
            <Moon className="size-3" /> {tr("D'après les reçus reçus par e-mail (Uber Eats, Uber, boutiques…).", "From receipts received by email (food delivery, rides, shops…).")}
          </p>
        </>
      )}
    </Panel>
  );
}

function Environment({ air: a, water: w }: { air: Awaited<ReturnType<typeof air>> | null; water: Awaited<ReturnType<typeof water>> }) {
  if (!a && !w.length) return null;
  const q = a ? aqiLabel(a.aqi) : null;
  const chip = "inline-flex items-center gap-1.5 rounded-full border border-white/10 bg-black/30 px-3 py-1 text-xs text-ink-2 backdrop-blur";
  return (
    <div className="mt-5 flex flex-wrap gap-2">
      {a && q && (
        <span className={chip} title={`${tr("PM2,5", "PM2.5")} ${nf(a.pm25, 1)} µg/m³ · PM10 ${nf(a.pm10, 1)} µg/m³ · ${tr("ozone", "ozone")} ${Math.round(a.ozone)} µg/m³`}>
          <Wind className="size-3.5" style={{ color: q.color }} /> {tr(`Air ${q.label.toLowerCase()}`, `Air: ${q.label.toLowerCase()}`)} <span className="font-mono text-ink-3">{a.aqi}</span>
        </span>
      )}
      {a && a.uvMax >= 3 && (
        <span className={chip}>
          <SunIcon className="size-3.5 text-sun" /> UV max {Math.round(a.uvMax)}{a.uvMax >= 6 ? tr(" · crème solaire", " · sunscreen") : ""}
        </span>
      )}
      {a?.pollen.map((p) => (
        <span key={p.label} className={chip}>
          <Flower2 className={cn("size-3.5", p.value >= 70 ? "text-bad" : p.value >= 20 ? "text-warn" : "text-good")} /> {p.label} {pollenLevel(p.value)}
        </span>
      ))}
      {w.map((x) => (
        <span key={x.station} className={chip} title={`${x.water} · ${x.station} · ${tr("relevé", "measured")} ${ago(x.at)}`}>
          <Droplets className="size-3.5 text-sky-300" /> {x.water}
          {x.temp != null && <span className="font-mono text-ink">{nf(x.temp, 1)}°</span>}
          {x.level && x.flow != null && <span className="font-mono text-ink-3">{nf(x.flow, 2)} m</span>}
        </span>
      ))}
    </div>
  );
}

async function Departures() {
  const b = await source(() => departures());
  return (
    <Panel kicker={tr("Transports", "Transit")} title={b.ok ? tr(`Départs · ${b.data.station}`, `Departures · ${b.data.station}`) : tr("Prochains départs", "Next departures")} accent="#fb5a6b">
      <Gate src={b}>
        {(board) => (
          <ul className="space-y-2">
            {board.departures.slice(0, 8).map((d) => (
              <li key={d.line + d.to + d.at} className="flex items-center gap-3 text-sm">
                <span className="w-12 shrink-0 font-mono text-xs text-ink">{hm(d.at)}</span>
                <span className="min-w-12 shrink-0 rounded-md bg-white/[0.07] px-1.5 py-0.5 text-center font-mono text-[11px] text-ink">{d.line}</span>
                <span className="min-w-0 flex-1 truncate text-ink-2">{d.to}</span>
                {d.delay > 0 && <span className="shrink-0 font-mono text-xs text-bad">+{d.delay}′</span>}
                {d.platform && <span className="shrink-0 font-mono text-[11px] text-ink-3">{tr("voie", "pl.")} {d.platform}</span>}
              </li>
            ))}
          </ul>
        )}
      </Gate>
      <p className="mt-4 flex items-center gap-1.5 text-xs text-ink-3"><TrainFront className="size-3" />{" "}
        {tr(
          "Transports publics suisses en direct (transport.opendata.ch). Choisis ton arrêt dans Sources de données.",
          "Swiss public transport, live (transport.opendata.ch). Pick your stop in Data sources.",
        )}
      </p>
    </Panel>
  );
}

const hours = (s: number) => (s >= 3600 ? `${Math.floor(s / 3600)} h ${String(Math.round((s % 3600) / 60)).padStart(2, "0")}` : `${Math.round(s / 60)} min`);

function ScreenTime({ a }: { a: AppleSnapshot | null }) {
  const days = screenDays(a);
  const today = days.find((d) => d.date === dayKey(Date.now()));
  return (
    <Panel kicker={tr("Écran", "Screen")} title={tr("Temps devant le Mac", "Time in front of the Mac")} accent="#B18CFF">
      {!a?.screen ? (
        <Empty>
          {tr(
            "zenith.app mesure le temps passé dans chaque app dès sa prochaine installation (npm run mac:install).",
            "zenith.app measures the time spent in each app once installed (npm run mac:install).",
          )}
        </Empty>
      ) : !days.length ? (
        <Empty>{tr("Mesure en cours, reviens dans quelques minutes.", "Measuring, come back in a few minutes.")}</Empty>
      ) : (
        <>
          <div className="mb-4 flex items-baseline gap-3">
            <span className="font-display text-3xl tabular">{hours(today?.total ?? 0)}</span>
            <span className="text-sm text-ink-3">{tr("aujourd'hui, à être vraiment actif", "today, actually active")}</span>
          </div>
          {today && (
            <HBars
              rows={today.apps.slice(0, 6).map((x) => ({ label: x.name, value: Math.round(x.seconds / 60), key: x.name }))}
              color="#B18CFF"
              format={(n) => hours(n * 60)}
            />
          )}
          {days.length > 1 && (
            <div className="mt-4 border-t border-line pt-4">
              <Bars data={days.slice(-7).map((d) => ({ label: dayLabel(d.date, { weekday: "short" }), value: Math.round((d.total / 3600) * 10) / 10 }))} color="#8F5CFF" height={70} />
            </div>
          )}
          <p className="mt-3 flex items-center gap-1.5 text-xs text-ink-3"><Monitor className="size-3" />{" "}
            {tr(
              "App au premier plan toutes les 20 s, hors écran verrouillé et après 3 min sans clavier ni souris.",
              "Foreground app every 20 s, except when locked or after 3 min without keyboard or mouse.",
            )}
          </p>
        </>
      )}
    </Panel>
  );
}

function Around({ a, holidays }: { a: AppleSnapshot | null; holidays: { date: string; name: string }[] }) {
  const birthdays = upcomingBirthdays(a, 30);
  const music = a?.music?.recent ?? [];
  const when = (n: number) => (n === 0 ? tr("aujourd'hui", "today") : n === 1 ? tr("demain", "tomorrow") : tr(`dans ${n} j`, `in ${n} d`));
  return (
    <Panel kicker={tr("À venir", "Coming up")} title={tr("Anniversaires, fériés & musique", "Birthdays, holidays & music")} accent="#FFD166">
      <div className="space-y-5">
        <div>
          {!a?.birthdays ? (
            <div className="text-sm text-ink-3">{tr("Anniversaires : zenith.app les lira dans Contacts après sa prochaine installation.", "Birthdays: zenith.app reads them from Contacts once installed.")}</div>
          ) : !a.birthdays.authorized ? (
            <div className="text-sm text-ink-3">{tr("Contacts refusés : Réglages Système → Confidentialité → Contacts → zenith.", "Contacts denied: System Settings → Privacy → Contacts → zenith.")}</div>
          ) : birthdays.length ? (
            <ul className="space-y-2">
              {birthdays.slice(0, 6).map((b) => (
                <li key={b.name + b.date} className={cn("flex items-center gap-3 text-sm", b.inDays === 0 && "text-sun")}>
                  <Cake className="size-4 shrink-0 text-sun" />
                  <span className="min-w-0 flex-1 truncate text-ink">{b.name}{b.age ? <span className="text-ink-3"> · {tr(`${b.age} ans`, `turns ${b.age}`)}</span> : null}</span>
                  <span className="shrink-0 text-xs text-ink-3">{when(b.inDays)}</span>
                </li>
              ))}
            </ul>
          ) : (
            <div className="text-sm text-ink-3">{tr("Aucun anniversaire dans les 30 jours.", "No birthday in the next 30 days.")}</div>
          )}
        </div>
        {holidays.length > 0 && (
          <ul className="space-y-2 border-t border-line pt-4">
            {holidays.map((h) => (
              <li key={h.date} className="flex items-center gap-3 text-sm">
                <CalendarDays className="size-4 shrink-0 text-bad" />
                <span className="min-w-0 flex-1 truncate text-ink-2">{h.name}</span>
                <span className="shrink-0 text-xs text-ink-3">{dayLabel(h.date, { weekday: "short", day: "numeric", month: "short" })}</span>
              </li>
            ))}
          </ul>
        )}
        {music.length > 0 && (
          <ul className="space-y-2 border-t border-line pt-4">
            {music.slice(0, 5).map((t) => (
              <li key={t.title + t.at} className="flex items-center gap-3 text-sm">
                <Disc3 className="size-4 shrink-0 text-[#B18CFF]" />
                <span className="min-w-0 flex-1 truncate text-ink-2">{t.title} <span className="text-ink-3">— {t.artist}</span></span>
                <span className="shrink-0 text-xs text-ink-3">{ago(t.at)}</span>
              </li>
            ))}
          </ul>
        )}
      </div>
    </Panel>
  );
}
