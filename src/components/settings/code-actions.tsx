"use client";

import Link from "next/link";
import { useState } from "react";
import { RotateCw } from "lucide-react";
import { tr } from "@/lib/i18n";
import { useCode } from "@/components/code/store";
import { Status } from "@/components/z/status";

const quiet = "inline-flex h-7 items-center gap-1.5 rounded-md border border-line bg-surface px-2 text-xs text-ink-2 transition-colors hover:bg-hover hover:text-ink disabled:opacity-60";

type Initial = { enabled: boolean; built: boolean; running: boolean; starting: boolean; version: string | null; lastError: string | null };

/** zenith code's state, live from the shell's code host once it has reported, else as the server saw it. */
export function CodeState({ initial }: { initial: Initial }) {
  const live = useCode().status;
  const s = live ?? initial;
  const health = !s.enabled ? "unknown" : s.running ? "up" : s.starting ? "busy" : !s.built ? "warn" : "down";
  const label = !s.enabled
    ? tr("Désactivé", "Disabled")
    : s.running
      ? tr(`En marche${s.version ? ` · v${s.version}` : ""}`, `Running${s.version ? ` · v${s.version}` : ""}`)
      : s.starting
        ? tr("Démarrage…", "Starting…")
        : !s.built
          ? tr("Pas encore compilé", "Not built yet")
          : tr("Arrêté", "Stopped");
  return (
    <span className="inline-flex flex-col items-end gap-0.5">
      <Status health={health} label={label} />
      {s.enabled && !s.running && s.lastError && <span className="max-w-72 truncate text-2xs text-bad" title={s.lastError}>{s.lastError}</span>}
    </span>
  );
}

/** Restart zenith code (the shell's code host listens for the event) and open it. */
export function CodeActions() {
  const [busy, setBusy] = useState(false);
  return (
    <>
      <button
        type="button"
        disabled={busy}
        className={quiet}
        onClick={() => {
          setBusy(true);
          window.dispatchEvent(new Event("zenith:code-restart"));
          setTimeout(() => setBusy(false), 3000);
        }}
      >
        <RotateCw className={busy ? "size-3.5 animate-spin" : "size-3.5"} />
        {busy ? tr("Redémarrage…", "Restarting…") : tr("Redémarrer", "Restart")}
      </button>
      <Link href="/code" className={quiet}>
        {tr("Ouvrir", "Open")}
      </Link>
    </>
  );
}
