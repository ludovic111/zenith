"use client";

import Link from "next/link";
import { useRouter } from "next/navigation";
import { useState } from "react";
import { CalendarClock, LoaderCircle, Play, Radar } from "lucide-react";
import { ago, date } from "@/lib/format";
import { tr } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import { threadHref } from "@/components/code/store";

export type RoutineView = {
  id: string;
  title: string;
  at: string | null;
  /** The Now kinds it acts on, named, when it runs on an event. */
  on: string[] | null;
  days: number[];
  enabled: boolean;
  target: string;
  last: { at: string; threadId?: string; environmentId?: string; error?: string; item?: string } | null;
  /** Next scheduled run (ISO), null when paused. */
  next?: string | null;
};

const DAY = () => [tr("lun", "Mon"), tr("mar", "Tue"), tr("mer", "Wed"), tr("jeu", "Thu"), tr("ven", "Fri"), tr("sam", "Sat"), tr("dim", "Sun")];

function when(r: RoutineView) {
  if (r.on) return tr(`à chaque ${r.on.join(", ")}`, `on each ${r.on.join(", ")}`);
  const days =
    r.days.length === 7
      ? tr("tous les jours", "every day")
      : r.days.join() === "1,2,3,4,5"
        ? tr("en semaine", "on weekdays")
        : r.days.map((d) => DAY()[d - 1]).join(", ");
  return tr(`${days} à ${r.at}`, `${days} at ${r.at}`);
}

/** Agents that run on their own, at a set time or on an event: when, who, their last and next run, and a way to run one now. */
export function RoutinesList({ routines, className }: { routines: RoutineView[]; className?: string }) {
  const router = useRouter();
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  async function run(id: string) {
    setBusy(id);
    setError(null);
    try {
      const res = await fetch("/api/agent/routine", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ id }) });
      const data = (await res.json()) as { href?: string; error?: string };
      if (!res.ok || !data.href) throw new Error(data.error ?? `HTTP ${res.status}`);
      router.push(data.href);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setBusy(null);
    }
  }

  return (
    <ul className={cn("divide-y divide-line", className)}>
      {routines.map((r) => (
        <li key={r.id} className="flex items-center gap-3 px-4 py-2.5">
          {r.on ? (
            <Radar className={cn("size-4 shrink-0", r.enabled ? "text-ink-3" : "text-ink-3/50")} />
          ) : (
            <CalendarClock className={cn("size-4 shrink-0", r.enabled ? "text-ink-3" : "text-ink-3/50")} />
          )}
          <div className="min-w-0 flex-1">
            <div className="flex items-center gap-2 text-[13px]">
              <span className={cn("truncate", r.enabled ? "text-ink" : "text-ink-3")}>{r.title}</span>
              {!r.enabled && <span className="shrink-0 rounded border border-line px-1 text-2xs text-ink-3">{tr("en pause", "paused")}</span>}
            </div>
            <div className="truncate text-xs text-ink-3" suppressHydrationWarning>
              {when(r)}
              {r.target && ` · ${r.target}`}
              {r.last && (
                <>
                  {" · "}
                  {r.last.error ? (
                    <span className="text-bad" title={r.last.error}>{tr(`échec ${ago(r.last.at)}`, `failed ${ago(r.last.at)}`)}</span>
                  ) : r.last.threadId && r.last.environmentId ? (
                    <Link href={threadHref({ environmentId: r.last.environmentId, id: r.last.threadId })} className="text-ink-2 underline-offset-4 hover:underline">
                      {tr(`dernière ${ago(r.last.at)}`, `last ${ago(r.last.at)}`)}
                    </Link>
                  ) : (
                    tr(`dernière ${ago(r.last.at)}`, `last ${ago(r.last.at)}`)
                  )}
                </>
              )}
            </div>
          </div>
          {r.next && (
            <span className="hidden shrink-0 text-right text-xs text-ink-3 tabular sm:block" suppressHydrationWarning title={tr("Prochaine exécution", "Next run")}>
              {date(r.next, { weekday: "short", hour: "2-digit", minute: "2-digit" })}
            </span>
          )}
          <button
            type="button"
            onClick={() => run(r.id)}
            disabled={busy !== null}
            title={tr("Lancer maintenant", "Run now")}
            className="inline-flex h-7 shrink-0 items-center gap-1.5 rounded-md border border-line bg-surface px-2 text-xs text-ink-2 transition-colors hover:bg-hover hover:text-ink disabled:opacity-60"
          >
            {busy === r.id ? <LoaderCircle className="size-3.5 animate-spin" /> : <Play className="size-3.5" />}
            {tr("Lancer", "Run")}
          </button>
        </li>
      ))}
      {error && <li className="px-4 py-2 text-xs text-bad">{error}</li>}
    </ul>
  );
}
