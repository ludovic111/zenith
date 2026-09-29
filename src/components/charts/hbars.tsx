import { cn } from "@/lib/utils";
import { nf } from "@/lib/format";

/** Directly labelled horizontal bars: name on the left, value on the right. */
export function HBars({
  rows,
  color,
  className,
  format = (n) => nf(n),
}: {
  rows: { label: React.ReactNode; value: number; key?: string; color?: string; hint?: string }[];
  color: string;
  className?: string;
  format?: (n: number) => string;
}) {
  const max = Math.max(1, ...rows.map((r) => r.value));
  return (
    <ul className={cn("space-y-2.5", className)}>
      {rows.map((r, i) => (
        <li key={r.key ?? i} className="text-sm">
          <div className="mb-1 flex items-baseline justify-between gap-3">
            <span className="min-w-0 truncate text-ink-2">{r.label}</span>
            <span className="shrink-0 font-mono text-xs text-ink tabular">
              {format(r.value)}
              {r.hint && <span className="ml-1.5 text-ink-3">{r.hint}</span>}
            </span>
          </div>
          <div className="h-1.5 rounded-full bg-white/[0.05]">
            <div className="h-full rounded-full" style={{ width: `${Math.max(1.5, (r.value / max) * 100)}%`, background: r.color ?? color }} />
          </div>
        </li>
      ))}
    </ul>
  );
}
