"use client";

import { useState } from "react";
import { Check, Copy as CopyIcon } from "lucide-react";
import { cn } from "@/lib/utils";
import { tr } from "@/lib/i18n";

/** A value copied in one click (address, id, domain…). */
export function Copy({ value, children, className, mono }: { value: string; children?: React.ReactNode; className?: string; mono?: boolean }) {
  const [done, setDone] = useState(false);
  return (
    <button
      type="button"
      onClick={async () => {
        await navigator.clipboard.writeText(value);
        setDone(true);
        setTimeout(() => setDone(false), 1200);
      }}
      className={cn("group inline-flex max-w-full items-center gap-1.5 text-left text-ink-2 transition hover:text-ink", mono && "font-mono text-xs", className)}
      title={tr("Copier", "Copy")}
    >
      <span className="truncate">{children ?? value}</span>
      {done ? <Check className="size-3 shrink-0 text-good" /> : <CopyIcon className="size-3 shrink-0 opacity-0 transition group-hover:opacity-60" />}
    </button>
  );
}
