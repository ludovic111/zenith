"use client";

import { useState } from "react";
import { l10n, plural, tr } from "@/lib/i18n";

export type Day = { t: number; total: number; parts: { name: string; color: string; value: number }[] };

/** Single-hue (gold) sequential ramp, darkest to lightest. */
const RAMP = ["rgb(255 255 255 / .05)", "#4a3a1c", "#7c5e22", "#b98a2a", "#ffd166"];

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
    <div className="relative" onMouseLeave={() => setHover(null)}>
      <div className="flex gap-2">
        <div className="grid grid-rows-7 gap-[3px] pt-5 font-mono text-[9px] leading-none text-ink-3">
          {(tr("L,,M,,V,,D", "M,,W,,F,,S")).split(",").map((d, i) => (
            <span key={i} className="flex h-full items-center">{d}</span>
          ))}
        </div>
        <div className="min-w-0 flex-1 overflow-x-auto">
          <div className="mb-1.5 grid gap-[3px] font-mono text-[10px] text-ink-3" style={{ gridTemplateColumns: `repeat(${weeks}, minmax(10px, 1fr))` }}>
            {Array.from({ length: weeks }, (_, w) => {
              const d = cells[w * 7] ?? cells[w * 7 + 6];
              const first = d && new Date(d.t).getUTCDate() <= 7;
              return <span key={w} className="whitespace-nowrap">{first && d ? month.format(d.t) : ""}</span>;
            })}
          </div>
          <div className="grid grid-flow-col grid-rows-7 gap-[3px]" style={{ gridTemplateColumns: `repeat(${weeks}, minmax(10px, 1fr))` }}>
            {cells.map((d, i) => (
              <div
                key={i}
                onMouseEnter={() => d && setHover(i)}
                className="aspect-square rounded-[3px] transition-transform hover:scale-125"
                style={{ background: d ? RAMP[level(d.total)] : "transparent", outline: hover === i ? "1.5px solid #fff" : undefined }}
              />
            ))}
          </div>
        </div>
      </div>
      <div className="mt-3 flex items-center justify-end gap-1.5 text-[10px] text-ink-3">
        {tr("moins", "less")} {RAMP.map((c) => <span key={c} className="size-2.5 rounded-[3px]" style={{ background: c }} />)} {tr("plus", "more")}
      </div>
      {h && (
        <div
          className="pointer-events-none absolute top-0 z-20 min-w-44 -translate-x-1/2 -translate-y-[105%] rounded-xl border border-white/10 bg-[#141224]/95 px-3 py-2 text-xs shadow-xl"
          style={{ left: `clamp(90px, ${((col + 0.5) / weeks) * 100}%, calc(100% - 90px))` }}
        >
          <div className="mb-1 text-ink-3">
            {long.format(h.t)}
          </div>
          {h.total === 0 ? (
            <div className="text-ink-2">{tr("Journée calme", "Quiet day")}</div>
          ) : (
            <>
              {h.parts.filter((p) => p.value).map((p) => (
                <div key={p.name} className="flex items-center gap-2 text-ink-2">
                  <span className="size-2 rounded-full" style={{ background: p.color }} />
                  {p.name}
                  <span className="ml-auto font-mono text-ink">{p.value}</span>
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
