import type { Metadata } from "next";
import { ArrowUpRight, CircleAlert } from "lucide-react";
import { PROJECTS } from "@/lib/projects";
import { SUBSCRIPTIONS, CATEGORIES, monthly, type Subscription } from "@/lib/subscriptions";
import { source } from "@/lib/source";
import { base, date, money as fmt, pct } from "@/lib/format";
import { plural, tr } from "@/lib/i18n";
import { claudePlan, codexPlan, fx } from "@/lib/sources/plans";
import { AGENTS } from "@/lib/sources/agents";
import { PlanCard } from "@/components/plans/plan-card";
import { billing } from "@/lib/sources/railway";
import * as rc from "@/lib/sources/revenuecat";
import { agentUi } from "@/lib/agent/ui";
import { AskButton } from "@/components/agent/ask-button";
import { Empty, PageHeader, Panel } from "@/components/z/panel";
import { Stat } from "@/components/z/stat";
import { HBars } from "@/components/charts/hbars";
import { cn } from "@/lib/utils";

export const dynamic = "force-dynamic";
export function generateMetadata(): Metadata {
  return { title: tr("Abonnements", "Subscriptions") };
}

type Sub = Subscription & { live?: boolean };

const money = (v: number | null, c: string) => (v == null ? tr("prix inconnu", "unknown price") : fmt(v, c));
const PERIOD = (): Record<Subscription["period"], string> => ({
  month: tr("/ mois", "/ mo"),
  year: tr("/ an", "/ yr"),
  week: tr("/ semaine", "/ wk"),
  usage: tr("à l'usage", "pay as you go"),
});
const days = (iso: string | null) => (iso ? Math.round((new Date(iso).getTime() - Date.now()) / 864e5) : null);
const inDays = (d: number) => (d === 0 ? tr("aujourd'hui", "today") : d === 1 ? tr("demain", "tomorrow") : tr(`dans ${d} j`, `in ${d} d`));

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
  const ui = agentUi();
  const cur = rates.base;
  /** Monthly cost in your currency. */
  const toBase = (s: Subscription) => monthly(s) * (rates.toBase[s.currency] ?? 1);
  /** One charge in your currency. */
  const chargeBase = (s: Subscription) => (s.amount ?? 0) * (rates.toBase[s.currency] ?? 1);

  // Railway: the Hobby plan includes $5 of usage; the API gives the real usage of the month.
  const list: Sub[] = SUBSCRIPTIONS.map((s) =>
    s.vendor === "Railway" && railway.ok
      ? { ...s, evidence: tr(`Usage du mois : ${money(railway.data.currentUsage, "USD")} sur 5 $ inclus`, `Usage this month: ${money(railway.data.currentUsage, "USD")} of $5 included`), live: true }
      : s,
  );
  const active = list.filter((s) => s.status === "active" || s.status === "failing");
  const inactive = list.filter((s) => s.status === "cancelled" || s.status === "unknown");
  const perMonth = active.reduce((a, s) => a + toBase(s), 0);
  const upcoming = active.filter((s) => s.next_renewal && (days(s.next_renewal) ?? 999) >= 0).sort((a, b) => a.next_renewal!.localeCompare(b.next_renewal!));
  const next30 = upcoming.filter((s) => days(s.next_renewal)! <= 30);
  const failing = active.filter((s) => s.status === "failing").sort((a, b) => (a.next_renewal ?? "9999").localeCompare(b.next_renewal ?? "9999"));
  // RevenueCat reports in your currency already (see sources/revenuecat.ts).
  const income = withRc.length && revenues.some((r) => r.ok) ? revenues.reduce((a, r) => a + (r.ok ? (r.data.mrr?.value ?? 0) : 0), 0) : null;
  const categories = CATEGORIES();
  const byCategory = categories.map((c) => ({ c, v: active.filter((s) => s.category === c.id).reduce((a, s) => a + toBase(s), 0) })).filter((x) => x.v > 0);
  const earner = withRc.length === 1 ? tr(`${withRc[0].name} rapporte`, `${withRc[0].name} earns`) : tr("Tes apps rapportent", "Your apps earn");
  const unpriced = active.filter((s) => s.amount == null).length;

  const review = active
    .map((s) => `- ${s.name} (${s.vendor}) · ${money(s.amount, s.currency)} ${PERIOD()[s.period]} · ${categories.find((c) => c.id === s.category)?.label ?? s.category}${s.project ? ` · ${s.project}` : ""}`)
    .join("\n");

  return (
    <>
      <PageHeader
        title={tr("Abonnements", "Subscriptions")}
        description={[
          `${active.length} ${plural(active.length, ["actif", "actifs"], ["active", "active"])}`,
          failing.length ? tr(`${failing.length} en échec`, `${failing.length} failing`) : null,
          upcoming[0] ? tr(`prochain : ${upcoming[0].name} ${inDays(days(upcoming[0].next_renewal)!)}`, `next: ${upcoming[0].name} ${inDays(days(upcoming[0].next_renewal)!)}`) : null,
        ]
          .filter(Boolean)
          .join(" · ")}
        action={
          ui.enabled && active.length > 0 ? (
            <AskButton
              variant="primary"
              target="life"
              label={tr("Où économiser ?", "Where to save?")}
              prompt={tr(
                `Passe en revue mes abonnements et propose ceux que je pourrais résilier, réduire ou remplacer : doublons, outils que je n'utilise plus, plans trop gros pour mon usage, alternatives moins chères. Chiffre l'économie mensuelle de chaque proposition (en ${cur}) et classe-les de la plus rentable à la moins rentable. Appuie-toi sur argent.md du brief zenith et sur mes e-mails si besoin. N'annule, ne modifie et ne paie rien : c'est seulement une proposition.\n\nMes abonnements actifs (${base(perMonth, 0)} par mois) :\n${review}`,
                `Review my subscriptions and suggest which ones I could cancel, downgrade or replace: duplicates, tools I no longer use, plans too big for my usage, cheaper alternatives. Put a monthly saving on each suggestion (in ${cur}) and rank them from most to least worthwhile. Use argent.md from the zenith brief and my emails if needed. Don't cancel, change or pay anything: this is only a proposal.\n\nMy active subscriptions (${base(perMonth, 0)} per month):\n${review}`,
              )}
            />
          ) : undefined
        }
      />

      <div className={cn("grid grid-cols-2 gap-x-6 gap-y-4 rounded-xl border border-line bg-surface px-4 py-3.5", withRc.length ? "md:grid-cols-4" : "md:grid-cols-3")}>
        <Stat
          label={tr("Par mois", "Per month")}
          value={perMonth}
          format={{ style: "currency", currency: cur, maximumFractionDigits: 0 }}
          big
          hint={unpriced ? tr(`${unpriced} sans prix connu`, `${unpriced} without a known price`) : tr("annuels comptés pour 1/12", "yearly counted as 1/12")}
        />
        <Stat label={tr("Par an", "Per year")} value={perMonth * 12} format={{ style: "currency", currency: cur, maximumFractionDigits: 0 }} hint={tr("au rythme actuel", "at the current pace")} />
        <Stat
          label={tr("30 prochains jours", "Next 30 days")}
          value={next30.reduce((a, s) => a + chargeBase(s), 0)}
          format={{ style: "currency", currency: cur, maximumFractionDigits: 0 }}
          hint={`${next30.length} ${plural(next30.length, ["prélèvement", "prélèvements"], ["charge", "charges"])}`}
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

      {failing.length > 0 && (
        <section className="mt-4 overflow-hidden rounded-xl border border-bad/25 bg-bad/5">
          <header className="flex items-center gap-2 px-4 pt-3 text-[13px] font-semibold text-ink">
            <CircleAlert className="size-4 text-bad" />
            {tr("Paiements en échec", "Failed payments")}
            <span className="font-normal text-ink-3 tabular">{failing.length}</span>
          </header>
          <ul className="divide-y divide-bad/15 px-4 pb-1 pt-1">
            {failing.map((s) => {
              const d = days(s.next_renewal);
              return (
                <li key={s.name} className="flex flex-wrap items-center gap-x-4 gap-y-2 py-2.5">
                  <div className="min-w-0 flex-1">
                    <div className="flex items-center gap-2 text-[13px]">
                      <span className="font-medium text-ink">{s.name}</span>
                      {d != null && d >= 0 && d < 60 && <span className="rounded bg-bad/10 px-1.5 py-px text-2xs font-medium text-bad tabular">{inDays(d)}</span>}
                    </div>
                    {s.evidence && <div className="mt-0.5 text-xs text-ink-2">{s.evidence}</div>}
                  </div>
                  <div className="flex shrink-0 items-center gap-2">
                    {s.manage_url && (
                      <a href={s.manage_url} target="_blank" rel="noopener noreferrer" className="inline-flex h-7 items-center gap-1 rounded-md border border-line bg-surface px-2 text-xs text-ink-2 transition-colors hover:bg-hover hover:text-ink">
                        {tr("Gérer", "Manage")} <ArrowUpRight className="size-3 text-ink-3" />
                      </a>
                    )}
                    {ui.enabled && (
                      <AskButton
                        target="life"
                        label={tr("Règle ce paiement", "Fix this payment")}
                        prompt={tr(
                          `Le paiement de mon abonnement ${s.name} (${s.vendor}) est en échec${s.evidence ? ` : ${s.evidence}` : ""}. Trouve pourquoi (e-mails de ${s.vendor}, carte expirée, solde insuffisant…), dis-moi ce qui se passe si je ne fais rien et quand, puis prépare les étapes exactes pour le régler${s.manage_url ? ` (page de gestion : ${s.manage_url})` : ""}. Ne paie rien et ne change aucun moyen de paiement sans me demander.`,
                          `The payment for my ${s.name} subscription (${s.vendor}) failed${s.evidence ? `: ${s.evidence}` : ""}. Find out why (emails from ${s.vendor}, expired card, low balance…), tell me what happens if I do nothing and when, then prepare the exact steps to fix it${s.manage_url ? ` (billing page: ${s.manage_url})` : ""}. Don't pay anything or change any payment method without asking me.`,
                        )}
                      />
                    )}
                  </div>
                </li>
              );
            })}
          </ul>
        </section>
      )}

      <div className="mt-8 grid gap-4 lg:grid-cols-2">
        <PlanCard title="Claude" color={AGENTS.claude.color} price={planOf(active, "Anthropic")} src={claude} />
        <PlanCard title="ChatGPT & Codex" color={AGENTS.codex.color} price={planOf(active, "OpenAI")} src={codex} />
      </div>

      <div className="mt-8 grid items-start gap-4 lg:grid-cols-[1.6fr_1fr]">
        <Panel
          title={tr("Ce que tu paies", "What you pay")}
          action={<span className="tabular">{tr(`${base(perMonth, 0)} / mois`, `${base(perMonth, 0)} / mo`)}</span>}
          bodyClassName={active.length ? "px-0 pb-1 pt-1" : undefined}
        >
          {!active.length && (
            <Empty>
              {tr("Aucun abonnement. Liste-les sous ", "No subscription yet. List them under ")}
              <code className="font-mono text-xs text-ink-2">subscriptions</code>
              {tr(" dans ", " in ")}
              <code className="font-mono text-xs">zenith.config.json</code>.
            </Empty>
          )}
          {categories
            .filter((c) => active.some((s) => s.category === c.id))
            .sort((a, b) => (byCategory.find((x) => x.c.id === b.id)?.v ?? 0) - (byCategory.find((x) => x.c.id === a.id)?.v ?? 0))
            .map((c) => (
              <div key={c.id} className="mt-3 first:mt-0">
                <div className="flex items-center justify-between px-4 pb-1 text-xs">
                  <span className="font-medium text-ink-3">{c.label}</span>
                  <span className="text-ink-3 tabular">{base(active.filter((s) => s.category === c.id).reduce((a, s) => a + toBase(s), 0), 0)}</span>
                </div>
                <ul className="border-t border-line">
                  {active
                    .filter((s) => s.category === c.id)
                    .sort((a, b) => toBase(b) - toBase(a))
                    .map((s) => (
                      <Row key={s.name} s={s} perMonth={toBase(s)} cur={cur} />
                    ))}
                </ul>
              </div>
            ))}
        </Panel>

        <div className="space-y-4">
          <Panel title={tr("Prochains prélèvements", "Next charges")} action={next30.length ? <span className="tabular">{tr(`${next30.length} sur 30 j`, `${next30.length} in 30 d`)}</span> : undefined} bodyClassName="px-0 pb-1 pt-1">
            {upcoming.length ? (
              <ol>
                {upcoming.slice(0, 10).map((s) => {
                  const d = days(s.next_renewal)!;
                  const soon = d <= 7;
                  return (
                    <li key={s.name} className="flex items-center gap-3 border-t border-line px-4 py-2 first:border-0">
                      <div className="w-9 shrink-0 text-center leading-tight">
                        <div className={cn("text-[13px] font-semibold tabular", soon ? "text-warn" : "text-ink")}>{date(s.next_renewal!, { day: "numeric" })}</div>
                        <div className="text-2xs text-ink-3">{date(s.next_renewal!, { month: "short" })}</div>
                      </div>
                      <div className="min-w-0 flex-1">
                        <div className="flex items-center gap-1.5 truncate text-[13px] text-ink">
                          {s.status === "failing" && <CircleAlert className="size-3.5 shrink-0 text-bad" />}
                          <span className="truncate">{s.name}</span>
                        </div>
                        <div className={cn("text-xs", soon ? "text-warn" : "text-ink-3")}>{inDays(d)}</div>
                      </div>
                      <div className="shrink-0 text-right text-xs text-ink-2 tabular">{money(s.amount, s.currency)}</div>
                    </li>
                  );
                })}
              </ol>
            ) : (
              <p className="px-4 py-2 text-[13px] text-ink-3">{tr("Aucune date connue. Ajoute next_renewal à tes abonnements.", "No known date. Add next_renewal to your subscriptions.")}</p>
            )}
          </Panel>

          {byCategory.length > 0 && (
            <Panel title={tr("Par catégorie", "By category")} action={tr(`${cur} / mois`, `${cur} / mo`)}>
              <HBars rows={byCategory.sort((a, b) => b.v - a.v).map(({ c, v }) => ({ label: c.label, value: Math.round(v), key: c.id }))} color="var(--ink-3)" format={(n) => base(n, 0)} />
              <p className="mt-4 text-xs text-ink-3">
                {rates.date
                  ? tr(`Change du ${date(rates.date, { day: "numeric", month: "long" })} (BCE).`, `Rates of ${date(rates.date, { day: "numeric", month: "long" })} (ECB).`)
                  : tr("Change approximatif (BCE injoignable).", "Approximate rates (ECB unreachable).")}
              </p>
            </Panel>
          )}
        </div>
      </div>

      {inactive.length > 0 && (
        <Panel className="mt-4" title={tr("Arrêtés ou à vérifier", "Stopped or to check")} action={<span className="tabular">{inactive.length}</span>} bodyClassName="px-0 pb-1 pt-1">
          <ul>
            {inactive.map((s) => (
              <li key={s.name} className="flex items-baseline justify-between gap-4 border-t border-line px-4 py-2 text-[13px] first:border-0">
                <span className="truncate text-ink-2">{s.name}</span>
                <span className="min-w-0 truncate text-right text-xs text-ink-3">
                  {s.status === "cancelled" ? tr("arrêté", "stopped") : tr("à vérifier", "to check")}
                  {s.evidence ? ` · ${s.evidence}` : ""}
                </span>
              </li>
            ))}
          </ul>
        </Panel>
      )}
    </>
  );
}

