import Link from "next/link";
import { PlugZap, TriangleAlert } from "lucide-react";
import type { Source } from "@/lib/source";
import { tr } from "@/lib/i18n";

/** Renders a source's data, or explains what is missing to get it. */
export function Gate<T>({ src, children, compact }: { src: Source<T>; children: (data: T) => React.ReactNode; compact?: boolean }) {
  if (src.ok) return <>{children(src.data)}</>;
  if (src.missing)
    return (
      <div className={compact ? "text-xs text-ink-3" : "flex items-start gap-3 rounded-2xl border border-dashed border-line p-4 text-sm text-ink-2"}>
        {!compact && <PlugZap className="mt-0.5 size-4 shrink-0 text-sun" />}
        <div>
          {tr("À brancher : ", "To connect: ")}<code className="font-mono text-xs text-sun">{src.missing.join(", ")}</code> {tr("dans", "in")}{" "}
          {/* Env vars are UPPER_CASE; anything else is a field of zenith.config.json. */}
          <code className="font-mono text-xs">{src.missing.every((m) => /^[A-Z0-9_]+$/.test(m)) ? ".env.local" : "zenith.config.json"}</code>.{" "}
          <Link href="/reglages" className="text-ink underline underline-offset-4">
            {tr("Comment faire", "How to")}
          </Link>
        </div>
      </div>
    );
  return (
    <div className={compact ? "text-xs text-bad" : "flex items-start gap-3 rounded-2xl border border-bad/30 bg-bad/5 p-4 text-sm text-ink-2"}>
      {!compact && <TriangleAlert className="mt-0.5 size-4 shrink-0 text-bad" />}
      <div className="min-w-0 break-words">{tr("Source injoignable : ", "Source unreachable: ")}{src.error}</div>
    </div>
  );
}
