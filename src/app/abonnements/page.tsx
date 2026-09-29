import type { Metadata } from "next";
import { ArrowUpRight, CalendarClock, CircleAlert } from "lucide-react";
import { PROJECTS } from "@/lib/projects";
import { SUBSCRIPTIONS, CATEGORIES, monthly, type Subscription } from "@/lib/subscriptions";
import { source } from "@/lib/source";
import { base, date, money as fmt, pct } from "@/lib/format";
import { tr } from "@/lib/i18n";
import { claudePlan, codexPlan, fx } from "@/lib/sources/plans";
import { PlanCard } from "@/components/plans/plan-card";
import { billing } from "@/lib/sources/railway";
import * as rc from "@/lib/sources/revenuecat";
import { Empty, Panel } from "@/components/z/panel";
import { Stat } from "@/components/z/stat";
import { HBars } from "@/components/charts/hbars";
import { Meteors } from "@/components/ui/meteors";
import { cn } from "@/lib/utils";

export const dynamic = "force-dynamic";
export function generateMetadata(): Metadata {
  return { title: tr("Abonnements", "Subscriptions") };
}

const money = (v: number | null, c: string) => (v == null ? tr("prix inconnu", "unknown price") : fmt(v, c));
const PERIOD = (): Record<Subscription["period"], string> => ({
  month: tr("/ mois", "/ mo"),
  year: tr("/ an", "/ yr"),
  week: tr("/ semaine", "/ wk"),
  usage: tr("à l'usage", "pay as you go"),
});
const days = (iso: string | null) => (iso ? Math.round((new Date(iso).getTime() - Date.now()) / 864e5) : null);
const inDays = (d: number) => tr(`dans ${d} j`, `in ${d} d`);

/** Active monthly plan of a vendor, the most expensive first. */
const planOf = (list: Subscription[], vendor: string) =>
  list.filter((s) => s.vendor.toLowerCase() === vendor.toLowerCase() && s.period === "month").sort((a, b) => (b.amount ?? 0) - (a.amount ?? 0))[0];

