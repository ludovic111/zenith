"use client";

import { useState } from "react";
import { l10n, tr } from "@/lib/i18n";

type Sample = { t: number; ms: number | null; status: number };
const up = (s: Sample) => s.status > 0 && (s.status < 400 || s.status === 401);

/** One bar per measured minute: solid if the service answered, red otherwise. */
export function UptimeStrip({ samples, slots = 60 }: { samples: Sample[]; slots?: number }) {
  const [hover, setHover] = useState<number | null>(null);
  const cells: (Sample | null)[] = [...Array(Math.max(0, slots - samples.length)).fill(null), ...samples.slice(-slots)];
  const h = hover != null ? cells[hover] : null;
  return (
    <div className="relative" onMouseLeave={() => setHover(null)}>
      <div className="flex h-6 items-stretch gap-[2px]">
        {cells.map((s, i) => (
          <div
            key={i}
            onMouseEnter={() => setHover(i)}
            className="flex-1 rounded-[2px] transition-opacity"
            style={{
              background: s == null ? "rgb(255 255 255 / .06)" : up(s) ? "var(--good)" : "var(--bad)",
              opacity: hover != null && hover !== i ? 0.45 : s && up(s) ? 0.8 : 1,
            }}
          />
        ))}
      </div>
      {h && hover != null && (
        <div
          className="pointer-events-none absolute -top-2 z-20 -translate-x-1/2 -translate-y-full whitespace-nowrap rounded-lg border border-white/10 bg-[#141224]/95 px-2.5 py-1.5 text-xs shadow-xl"
          style={{ left: `clamp(70px, ${((hover + 0.5) / cells.length) * 100}%, calc(100% - 70px))` }}
        >
          <span className="text-ink-3">
            {new Intl.DateTimeFormat(l10n().locale, { timeZone: l10n().timeZone, hour: "2-digit", minute: "2-digit" }).format(h.t)}
          </span>{" "}
          <span className="font-mono text-ink">{up(h) ? `${h.status} · ${h.ms} ms` : h.status ? `HTTP ${h.status}` : tr("sans réponse", "no response")}</span>
        </div>
      )}
    </div>
  );
}
