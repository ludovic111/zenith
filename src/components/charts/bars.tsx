"use client";

import { useState } from "react";
import { cn } from "@/lib/utils";
import { l10n, tr } from "@/lib/i18n";

export type Part = { key: string; name: string; value: number; color: string };
export type BarDatum = { label: string; value?: number; parts?: Part[]; incomplete?: boolean };

/** "int" (default), "base" (the configured currency) or any ISO currency code ("usd", "EUR"…). */
export type BarUnit = string;

const fmt = (v: number, unit?: BarUnit) => {
  const { locale, currency } = l10n();
  if (!unit || unit === "int") return new Intl.NumberFormat(locale).format(v);
  const code = unit === "base" ? currency : unit.toUpperCase();
  return new Intl.NumberFormat(locale, { style: "currency", currency: code, maximumFractionDigits: 2 }).format(v);
};

/**
 * Thin vertical bars, rounded ends anchored to the baseline,
 * 2px between segments, tooltip when hovering anywhere in the column.
 */
export function Bars({
  data,
  color = "var(--primary)",
  unit,
  height = 160,
  legend,
  className,
}: {
  data: BarDatum[];
  color?: string;
  unit?: BarUnit;
  height?: number;
  legend?: { name: string; color: string }[];
  className?: string;
}) {
  const [hover, setHover] = useState<number | null>(null);
  const totals = data.map((d) => d.value ?? d.parts?.reduce((a, p) => a + p.value, 0) ?? 0);
  const max = Math.max(1, ...totals);

  return (
    <div className={cn("w-full", className)}>
      {legend && legend.length > 1 && (
        <div className="mb-3 flex flex-wrap gap-x-4 gap-y-1 text-xs text-ink-2">
          {legend.map((l) => (
            <span key={l.name} className="inline-flex items-center gap-1.5">
              <span className="size-2 rounded-full" style={{ background: l.color }} />
              {l.name}
            </span>
          ))}
        </div>
      )}
      <div className="relative" style={{ height }} onMouseLeave={() => setHover(null)}>
        {/* Two quiet gridlines, labelled on the right. */}
        {[0.5, 1].map((f) => (
          <div key={f} className="absolute inset-x-0 border-t border-dashed border-line" style={{ bottom: `${f * 100}%` }}>
            <span className="absolute -top-2 right-0 text-3xs text-ink-3 tabular">{fmt(Math.round(max * f * 100) / 100, unit)}</span>
          </div>
        ))}
        <div className="absolute inset-0 flex items-end gap-[3px] border-b border-line pr-10">
          {data.map((d, i) => {
            const total = totals[i];
            const parts = d.parts ?? [{ key: "v", name: "", value: total, color }];
            return (
              <div key={i} className="relative flex h-full flex-1 flex-col justify-end" onMouseEnter={() => setHover(i)}>
                <div
                  className={cn("flex flex-col-reverse gap-px transition-opacity", hover != null && hover !== i && "opacity-40")}
                  style={{ height: `${(total / max) * 100}%`, minHeight: total > 0 ? 3 : 0 }}
                >
                  {parts
                    .filter((p) => p.value > 0)
                    .map((p, j, arr) => (
                      <div
                        key={p.key}
                        className={cn(j === arr.length - 1 && "rounded-t-[3px]", d.incomplete && "opacity-50")}
                        style={{ flexGrow: p.value, background: p.color, minHeight: 2 }}
                      />
                    ))}
                </div>
              </div>
            );
          })}
        </div>
        {hover != null && (
          <div
            className="pointer-events-none absolute top-0 z-20 min-w-36 -translate-x-1/2 rounded-lg border border-line bg-popover px-3 py-2 text-xs shadow-lg"
            style={{ left: `clamp(72px, calc(${((hover + 0.5) / data.length) * 100}% - ${((hover + 0.5) / data.length) * 40}px), calc(100% - 72px))` }}
          >
            <div className="mb-1 text-ink-3">
              {data[hover].label}
              {data[hover].incomplete ? tr(" · en cours", " · in progress") : ""}
            </div>
            {data[hover].parts ? (
              <>
                {data[hover]
                  .parts!.filter((p) => p.value > 0)
                  .map((p) => (
                    <div key={p.key} className="flex items-center gap-2 text-ink-2">
                      <span className="size-2 rounded-full" style={{ background: p.color }} />
                      {p.name}
                      <span className="ml-auto pl-3 font-medium text-ink tabular">{fmt(p.value, unit)}</span>
                    </div>
                  ))}
                <div className="mt-1 flex border-t border-line pt-1 text-ink-2">
                  Total <span className="ml-auto font-medium text-ink tabular">{fmt(totals[hover], unit)}</span>
                </div>
              </>
            ) : (
              <div className="text-[13px] font-semibold text-ink tabular">{fmt(totals[hover], unit)}</div>
            )}
          </div>
        )}
      </div>
      <div className="mt-2 flex justify-between pr-10 text-3xs text-ink-3 tabular">
        <span>{data[0]?.label}</span>
        <span>{data[Math.floor(data.length / 2)]?.label}</span>
        <span>{data.at(-1)?.label}</span>
      </div>
    </div>
  );
}