export default async function Abonnements() {
  const withRc = PROJECTS.filter((p) => p.revenuecat);
  const [rates, railway, revenues, claude, codex] = await Promise.all([
    fx(),
    source(billing),
    Promise.all(withRc.map((p) => source(() => rc.overview(p.revenuecat!.projectId)))),
    source(claudePlan),
    source(codexPlan),
  ]);
  const cur = rates.base;
  /** Monthly cost in your currency. */
  const toBase = (s: Subscription) => monthly(s) * (rates.toBase[s.currency] ?? 1);

  // Railway: the Hobby plan includes $5 of usage; the API gives the real usage of the month.
  const list = SUBSCRIPTIONS.map((s) =>
    s.vendor === "Railway" && railway.ok
      ? { ...s, evidence: tr(`Usage du mois : ${money(railway.data.currentUsage, "USD")} sur 5 $ inclus`, `Usage this month: ${money(railway.data.currentUsage, "USD")} of $5 included`), live: true }
      : s,
  );
  const active = list.filter((s) => s.status === "active" || s.status === "failing");
  const inactive = list.filter((s) => s.status === "cancelled" || s.status === "unknown");
  const perMonth = active.reduce((a, s) => a + toBase(s), 0);
  const upcoming = active
    .filter((s) => s.next_renewal && (days(s.next_renewal) ?? 999) >= 0)
    .sort((a, b) => a.next_renewal!.localeCompare(b.next_renewal!));
  const failing = active.filter((s) => s.status === "failing");
  // RevenueCat reports in your currency already (see sources/revenuecat.ts).
  const income = withRc.length && revenues.some((r) => r.ok) ? revenues.reduce((a, r) => a + (r.ok ? (r.data.mrr?.value ?? 0) : 0), 0) : null;
  const categories = CATEGORIES();
  const byCategory = categories.map((c) => ({ c, v: active.filter((s) => s.category === c.id).reduce((a, s) => a + toBase(s), 0) })).filter((x) => x.v > 0);
  const earner = withRc.length === 1 ? tr(`${withRc[0].name} rapporte`, `${withRc[0].name} earns`) : tr("Tes apps rapportent", "Your apps earn");

  return (
    <>
      <header className="relative mb-8 overflow-hidden rounded-[2rem] border border-line px-6 py-8 sm:px-10 sm:py-10">
        <div aria-hidden className="absolute inset-0" style={{ background: "radial-gradient(110% 140% at 0% 0%, #34d39926 0%, transparent 55%), radial-gradient(90% 130% at 100% 100%, #D4724F30 0%, transparent 60%)" }} />
        <div aria-hidden className="absolute inset-0 overflow-hidden opacity-60"><Meteors number={10} /></div>
        <div className="relative">
          <h1 className="font-display text-5xl font-black tracking-tight sm:text-7xl">{tr("Abonnements", "Subscriptions")}</h1>
          <p className="mt-2 max-w-2xl font-serif text-xl italic text-ink-2 sm:text-2xl">
            {tr("Tout ce qui est prélevé, quand, pour quoi, et combien il reste sur les plans IA.", "Everything that gets charged, when, what for, and how much is left on the AI plans.")}
          </p>
          <div className={cn("mt-8 grid grid-cols-2 gap-6 rounded-3xl border border-white/10 bg-black/30 p-6 backdrop-blur", withRc.length ? "md:grid-cols-4" : "md:grid-cols-3")}>
            <Stat
              label={tr("Par mois", "Per month")}
              value={perMonth}
              format={{ style: "currency", currency: cur, maximumFractionDigits: 0 }}
              color="#FFD166"
              hint={tr(`${active.length} actifs · ${active.filter((s) => s.amount == null).length} sans prix connu`, `${active.length} active · ${active.filter((s) => s.amount == null).length} without a known price`)}
            />
            <Stat label={tr("Par an", "Per year")} value={perMonth * 12} format={{ style: "currency", currency: cur, maximumFractionDigits: 0 }} hint={tr("au rythme actuel", "at the current pace")} />
            <Stat
              label={tr("Prochain prélèvement", "Next charge")}
              value={upcoming[0] ? days(upcoming[0].next_renewal) : null}
              suffix={tr(" j", " d")}
              hint={upcoming[0] ? `${upcoming[0].name} · ${money(upcoming[0].amount, upcoming[0].currency)}` : undefined}
            />
            {withRc.length > 0 && (
              <Stat
                label={earner}
                value={income}
                format={{ style: "currency", currency: cur }}
                hint={income != null ? (perMonth > 0 ? tr(`MRR · couvre ${pct(income / perMonth, 1)} des frais`, `MRR · covers ${pct(income / perMonth, 1)} of costs`) : "MRR") : tr("RevenueCat à brancher", "Connect RevenueCat")}
              />
            )}
          </div>
        </div>
      </header>

      {failing.length > 0 && (
        <div className="mb-5 rounded-2xl border border-bad/30 bg-bad/[0.06] p-5">
          <div className="mb-3 flex items-center gap-2 text-sm font-medium text-bad">
            <CircleAlert className="size-4" /> {tr("Paiements en échec : à régler", "Failed payments: to fix")}
          </div>
          <ul className="space-y-2">
            {failing.map((s) => {
              const d = days(s.next_renewal);
              return (
                <li key={s.name} className="flex flex-wrap items-center gap-x-3 gap-y-1 text-sm">
                  <span className="font-medium text-ink">{s.name}</span>
                  <span className="text-ink-2">{s.evidence}</span>
                  {d != null && d >= 0 && d < 60 && <span className="rounded-full bg-bad/15 px-2 py-0.5 text-[11px] text-bad">{inDays(d)}</span>}
                  {s.manage_url && (
                    <a href={s.manage_url} target="_blank" rel="noopener noreferrer" className="ml-auto inline-flex items-center gap-1 text-xs text-ink hover:underline">
                      {tr("Régler", "Fix")} <ArrowUpRight className="size-3" />
                    </a>
                  )}
                </li>
              );
            })}
          </ul>
        </div>
      )}

      <div className="grid gap-5 xl:grid-cols-2">
        <PlanCard title="Claude" color="#D4724F" price={planOf(active, "Anthropic")} src={claude} />
        <PlanCard title="ChatGPT & Codex" color="#5B8DEF" price={planOf(active, "OpenAI")} src={codex} />
      </div>

      <div className="mt-5 grid gap-5 xl:grid-cols-[1.6fr_1fr]">
        <Panel kicker={tr("Détail", "Detail")} title={tr("Ce que tu paies", "What you pay")} bodyClassName={active.length ? "p-0" : undefined}>
          {!active.length && (
            <Empty>
              {tr("Aucun abonnement. Liste-les sous ", "No subscription yet. List them under ")}
              <code className="font-mono text-xs text-sun">subscriptions</code>
              {tr(" dans ", " in ")}
              <code className="font-mono text-xs">zenith.config.json</code>.
            </Empty>
          )}
          {categories.filter((c) => active.some((s) => s.category === c.id)).map((c) => (
            <div key={c.id} className="border-t border-line first:border-0">
              <div className="flex items-center justify-between px-5 pb-2 pt-4">
                <span className="text-[11px] uppercase tracking-[0.18em] text-ink-3">{c.label}</span>
                <span className="font-mono text-xs text-ink-2">{base(active.filter((s) => s.category === c.id).reduce((a, s) => a + toBase(s), 0))} {tr("/ mois", "/ mo")}</span>
              </div>
              <ul>
                {active.filter((s) => s.category === c.id).sort((a, b) => toBase(b) - toBase(a)).map((s) => <Row key={s.name} s={s} perMonth={toBase(s)} />)}
              </ul>
            </div>
          ))}
        </Panel>

        <div className="space-y-5">
          <Panel kicker={tr("Répartition", "Breakdown")} title={tr(`Par catégorie, en ${cur} par mois`, `By category, in ${cur} per month`)}>
            <HBars rows={byCategory.sort((a, b) => b.v - a.v).map(({ c, v }) => ({ label: c.label, value: Math.round(v), key: c.id }))} color="#FFD166" format={(n) => base(n, 0)} />
            <p className="mt-4 text-xs text-ink-3">
              {rates.date
                ? tr(`Change du ${date(rates.date, { day: "numeric", month: "long" })} (BCE)`, `Rates of ${date(rates.date, { day: "numeric", month: "long" })} (ECB)`)
                : tr("Change approximatif (BCE injoignable)", "Approximate rates (ECB unreachable)")}
              {tr(" · les abonnements annuels comptent pour 1/12.", " · yearly subscriptions count as 1/12.")}
            </p>
          </Panel>
          <Panel kicker={tr("Calendrier", "Calendar")} title={tr("Prochains prélèvements", "Next charges")}>
            <ol className="space-y-3">
              {upcoming.slice(0, 10).map((s) => {
                const d = days(s.next_renewal)!;
                return (
                  <li key={s.name} className="flex items-center gap-3 text-sm">
                    <CalendarClock className={cn("size-4 shrink-0", d <= 7 ? "text-warn" : "text-ink-3")} />
                    <div className="min-w-0 flex-1">
                      <div className="truncate text-ink">{s.name}</div>
                      <div className="text-xs text-ink-3">{date(s.next_renewal!, { weekday: "short", day: "numeric", month: "short" })}</div>
                    </div>
                    <div className="text-right">
                      <div className="font-mono text-xs text-ink">{money(s.amount, s.currency)}</div>
                      <div className={cn("font-mono text-[11px]", d <= 7 ? "text-warn" : "text-ink-3")}>{inDays(d)}</div>
                    </div>
                  </li>
                );
              })}
              {!upcoming.length && <li className="text-sm text-ink-3">{tr("Aucune date connue.", "No known date.")}</li>}
            </ol>
          </Panel>
        </div>
      </div>

      {inactive.length > 0 && (
        <Panel className="mt-5" kicker={tr("Historique", "History")} title={tr("Arrêtés ou à vérifier", "Stopped or to check")}>
          <ul className="grid gap-x-8 gap-y-2 sm:grid-cols-2">
            {inactive.map((s) => (
              <li key={s.name} className="flex items-baseline justify-between gap-3 text-sm">
                <span className="truncate text-ink-2">{s.name}</span>
                <span className="shrink-0 text-xs text-ink-3">{s.status === "cancelled" ? tr("arrêté", "stopped") : tr("à vérifier", "to check")}{s.evidence ? ` · ${s.evidence}` : ""}</span>
              </li>
            ))}
          </ul>
        </Panel>
      )}
    </>
  );
}

