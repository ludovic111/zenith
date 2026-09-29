"use client";

import NumberFlow, { type Format } from "@number-flow/react";
import { cn } from "@/lib/utils";
import { l10n } from "@/lib/i18n";

export function Stat({
  label,
  value,
  format,
  suffix,
  prefix,
  hint,
  color,
  big,
  className,
}: {
  label: React.ReactNode;
  value: number | null | undefined;
  format?: Format;
  suffix?: string;
  prefix?: string;
  hint?: React.ReactNode;
  color?: string;
  big?: boolean;
  className?: string;
}) {
  return (
    <div className={cn("min-w-0", className)}>
      <div className="flex items-center gap-2 text-[11px] font-medium uppercase tracking-[0.18em] text-ink-3">
        {color && <span className="size-1.5 rounded-full" style={{ background: color }} />}
        {label}
      </div>
      <div className={cn("mt-1.5 font-display font-medium tracking-tight text-ink tabular", big ? "text-4xl sm:text-5xl" : "text-2xl sm:text-3xl")}>
        {value == null ? (
          <span className="text-ink-3">—</span>
        ) : (
          <NumberFlow value={value} format={format} locales={l10n().locale} prefix={prefix} suffix={suffix} willChange />
        )}
      </div>
      {hint && <div className="mt-1 truncate text-xs text-ink-3">{hint}</div>}
    </div>
  );
}
