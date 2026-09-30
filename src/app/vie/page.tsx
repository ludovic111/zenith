import { Suspense } from "react";
import type { Metadata } from "next";
import { Disc3, NotebookPen } from "lucide-react";
import { source } from "@/lib/source";
import { config } from "@/lib/config";
import { l10n, tr } from "@/lib/i18n";
import { ago, base, date, money } from "@/lib/format";
import { life, rhythm } from "@/lib/sources/life";
import { apple, screenDays } from "@/lib/sources/apple";
import { departures, transitStop } from "@/lib/sources/transit";
import { notes } from "@/lib/sources/obsidian";
import { refreshLifePrompt } from "@/lib/agent/tasks";
import { agentUi } from "@/lib/agent/ui";
import { AskButton } from "@/components/agent/ask-button";
import { Empty, PageHeader, Panel, SectionTitle, Skeleton } from "@/components/z/panel";
import { Gate } from "@/components/z/gate";
import { Stat } from "@/components/z/stat";
import { Bars } from "@/components/charts/bars";
import { HBars } from "@/components/charts/hbars";
import { Today } from "@/components/life/today";
import { Waiting } from "@/components/life/waiting";
import { Agenda, Birthdays } from "@/components/life/agenda";
import { Counted, Row, Rows } from "@/components/life/rows";
import { spendingPrompt } from "@/components/life/prompts";
import { dayKey, dayLabel, hm, hours } from "@/components/life/time";
import { cn } from "@/lib/utils";

export const dynamic = "force-dynamic";
export function generateMetadata(): Metadata {
  return { title: tr("Ma vie", "My day") };
}

/** Series color: the accent, readable in both appearances. */
const SERIES = "var(--primary)";

export default function Vie() {
  const agent = agentUi().enabled;
  const loc = config().location;
  return (
    <>
      <PageHeader
        title={tr("Ma vie", "My day")}
        description={`${date(new Date(), { weekday: "long", day: "numeric", month: "long" }).replace(/^./, (c) => c.toUpperCase())}${loc ? ` · ${loc.name}` : ""}`}
        action={
          <Suspense fallback={null}>
            <Refresh agent={agent} />
          </Suspense>
        }
      />

      <Suspense fallback={<Skeleton className="h-60" />}>
        <Today />
      </Suspense>

      <div className="mt-4 grid items-start gap-4 lg:grid-cols-[1.45fr_1fr]">
        <Suspense fallback={<Skeleton className="h-96" />}>
          <Waiting agent={agent} />
        </Suspense>
        <div className="grid gap-4">
          <Suspense fallback={<Skeleton className="h-72" />}>
            <Agenda />
          </Suspense>
          <Suspense fallback={<Skeleton className="h-48" />}>
            <Birthdays agent={agent} />
          </Suspense>
          {transitStop() && (
            <Suspense fallback={<Skeleton className="h-72" />}>
              <Departures />
            </Suspense>
          )}
        </div>
      </div>

      <SectionTitle>{tr("Argent et habitudes", "Money and habits")}</SectionTitle>
      <div className="grid items-start gap-4 lg:grid-cols-3">
        <Suspense fallback={<Skeleton className="h-80" />}>
          <Spending agent={agent} />
        </Suspense>
        <Suspense fallback={<Skeleton className="h-80" />}>
          <Rhythm />
        </Suspense>
        <Suspense fallback={<Skeleton className="h-80" />}>
          <ScreenTime />
        </Suspense>
      </div>

      <SectionTitle>{tr("Autour de toi", "Around you")}</SectionTitle>
      <div className="grid items-start gap-4 lg:grid-cols-3">
        <Suspense fallback={<Skeleton className="h-72" />}>
          <Music />
        </Suspense>
        <Suspense fallback={<Skeleton className="h-72 lg:col-span-2" />}>
          <Notebook />
        </Suspense>
      </div>

      <Suspense fallback={null}>
        <Sources />
      </Suspense>
    </>
  );
}

/** When the snapshot was taken, and the one button that takes a new one. */
async function Refresh({ agent }: { agent: boolean }) {
  const snap = await source(life);
  const at = snap.ok ? snap.data?.capturedAt : null;
  const stale = !at || Date.now() - new Date(at).getTime() > 20 * 3600e3;
  return (
    <>
      <span className={cn("text-xs", stale && at ? "text-warn" : "text-ink-3")}>{at ? tr(`Relevé ${ago(at)}`, `Captured ${ago(at)}`) : tr("Jamais relevé", "Never captured")}</span>
      {agent ? (
        <AskButton prompt={refreshLifePrompt()} target="life" label={tr("Rafraîchir ma vie", "Refresh my life")} variant={stale ? "primary" : "quiet"} />
      ) : (
        <span className="text-xs text-ink-3">{tr("· demande « mets à jour ma vie dans zenith »", '· ask "update my life in zenith"')}</span>
      )}
    </>
  );
}