function Row({ s, perMonth, cur }: { s: Sub; perMonth: number; cur: string }) {
  const p = PROJECTS.find((x) => x.id === s.project);
  const d = days(s.next_renewal);
  const converted = s.currency !== cur || (s.period !== "month" && s.period !== "usage");
  return (
    <li className="group flex items-center gap-3 border-t border-line px-4 py-2 first:border-0 hover:bg-hover">
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2 text-[13px]">
          <span className="truncate text-ink">{s.name}</span>
          {s.status === "failing" && <span className="shrink-0 rounded bg-bad/10 px-1.5 py-px text-2xs font-medium text-bad">{tr("en échec", "failing")}</span>}
          {s.live && <span className="shrink-0 rounded bg-good/10 px-1.5 py-px text-2xs font-medium text-good">{tr("en direct", "live")}</span>}
        </div>
        <div className="flex flex-wrap items-center gap-x-2.5 text-xs text-ink-3">
          {s.vendor && s.vendor !== s.name && <span>{s.vendor}</span>}
          {p ? (
            <span className="inline-flex items-center gap-1.5">
              <span className="size-1.5 rounded-full" style={{ background: p.color }} />
              {p.name}
            </span>
          ) : (
            s.project && <span>{s.project}</span>
          )}
          {d != null && d >= 0 && <span>{tr(`renouvellement ${inDays(d)}`, `renews ${inDays(d)}`)}</span>}
          {s.live && s.evidence && <span className="truncate">{s.evidence}</span>}
        </div>
      </div>
      <div className="shrink-0 text-right">
        <div className="text-[13px] text-ink tabular">
          {s.amount == null ? <span className="text-ink-3">{tr("prix inconnu", "unknown price")}</span> : money(s.amount, s.currency)}
          {s.amount != null && <span className="text-xs text-ink-3"> {PERIOD()[s.period]}</span>}
        </div>
        {converted && s.amount != null && s.period !== "usage" && <div className="text-2xs text-ink-3 tabular">≈ {tr(`${base(perMonth)} / mois`, `${base(perMonth)} / mo`)}</div>}
      </div>
      {s.manage_url ? (
        <a href={s.manage_url} target="_blank" rel="noopener noreferrer" aria-label={tr(`Gérer ${s.name}`, `Manage ${s.name}`)} className="shrink-0 rounded p-0.5 text-ink-3 opacity-60 transition-opacity hover:text-ink group-hover:opacity-100">
          <ArrowUpRight className="size-4" />
        </a>
      ) : (
        <span className="w-5 shrink-0" />
      )}
    </li>
  );
}
