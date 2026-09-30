"use client";

import { useState } from "react";
import { l10n, plural, tr } from "@/lib/i18n";

export type Day = { t: number; total: number; parts: { name: string; color: string; value: number }[] };

/** One-hue sequential ramp on the accent, from an empty cell to full; works on light and dark. */
const RAMP = [
  "color-mix(in srgb, var(--foreground) 7%, transparent)",
  "color-mix(in srgb, var(--primary) 28%, var(--surface))",
  "color-mix(in srgb, var(--primary) 50%, var(--surface))",
  "color-mix(in srgb, var(--primary) 75%, var(--surface))",
  "var(--primary)",
];

export function Heatmap({ days }: { days: Day[] }) {
  const [hover, setHover] = useState<number | null>(null);
  const max = Math.max(1, ...days.map((d) => d.total));
  const level = (v: number) => (v === 0 ? 0 : Math.min(4, Math.ceil((v / max) * 4)));
  // Columns are weeks, Monday on top. Days are UTC midnights, so they are formatted in UTC.
  const month = new Intl.DateTimeFormat(l10n().locale, { month: "short", timeZone: "UTC" });
  const long = new Intl.DateTimeFormat(l10n().locale, { weekday: "long", day: "numeric", month: "long", timeZone: "UTC" });
  const offset = (new Date(days[0]?.t ?? 0).getUTCDay() + 6) % 7;
  const cells: (Day | null)[] = [...Array(offset).fill(null), ...days];
  const weeks = Math.ceil(cells.length / 7);
  const h = hover != null ? cells[hover] : null;
  const col = hover != null ? Math.floor(hover / 7) : 0;

  return (
    <div className="relative w-fit max-w-full" onMouseLeave={() => setHover(null)}>
      <div className="flex gap-2">
        <div className="grid grid-rows-7 gap-[3px] pt-5 text-3xs leading-none text-ink-3">
          {tr("L,,M,,V,,D", "M,,W,,F,,S")
            .split(",")
            .map((d, i) => (
              <span key={i} className="flex h-full items-center">
                {d}
              </span>
            ))}
        </div>
        <div className="min-w-0 flex-1 overflow-x-auto">
          <div className="mb-1.5 grid gap-[3px] text-3xs text-ink-3" style={{ gridTemplateColumns: `repeat(${weeks}, minmax(10px, 18px))` }}>
            {Array.from({ length: weeks }, (_, w) => {
              // A month is named on the first week that ends in it.
              const d = cells[w * 7 + 6] ?? cells[w * 7];
              const prev = w > 0 ? (cells[w * 7 - 1] ?? null) : null;
              const first = d && (!prev || new Date(prev.t).getUTCMonth() !== new Date(d.t).getUTCMonth());
              return (
                <span key={w} className="whitespace-nowrap">
                  {first && d ? month.format(d.t) : ""}
                </span>
              );
            })}
          </div>
          <div className="grid grid-flow-col grid-rows-7 gap-[3px]" style={{ gridTemplateColumns: `repeat(${weeks}, minmax(10px, 18px))` }}>
            {cells.map((d, i) => (
              <div
                key={i}
                onMouseEnter={() => d && setHover(i)}
                className="aspect-square rounded-[3px]"
                style={{ background: d ? RAMP[level(d.total)] : "transparent", outline: hover === i ? "1.5px solid var(--foreground)" : undefined, outlineOffset: -1 }}
              />
            ))}
          </div>
        </div>
      </div>
      <div className="mt-3 flex items-center justify-end gap-1 text-3xs text-ink-3">
        <span className="mr-1">{tr("moins", "less")}</span>
        {RAMP.map((c) => (
          <span key={c} className="size-2.5 rounded-[3px]" style={{ background: c }} />
        ))}
        <span className="ml-1">{tr("plus", "more")}</span>
      </div>
      {h && (
        <div
          className="pointer-events-none absolute top-0 z-20 min-w-44 -translate-x-1/2 -translate-y-[105%] rounded-lg border border-line bg-popover px-3 py-2 text-xs shadow-lg"
          style={{ left: `clamp(90px, ${((col + 0.5) / weeks) * 100}%, calc(100% - 90px))` }}
        >
          <div className="mb-1 text-ink-3">{long.format(h.t)}</div>
          {h.total === 0 ? (
            <div className="text-ink-2">{tr("Journée calme", "Quiet day")}</div>
          ) : (
            <>
              {h.parts
                .filter((p) => p.value)
                .map((p) => (
                  <div key={p.name} className="flex items-center gap-2 text-ink-2">
                    <span className="size-2 rounded-full" style={{ background: p.color }} />
                    {p.name}
                    <span className="ml-auto font-medium text-ink tabular">{p.value}</span>
                  </div>
                ))}
              <div className="mt-1 flex border-t border-line pt-1 text-ink-2">
                {h.total} {plural(h.total, ["commit", "commits"], ["commit", "commits"])}
              </div>
            </>
          )}
        </div>
      )}
    </div>
  );
}
