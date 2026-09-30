"use client";

import { useRouter } from "next/navigation";
import { useState } from "react";
import { LoaderCircle, SquarePen } from "lucide-react";
import { cn } from "@/lib/utils";
import { askZenith, openAsk } from "./client";

/**
 * One button, one request: starts an agent on a prepared prompt and opens its thread.
 * `primary` for a page's main action, `quiet` (the default) for actions on rows and cards.
 * With `edit`, it opens the ask box prefilled instead, so you can adjust the words first.
 */
export function AskButton({
  prompt,
  target,
  label,
  variant = "quiet",
  edit = false,
  className,
}: {
  prompt: string;
  target?: string;
  label: string;
  variant?: "primary" | "quiet";
  edit?: boolean;
  className?: string;
}) {
  const router = useRouter();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  return (
    <span className={cn("inline-flex flex-col gap-1", className)}>
      <button
        type="button"
        disabled={busy}
        title={prompt}
        onClick={async () => {
          if (edit) return openAsk(prompt);
          setBusy(true);
          setError(null);
          try {
            router.push((await askZenith({ prompt, target })).href);
          } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
            setBusy(false);
          }
        }}
        className={cn(
          "inline-flex items-center gap-1.5 whitespace-nowrap rounded-md font-medium transition disabled:opacity-60",
          variant === "primary"
            ? "h-8 bg-primary px-3 text-[13px] text-primary-foreground hover:brightness-110"
            : "h-7 border border-line bg-surface px-2 text-xs text-ink-2 hover:bg-hover hover:text-ink",
        )}
      >
        {busy ? <LoaderCircle className="size-3.5 animate-spin" /> : <SquarePen className="size-3.5" />}
        {label}
      </button>
      {error && <span className="text-xs text-bad">{error}</span>}
    </span>
  );
}
