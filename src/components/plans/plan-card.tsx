import { Gauge } from "lucide-react";
import type { Source } from "@/lib/source";
import type { Subscription } from "@/lib/subscriptions";
import type { PlanUsage } from "@/lib/sources/plans";
import { ago, date, money, pct as percent } from "@/lib/format";
import { tr } from "@/lib/i18n";
import { Panel } from "@/components/z/panel";

const price$ = (v: number | null, c: string) => (v == null ? tr("prix inconnu", "unknown price") : money(v, c));

/**
 * Gauges of an AI plan (Claude, ChatGPT/Codex): share used and reset time. `price` is the
 * matching subscription from the config, if any; `hint` explains how to get a first reading.
 */
export function PlanCard({ title, color, price, src, hint }: { title: string; color: string; price?: Subscription; src: Source<PlanUsage | null>; hint?: React.ReactNode }) {
  const plan = src.ok ? src.data : null;
  return (
    <Panel title={plan?.plan ?? title} accent={color} action={price && <span className="font-mono text-sm text-ink">{price$(price.amount, price.currency)} {tr("/ mois", "/ mo")}</span>} kicker={tr("Plan IA", "AI plan")}>
      {plan ? (
        <>
          <div className="space-y-5">
            {plan.windows.map((w) => {
              // A frozen reading whose window has reset since says nothing about current usage.
              const stale = !plan.live && !!w.resetsAt && new Date(w.resetsAt).getTime() < Date.now();
              const pct = stale ? 0 : Math.max(0, Math.min(100, w.percentUsed));
              const tone = pct >= 90 ? "var(--bad)" : pct >= 70 ? "var(--warn)" : color;
              return (
                <div key={w.label}>
                  <div className="mb-1.5 flex items-baseline justify-between text-sm">
                    <span className="text-ink-2">{w.label}</span>
                    <span className="font-mono text-ink">{stale ? tr("rechargée", "reset") : percent(pct / 100)}</span>
                  </div>
                  <div className="h-2.5 overflow-hidden rounded-full bg-white/[0.06]">
                    {!stale && <div className="h-full rounded-full" style={{ width: `${Math.max(pct, 1.5)}%`, background: tone }} />}
                  </div>
                  {w.resetsAt && !stale && <div className="mt-1 text-xs text-ink-3">{tr("se recharge", "resets")} {date(w.resetsAt, { weekday: "short", day: "numeric", month: "short", hour: "2-digit", minute: "2-digit" })}</div>}
                </div>
              );
            })}
          </div>
          <p className="mt-5 inline-flex items-center gap-1.5 text-xs text-ink-3">
            <Gauge className="size-3.5" />
            {plan.live
              ? tr(`En direct depuis les sessions Codex · ${ago(plan.capturedAt)}`, `Live from Codex sessions · ${ago(plan.capturedAt)}`)
              : tr(`Relevé par Claude ${ago(plan.capturedAt)} — demande « mets à jour mes limites Claude dans zenith »`, `Read by Claude ${ago(plan.capturedAt)} — ask "update my Claude limits in zenith"`)}
          </p>
        </>
      ) : (
        <div className="text-sm text-ink-3">
          {tr("Pas encore de relevé.", "No reading yet.")}
          {hint && <div className="mt-1 text-xs">{hint}</div>}
        </div>
      )}
    </Panel>
  );
}