async function Spending({ agent }: { agent: boolean }) {
  const snap = await source(life);
  const l = snap.ok ? snap.data : null;
  const months = l?.spending.months ?? [];
  const [cur, prev] = months;
  // The snapshot says which currency it counts in; else, yours.
  const currency = l?.spending.currency || l10n().currency;
  const fmt = (n: number) => (currency === l10n().currency ? base(n, 0) : money(n, currency, 0));
  const total = (m?: { categories: { amount: number }[] }) => (m ? m.categories.reduce((a, c) => a + c.amount, 0) : 0);
  const monthName = (m: string) => dayLabel(`${m}-15`, { month: "long" });
  const shortMonth = (m: string) => dayLabel(`${m}-15`, { month: "short" });
  // Food delivery is the one line that tends to run away: call it out when it leads.
  const FOOD = /livraison|delivery|uber ?eats|deliveroo|just ?eat|smood|repas|food/i;
  const food = cur?.categories.find((c) => FOOD.test(c.label) && c.amount > 0);
  const foodPrev = prev?.categories.find((c) => FOOD.test(c.label));
  const diff = cur && prev ? total(cur) - total(prev) : 0;
  return (
    <Panel
      title={cur ? tr(`Dépenses · ${monthName(cur.month)}`, `Spending · ${monthName(cur.month)}`) : tr("Dépenses", "Spending")}
      action={agent && cur ? <AskButton prompt={spendingPrompt(monthName(cur.month))} target="life" label={tr("Analyse", "Analyze")} /> : tr("hors abonnements", "subscriptions aside")}
    >
      {!l ? (
        <Empty>{tr("Pas encore de relevé des reçus.", "No receipts captured yet.")}</Empty>
      ) : !cur ? (
        <Empty>{tr("Aucun reçu ce mois-ci.", "No receipt this month.")}</Empty>
      ) : (
        <>
          <div className="flex items-baseline gap-2.5">
            <span className="text-2xl font-semibold tracking-tight text-ink tabular">{fmt(total(cur))}</span>
            {prev && diff !== 0 && (
              <span className={cn("text-xs tabular", diff > 0 ? "text-warn" : "text-good")}>
                {diff > 0 ? "+" : "−"}
                {fmt(Math.abs(diff))} {tr("vs", "vs")} {shortMonth(prev.month)}
              </span>
            )}
          </div>
          <div className="mt-0.5 text-xs text-ink-3">{tr("hors abonnements, d'après les reçus par e-mail", "subscriptions aside, from emailed receipts")}</div>
          {food && (
            <div className="mt-4 rounded-lg border border-warn/25 bg-warn/5 px-3 py-2 text-xs text-ink-2">
              <span className="font-medium text-ink">{food.label}</span>
              {tr(
                ` : ${fmt(food.amount)} en ${food.count} commande${food.count > 1 ? "s" : ""}, ${Math.round((food.amount / Math.max(1, total(cur))) * 100)}\u00a0% du mois`,
                `: ${fmt(food.amount)} over ${food.count} order${food.count === 1 ? "" : "s"}, ${Math.round((food.amount / Math.max(1, total(cur))) * 100)}% of the month`,
              )}
              {foodPrev && foodPrev.amount > 0 && (
                <span className={food.amount > foodPrev.amount ? "text-warn" : "text-good"}>
                  {tr(` (${food.amount > foodPrev.amount ? "+" : "−"}${fmt(Math.abs(food.amount - foodPrev.amount))} vs ${shortMonth(prev!.month)})`, ` (${food.amount > foodPrev.amount ? "+" : "−"}${fmt(Math.abs(food.amount - foodPrev.amount))} vs ${shortMonth(prev!.month)})`)}
                </span>
              )}
              .
            </div>
          )}
          <HBars
            className="mt-4"
            rows={[...cur.categories].filter((c) => c.amount > 0).sort((a, b) => b.amount - a.amount).map((c) => ({ label: c.label, value: Math.round(c.amount), key: c.label, hint: `${c.count}×` }))}
            color={SERIES}
            format={fmt}
          />
          {months.length > 1 && (
            <div className="mt-4 grid border-t border-line pt-3" style={{ gridTemplateColumns: `repeat(${Math.min(months.length, 4)}, minmax(0, 1fr))` }}>
              {[...months].slice(0, 4).reverse().map((m) => (
                <div key={m.month} className="min-w-0">
                  <div className="text-2xs text-ink-3">{shortMonth(m.month)}</div>
                  <div className={cn("text-[13px] font-medium tabular", m === cur ? "text-ink" : "text-ink-2")}>{fmt(total(m))}</div>
                </div>
              ))}
            </div>
          )}
        </>
      )}
    </Panel>
  );
}

