"use client";

import { useRouter } from "next/navigation";
import { useEffect, useRef, useState } from "react";
import { ChevronDown, LoaderCircle, SquarePen } from "lucide-react";
import { askZenith, openAsk } from "@/components/agent/client";
import { cn } from "@/lib/utils";

export type QuickAsk = { label: string; prompt: string };

/**
 * A project header's agent entry: "Ask zenith about <project>" opens the ask box aimed at the
 * project; the chevron lists ready-made requests that start at once.
 */
export function ProjectAsk({ id, label, quick, moreLabel }: { id: string; label: string; quick: QuickAsk[]; moreLabel: string }) {
  const router = useRouter();
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const close = (e: MouseEvent | KeyboardEvent) => {
      if (e instanceof KeyboardEvent ? e.key === "Escape" : !ref.current?.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", close);
    document.addEventListener("keydown", close);
    return () => {
      document.removeEventListener("mousedown", close);
      document.removeEventListener("keydown", close);
    };
  }, [open]);

  const run = async (q: QuickAsk) => {
    setBusy(q.label);
    setError(null);
    try {
      router.push((await askZenith({ prompt: q.prompt, target: id })).href);
      setOpen(false);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(null);
    }
  };

  return (
    <div ref={ref} className="relative flex">
      <button
        type="button"
        onClick={() => openAsk(`@${id} `)}
        className="inline-flex h-7 items-center gap-1.5 rounded-l-md border border-line bg-surface px-2 text-xs font-medium text-ink-2 transition-colors hover:bg-hover hover:text-ink"
      >
        <SquarePen className="size-3.5" />
        {label}
      </button>
      <button
        type="button"
        aria-label={moreLabel}
        aria-expanded={open}
        onClick={() => setOpen((o) => !o)}
        className={cn(
          "inline-flex h-7 w-6 items-center justify-center rounded-r-md border border-l-0 border-line bg-surface text-ink-3 transition-colors hover:bg-hover hover:text-ink",
          open && "bg-hover text-ink",
        )}
      >
        <ChevronDown className="size-3.5" />
      </button>
      {open && (
        <div role="menu" className="absolute right-0 top-full z-30 mt-1 w-72 rounded-lg border border-line bg-popover p-1 shadow-lg">
          {quick.map((q) => (
            <button
              key={q.label}
              role="menuitem"
              type="button"
              disabled={!!busy}
              title={q.prompt}
              onClick={() => run(q)}
              className="flex h-8 w-full items-center gap-2 rounded-md px-2 text-left text-[13px] text-ink-2 transition-colors hover:bg-hover hover:text-ink disabled:opacity-60"
            >
              {busy === q.label ? <LoaderCircle className="size-3.5 shrink-0 animate-spin text-ink-3" /> : <SquarePen className="size-3.5 shrink-0 text-ink-3" />}
              <span className="truncate">{q.label}</span>
            </button>
          ))}
          {error && <p className="px-2 py-1.5 text-xs text-bad">{error}</p>}
        </div>
      )}
    </div>
  );
}