function Row({ s, perMonth }: { s: Subscription & { live?: boolean }; perMonth: number }) {
  const p = PROJECTS.find((x) => x.id === s.project);
  const d = days(s.next_renewal);
  return (
    <li className="flex items-center gap-3 border-t border-line/60 px-5 py-3 text-sm first:border-0">
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2">
          <span className="truncate text-ink">{s.name}</span>
          {s.status === "failing" && <span className="rounded-full bg-bad/15 px-2 py-0.5 text-[10px] text-bad">{tr("paiement en échec", "payment failed")}</span>}
          {s.live && <span className="rounded-full bg-good/15 px-2 py-0.5 text-[10px] text-good">{tr("en direct", "live")}</span>}
        </div>
        <div className="mt-0.5 flex flex-wrap gap-x-3 text-xs text-ink-3">
          {s.vendor && <span>{s.vendor}</span>}
          {p && (
            <span className="inline-flex items-center gap-1.5">
              <span className="size-1.5 rounded-full" style={{ background: p.color }} />
              {p.name}
            </span>
          )}
          {!p && s.project && <span>{s.project}</span>}
          {d != null && d >= 0 && <span>{tr(`renouvellement dans ${d} j`, `renews in ${d} d`)}</span>}
        </div>
      </div>
      <div className="text-right">
        <div className="font-mono text-sm text-ink">
          {money(s.amount, s.currency)} <span className="text-xs text-ink-3">{PERIOD()[s.period]}</span>
        </div>
        {s.period !== "month" && s.period !== "usage" && <div className="font-mono text-[11px] text-ink-3">≈ {base(perMonth)} {tr("/ mois", "/ mo")}</div>}
      </div>
      {s.manage_url ? (
        <a href={s.manage_url} target="_blank" rel="noopener noreferrer" aria-label={tr(`Gérer ${s.name}`, `Manage ${s.name}`)} className="text-ink-3 hover:text-ink">
          <ArrowUpRight className="size-4" />
        </a>
      ) : (
        <span className="w-4" />
      )}
    </li>
  );
}