async function Rhythm() {
  const rh = await source(rhythm);
  return (
    <Panel title={tr("Rythme de travail", "Work rhythm")} action={tr("7 jours", "7 days")}>
      <Gate src={rh}>
        {(r) => (
          <>
            <div className="grid grid-cols-2 gap-x-4 gap-y-3">
              <Stat label={tr("Avec un agent", "With an agent")} value={r.weekHours} suffix=" h" />
              <Stat label="Commits" value={r.weekCommits} />
              <Stat
                label={tr("Après minuit", "After midnight")}
                value={r.nightHours}
                suffix=" h"
                hint={r.lastLate ? tr(`dernier ${ago(r.lastLate)}`, `last ${ago(r.lastLate)}`) : tr("aucune nuit blanche", "no all-nighter")}
              />
              <Stat label={tr("Jours off", "Days off")} value={r.daysOff} suffix=" / 7" hint={r.daysOff === 0 ? tr("pense à souffler", "take a breather") : tr("bien joué", "well done")} />
            </div>
            <div className="mt-4 border-t border-line pt-4">
              <Bars data={r.days.map((d) => ({ label: dayLabel(d.date, { weekday: "short", day: "numeric" }), value: d.hours }))} color={SERIES} height={84} />
              <p className="mt-2 text-2xs text-ink-3">
                {tr("Heures où Claude Code ou Codex a vraiment travaillé, sur 14 jours.", "Hours where Claude Code or Codex really worked, over 14 days.")}
              </p>
            </div>
          </>
        )}
      </Gate>
    </Panel>
  );
}

async function ScreenTime() {
  const ap = await source(apple);
  const a = ap.ok ? ap.data : null;
  const days = screenDays(a);
  const today = days.find((d) => d.date === dayKey(Date.now()));
  const week = days.slice(-7);
  const avg = week.length ? week.reduce((s, d) => s + d.total, 0) / week.length : 0;
  return (
    <Panel title={tr("Temps d'écran", "Screen time")} action={tr("ce Mac", "this Mac")}>
      {!a?.screen ? (
        <Empty>{tr("zenith.app mesure le temps passé dans chaque app une fois installé (npm run mac:install).", "zenith.app measures the time spent in each app once installed (npm run mac:install).")}</Empty>
      ) : !days.length ? (
        <Empty>{tr("Mesure en cours, reviens dans quelques minutes.", "Measuring, come back in a few minutes.")}</Empty>
      ) : (
        <>
          <div className="flex items-baseline gap-2.5">
            <span className="text-2xl font-semibold tracking-tight text-ink tabular">{hours(today?.total ?? 0)}</span>
            <span className="text-xs text-ink-3">{tr("actif aujourd'hui", "active today")}</span>
          </div>
          {week.length > 1 && <div className="mt-0.5 text-xs text-ink-3">{tr(`moyenne ${hours(avg)} par jour sur ${week.length} jours`, `average ${hours(avg)} a day over ${week.length} days`)}</div>}
          {today && today.apps.some((x) => x.seconds >= 60) && (
            <HBars className="mt-4" rows={today.apps.filter((x) => x.seconds >= 60).slice(0, 5).map((x) => ({ label: x.name, value: Math.round(x.seconds / 60), key: x.name }))} color={SERIES} format={(n) => hours(n * 60)} />
          )}
          {week.length > 2 && (
            <div className="mt-4 border-t border-line pt-4">
              <Bars data={week.map((d) => ({ label: dayLabel(d.date, { weekday: "short" }), value: Math.round((d.total / 3600) * 10) / 10 }))} color={SERIES} height={60} />
            </div>
          )}
          <p className="mt-3 text-2xs text-ink-3">{tr("Hors écran verrouillé et après 3 min sans clavier ni souris.", "Excludes locked screen and 3 min without keyboard or mouse.")}</p>
        </>
      )}
    </Panel>
  );
}

