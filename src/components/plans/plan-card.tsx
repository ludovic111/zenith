import type { Source } from "@/lib/source";
import type { Subscription } from "@/lib/subscriptions";
import type { PlanUsage } from "@/lib/sources/plans";
import { ago, date, money, pct as percent } from "@/lib/format";
import { tr } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import { Panel } from "@/components/z/panel";

const price$ = (v: number | null, c: string) => (v == null ? tr("prix inconnu", "unknown price") : money(v, c));

/**
 * Gauges of an AI plan (Claude, ChatGPT/Codex): share used and reset time, as thin bars.
 * `price` is the matching subscription from the config, if any; `hint` explains how to get
 * a first reading; `action` sits under the gauges (e.g. a button to take a new reading).
 */
export function PlanCard({
  title,
  color,
  price,
  src,
  hint,
  action,
  className,
}: {
  title: string;
  color: string;
  price?: Subscription;
  src: Source<PlanUsage | null>;
  hint?: React.ReactNode;
  action?: React.ReactNode;
  className?: string;
}) {
  const plan = src.ok ? src.data : null;
  return (
    <Panel
      className={className}
      title={
        <span className="inline-flex items-center gap-2">
          <span className="size-2 rounded-full" style={{ background: color }} />
          {plan?.plan ?? title}
        </span>
      }
      action={price && <span className="text-xs text-ink-2 tabular">{price$(price.amount, price.currency)} {tr("/ mois", "/ mo")}</span>}
    >
      {plan ? (
        <>
          <div className="space-y-3.5">
            {plan.windows.map((w) => {
              // A frozen reading whose window has reset since says nothing about current usage.
              const stale = !plan.live && !!w.resetsAt && new Date(w.resetsAt).getTime() < Date.now();
              const pct = stale ? 0 : Math.max(0, Math.min(100, w.percentUsed));
              const tone = pct >= 90 ? "var(--bad)" : pct >= 70 ? "var(--warn)" : color;
              return (
                <div key={w.label}>
                  <div className="mb-1 flex items-baseline justify-between gap-3 text-[13px]">
                    <span className="truncate text-ink-2">{w.label}</span>
                    <span className={cn("font-medium tabular", pct >= 90 ? "text-bad" : pct >= 70 ? "text-warn" : "text-ink")}>{stale ? tr("rechargée", "reset") : percent(pct / 100)}</span>
                  </div>
                  <div className="h-1.5 overflow-hidden rounded-full bg-muted">{!stale && <div className="h-full rounded-full" style={{ width: `${Math.max(pct, 1)}%`, background: tone }} />}</div>
                  {w.resetsAt && !stale && (
                    <div className="mt-1 text-2xs text-ink-3">
                      {tr("se recharge", "resets")} {date(w.resetsAt, { weekday: "short", day: "numeric", month: "short", hour: "2-digit", minute: "2-digit" })}
                    </div>
                  )}
                </div>
              );
            })}
          </div>
          <div className="mt-4 flex flex-wrap items-center justify-between gap-2 border-t border-line pt-3">
            <p className="text-xs text-ink-3">
              {plan.live ? tr(`En direct des sessions Codex · ${ago(plan.capturedAt)}`, `Live from Codex sessions · ${ago(plan.capturedAt)}`) : tr(`Relevé ${ago(plan.capturedAt)}`, `Read ${ago(plan.capturedAt)}`)}
            </p>
            {action}
          </div>
        </>
      ) : (
        <div className="space-y-3">
          <p className="text-[13px] text-ink-3">
            {tr("Pas encore de relevé.", "No reading yet.")}
            {hint && <span className="mt-1 block text-xs">{hint}</span>}
          </p>
          {action}
        </div>
      )}
    </Panel>
  );
}
