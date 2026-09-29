"use client";

import Link from "next/link";
import { useRouter } from "next/navigation";
import { useState } from "react";
import { CalendarClock, LoaderCircle, Play } from "lucide-react";
import { ago } from "@/lib/format";
import { tr } from "@/lib/i18n";
import { threadHref } from "@/components/code/store";

export type RoutineView = {
  id: string;
  title: string;
  at: string;
  days: number[];
  enabled: boolean;
  target: string;
  last: { at: string; threadId?: string; environmentId?: string; error?: string } | null;
};

const DAY = () => [tr("lun", "Mon"), tr("mar", "Tue"), tr("mer", "Wed"), tr("jeu", "Thu"), tr("ven", "Fri"), tr("sam", "Sat"), tr("dim", "Sun")];

function when(r: RoutineView) {
  const days =
    r.days.length === 7
      ? tr("tous les jours", "every day")
      : r.days.join() === "1,2,3,4,5"
        ? tr("en semaine", "on weekdays")
        : r.days.map((d) => DAY()[d - 1]).join(", ");
  return tr(`${days} à ${r.at}`, `${days} at ${r.at}`);
}

/** Agents that run on their own, once a day: when, where, their last run, and a way to run one now. */
export function RoutinesList({ routines }: { routines: RoutineView[] }) {
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
    <ul className="divide-y divide-line">
      {routines.map((r) => (
        <li key={r.id} className="flex items-center gap-3 py-3">
          <span className="grid size-9 shrink-0 place-items-center rounded-xl bg-sun/10 text-sun">
            <CalendarClock className="size-4" />
          </span>
          <div className="min-w-0 flex-1">
            <div className="truncate text-sm text-ink">
              {r.title}
              {!r.enabled && <span className="ml-2 text-xs text-ink-3">{tr("en pause", "paused")}</span>}
            </div>
            <div className="truncate text-xs text-ink-3">
              {when(r)} · {r.target}
              {r.last && (
                <>
                  {" · "}
                  {r.last.error ? (
                    <span className="text-bad">{tr(`échec ${ago(r.last.at)}`, `failed ${ago(r.last.at)}`)}</span>
                  ) : r.last.threadId && r.last.environmentId ? (
                    <Link href={threadHref({ environmentId: r.last.environmentId, id: r.last.threadId })} className="text-ink-2 underline-offset-4 hover:underline">
                      {tr(`dernière fois ${ago(r.last.at)}`, `last run ${ago(r.last.at)}`)}
                    </Link>
                  ) : null}
                </>
              )}
            </div>
          </div>
          <button
            type="button"
            onClick={() => run(r.id)}
            disabled={busy !== null}
            className="inline-flex items-center gap-1.5 rounded-full border border-white/10 bg-white/[0.04] px-3 py-1.5 text-xs text-ink-2 transition hover:border-white/20 hover:text-ink disabled:opacity-60"
          >
            {busy === r.id ? <LoaderCircle className="size-3.5 animate-spin" /> : <Play className="size-3.5" />}
            {tr("Lancer", "Run")}
          </button>
        </li>
      ))}
      {error && <li className="py-2 text-xs text-bad">{error}</li>}
    </ul>
  );
}
