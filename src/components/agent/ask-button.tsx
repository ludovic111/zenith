"use client";

import { useRouter } from "next/navigation";
import { useState } from "react";
import { LoaderCircle, Sparkles } from "lucide-react";
import { askZenith } from "./client";

/** One button, one request: starts an agent on a prepared prompt and opens its thread. */
export function AskButton({ prompt, target, label }: { prompt: string; target?: string; label: string }) {
  const router = useRouter();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  return (
    <span className="inline-flex flex-col gap-2">
      <button
        type="button"
        disabled={busy}
        onClick={async () => {
          setBusy(true);
          setError(null);
          try {
            router.push((await askZenith({ prompt, target })).href);
          } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
            setBusy(false);
          }
        }}
        className="inline-flex items-center gap-2 rounded-full bg-sun px-4 py-2 text-sm font-medium text-[#1a1204] shadow-[0_0_28px_-6px_#FFD166] transition hover:scale-[1.03] disabled:opacity-70"
      >
        {busy ? <LoaderCircle className="size-4 animate-spin" /> : <Sparkles className="size-4" />}
        {label}
      </button>
      {error && <span className="text-xs text-bad">{error}</span>}
    </span>
  );
}
