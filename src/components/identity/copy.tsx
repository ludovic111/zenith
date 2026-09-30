"use client";

import { useState } from "react";
import { Check, Copy as CopyIcon } from "lucide-react";
import { cn } from "@/lib/utils";
import { tr } from "@/lib/i18n";

/** A value copied in one click (address, id, domain…). `wrap` lets long values (commands, JSON) run on several lines. */
export function Copy({ value, children, className, mono, wrap }: { value: string; children?: React.ReactNode; className?: string; mono?: boolean; wrap?: boolean }) {
  const [done, setDone] = useState(false);
  return (
    <button
      type="button"
      onClick={async () => {
        await navigator.clipboard.writeText(value);
        setDone(true);
        setTimeout(() => setDone(false), 1200);
      }}
      className={cn(
        "group inline-flex max-w-full gap-1.5 rounded-sm text-left text-ink-2 transition-colors hover:text-ink focus-visible:outline-2 focus-visible:outline-primary",
        wrap ? "items-start" : "items-center",
        mono && "font-mono text-xs",
        className,
      )}
      title={done ? tr("Copié", "Copied") : tr("Copier", "Copy")}
    >
      <span className={wrap ? "min-w-0 flex-1 whitespace-pre-wrap break-all" : "truncate"}>{children ?? value}</span>
      {done ? (
        <Check className={cn("size-3 shrink-0 text-good", wrap && "mt-0.5")} />
      ) : (
        <CopyIcon className={cn("size-3 shrink-0 text-ink-3 opacity-0 transition-opacity group-hover:opacity-100 group-focus-visible:opacity-100", wrap && "mt-0.5")} />
      )}
    </button>
  );
}
