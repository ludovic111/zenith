"use client";

import { useId, useRef, useState } from "react";
import { l10n, tr } from "@/lib/i18n";

export type Point = { t: number; v: number | null };

const time = (t: number) =>
  new Intl.DateTimeFormat(l10n().locale, { timeZone: l10n().timeZone, day: "numeric", month: "short", hour: "2-digit", minute: "2-digit" }).format(t);

/** 2px line + gradient area, crosshair and tooltip on hover. Gaps (null) break the line. */
export function Line({
  points,
  color,
  height = 64,
  unit = "ms",
  axis = false,
}: {
  points: Point[];
  color: string;
  height?: number;
  unit?: string;
  axis?: boolean;
}) {
  const id = useId();
  const ref = useRef<SVGSVGElement>(null);
  const [hover, setHover] = useState<number | null>(null);
  const W = 400;
  const vals = points.map((p) => p.v).filter((v): v is number => v != null);
  if (points.length < 2 || !vals.length) return <div style={{ height }} className="grid place-items-center text-xs text-ink-3">{tr("Pas encore de mesures", "No measurements yet")}</div>;
  const max = Math.max(...vals) * 1.15 || 1;
  const x = (i: number) => (i / (points.length - 1)) * W;
  const y = (v: number) => height - 3 - (v / max) * (height - 8);

  const segments: string[] = [];
  let cur = "";
  points.forEach((p, i) => {
    if (p.v == null) {
      if (cur) segments.push(cur);
      cur = "";
    } else cur += `${cur ? "L" : "M"}${x(i).toFixed(1)},${y(p.v).toFixed(1)}`;
  });
  if (cur) segments.push(cur);
  const line = segments.join(" ");
  const area = segments.map((s) => {
    const xs = [...s.matchAll(/[ML]([\d.]+),/g)].map((m) => m[1]);
    return `${s} L${xs.at(-1)},${height} L${xs[0]},${height} Z`;
  });

  const onMove = (e: React.MouseEvent) => {
    const r = ref.current!.getBoundingClientRect();
    setHover(Math.round(((e.clientX - r.left) / r.width) * (points.length - 1)));
  };
  const h = hover != null ? points[Math.max(0, Math.min(points.length - 1, hover))] : null;

  return (
    <div className="relative">
      <svg ref={ref} viewBox={`0 0 ${W} ${height}`} preserveAspectRatio="none" className="block w-full overflow-visible" style={{ height }} onMouseMove={onMove} onMouseLeave={() => setHover(null)}>
        <defs>
          <linearGradient id={id} x1="0" x2="0" y1="0" y2="1">
            <stop offset="0%" stopColor={color} stopOpacity="0.35" />
            <stop offset="100%" stopColor={color} stopOpacity="0" />
          </linearGradient>
        </defs>
        {area.map((d, i) => <path key={i} d={d} fill={`url(#${id})`} />)}
        <path d={line} fill="none" stroke={color} strokeWidth={2} vectorEffect="non-scaling-stroke" strokeLinejoin="round" strokeLinecap="round" />
        {h && hover != null && (
          <line x1={x(hover)} x2={x(hover)} y1={0} y2={height} stroke="rgb(255 255 255 / .35)" strokeWidth={1} vectorEffect="non-scaling-stroke" />
        )}
      </svg>
      {h && hover != null && (
        <>
          {h.v != null && (
            <span className="pointer-events-none absolute size-2.5 -translate-x-1/2 -translate-y-1/2 rounded-full ring-2 ring-[#0b0a14]" style={{ left: `${(hover / (points.length - 1)) * 100}%`, top: y(h.v), background: color }} />
          )}
          <div
            className="pointer-events-none absolute -top-2 z-20 -translate-x-1/2 -translate-y-full whitespace-nowrap rounded-lg border border-white/10 bg-[#141224]/95 px-2.5 py-1.5 text-xs shadow-xl"
            style={{ left: `clamp(60px, ${(hover / (points.length - 1)) * 100}%, calc(100% - 60px))` }}
          >
            <span className="text-ink-3">{time(h.t)}</span>{" "}
            <span className="font-mono text-ink">{h.v == null ? tr("sans réponse", "no response") : `${Math.round(h.v)} ${unit}`}</span>
          </div>
        </>
      )}
      {axis && (
        <div className="mt-1 flex justify-between font-mono text-[10px] text-ink-3">
          <span>{time(points[0].t)}</span>
          <span>{time(points.at(-1)!.t)}</span>
        </div>
      )}
    </div>
  );
}
