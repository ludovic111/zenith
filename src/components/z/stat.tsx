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
      <div className="flex items-center gap-1.5 text-xs text-ink-3">
        {color && <span className="size-1.5 shrink-0 rounded-full" style={{ background: color }} />}
        <span className="truncate">{label}</span>
      </div>
      <div className={cn("mt-1 font-semibold tracking-tight text-ink tabular", big ? "text-3xl" : "text-xl")}>
        {value == null ? (
          <span className="text-ink-3">—</span>
        ) : (
          <NumberFlow value={value} format={format} locales={l10n().locale} prefix={prefix} suffix={suffix} willChange />
        )}
      </div>
      {hint && <div className="mt-0.5 truncate text-xs text-ink-3">{hint}</div>}
    </div>
  );
}