async function Departures() {
  const b = await source(() => departures());
  return (
    <Panel title={b.ok ? tr(`Départs · ${b.data.station}`, `Departures · ${b.data.station}`) : tr("Prochains départs", "Next departures")} bodyClassName="p-0 pb-1.5 pt-1">
      <Gate src={b}>
        {(board) => (
          <ul className="divide-y divide-line">
            {board.departures.slice(0, 8).map((d) => (
              <li key={d.line + d.to + d.at} className="flex h-9 items-center gap-3 px-4 text-[13px]">
                <span className="w-11 shrink-0 text-xs text-ink tabular">{hm(d.at)}</span>
                <span className="min-w-9 shrink-0 rounded-md bg-muted px-1.5 py-0.5 text-center text-2xs font-medium text-ink tabular">{d.line}</span>
                <span className="min-w-0 flex-1 truncate text-ink-2">{d.to}</span>
                {d.delay > 0 && <span className="shrink-0 text-xs text-bad tabular">+{d.delay}′</span>}
                {d.platform && <span className="shrink-0 text-2xs text-ink-3">{tr("voie", "pl.")} {d.platform}</span>}
              </li>
            ))}
          </ul>
        )}
      </Gate>
    </Panel>
  );
}

async function Music() {
  const ap = await source(apple);
  const a = ap.ok ? ap.data : null;
  const recent = a?.music?.recent ?? [];
  return (
    <Panel title={<Counted count={recent.length}>{tr("Musique récente", "Recent music")}</Counted>} bodyClassName="p-0 pb-1.5 pt-1">
      {recent.length ? (
        <Rows>
          {recent.slice(0, 7).map((t) => (
            <Row key={t.title + t.at} icon={Disc3} title={t.title} meta={`${t.artist}${t.app ? ` · ${t.app}` : ""}`} aside={ago(t.at)} />
          ))}
        </Rows>
      ) : (
        <div className="px-4 pb-3">
          <Empty>{a?.music ? tr("Rien écouté récemment.", "Nothing played lately.") : tr("zenith.app relève Musique et Spotify une fois installé.", "zenith.app reads Music and Spotify once installed.")}</Empty>
        </div>
      )}
    </Panel>
  );
}

async function Notebook() {
  const nt = await source(notes);
  return (
    <Panel className="lg:col-span-2" title={nt.ok ? tr(`Carnet · ${nt.data.vault}`, `Notebook · ${nt.data.vault}`) : tr("Carnet", "Notebook")} action="Obsidian" bodyClassName="p-0 pb-1.5 pt-1">
      <Gate src={nt}>
        {(v) =>
          v.notes.length ? (
            <Rows className="sm:grid sm:grid-cols-2 sm:divide-y-0">
              {v.notes.slice(0, 12).map((n) => (
                <Row key={n.path} icon={NotebookPen} title={n.title} href={n.url} meta={[n.folder, n.excerpt].filter(Boolean).join(" · ") || null} aside={ago(n.modified)} />
              ))}
            </Rows>
          ) : (
            <div className="px-4 pb-3">
              <Empty>{tr("Aucune note.", "No note.")}</Empty>
            </div>
          )
        }
      </Gate>
    </Panel>
  );
}

/** Where each piece of this page comes from. */
async function Sources() {
  const [snap, ap] = await Promise.all([source(life), source(apple)]);
  const l = snap.ok ? snap.data : null;
  const a = ap.ok ? ap.data : null;
  const c = config();
  const live = [
    ...(c.location ? [tr("météo, air et pollens", "weather, air and pollen")] : []),
    ...(c.water?.stations.length ? [tr("rivières et lacs", "rivers and lakes")] : []),
    ...(transitStop() ? [tr("transports", "transit")] : []),
    ...(c.location?.country ? [tr("fériés", "public holidays")] : []),
    "Obsidian",
    tr("rythme", "rhythm"),
  ];
  return (
    <p className="mt-8 text-center text-2xs leading-relaxed text-ink-3">
      {l ? tr(`Gmail et Google Agenda relevés ${ago(l.capturedAt)} par un agent.`, `Gmail and Google Calendar captured ${ago(l.capturedAt)} by an agent.`) : tr("Gmail et Google Agenda pas encore relevés.", "Gmail and Google Calendar not captured yet.")}{" "}
      {a
        ? tr(`Calendrier, Rappels, Mail, Contacts, musique et temps d'écran relevés par zenith.app ${ago(a.capturedAt)}.`, `Calendar, Reminders, Mail, Contacts, music and screen time read by zenith.app ${ago(a.capturedAt)}.`)
        : tr("Ouvre zenith.app pour brancher Calendrier, Rappels, Mail, Contacts, musique et temps d'écran.", "Open zenith.app to connect Calendar, Reminders, Mail, Contacts, music and screen time.")}{" "}
      {tr("En direct : ", "Live: ")}
      {live.join(", ")}.
    </p>
  );
}
