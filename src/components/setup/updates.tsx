"use client";

import { useRouter } from "next/navigation";
import { useEffect, useState } from "react";
import { ArrowDownToLine, LoaderCircle, RefreshCw } from "lucide-react";
import { ago } from "@/lib/format";
import { tr } from "@/lib/i18n";
import type { UpdateState } from "@/lib/updater";
import { saveConfig } from "./api";
import { Button, Switch } from "./fields";

const STEP = (): Record<string, string> => ({
  pull: tr("récupération", "pulling"),
  install: tr("installation des dépendances", "installing dependencies"),
  build: tr("construction", "building"),
  code: tr("construction de zenith code", "building zenith code"),
  restart: tr("redémarrage", "restarting"),
});

/** Version, what's new upstream, and the way to get it: on its own, or now. */
export function UpdatePanel({ initial, auto, selfRestarts }: { initial: UpdateState; auto: boolean; selfRestarts: boolean }) {
  const router = useRouter();
  const [s, setS] = useState(initial);
  const [on, setOn] = useState(auto);
  const [busy, setBusy] = useState<null | "check" | "apply">(null);
  const [error, setError] = useState<string | null>(null);
  const moving = s.state === "updating" || s.state === "waiting";

  // While it works, follow it; the app restarts on its own at the end.
  useEffect(() => {
    if (!moving) return;
    const t = window.setInterval(async () => {
      const r = await fetch("/api/update").then((x) => (x.ok ? x.json() : null)).catch(() => null);
      if (r) setS(r);
    }, 3000);
    return () => window.clearInterval(t);
  }, [moving]);

  const act = async (action: "check" | "apply") => {
    setBusy(action);
    setError(null);
    try {
      const res = await fetch("/api/update", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ action }) });
      const data = (await res.json()) as UpdateState & { error?: string };
      if (!res.ok) throw new Error(data.error ?? `HTTP ${res.status}`);
      setS(data);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(null);
    }
  };

  const status =
    s.state === "updating"
      ? tr(`Mise à jour en cours · ${STEP()[s.step ?? "build"] ?? s.step}…`, `Updating · ${STEP()[s.step ?? "build"] ?? s.step}…`)
      : s.state === "waiting"
        ? tr("Prête : elle s'installera dès que tes agents auront fini leur travail.", "Ready: it will install as soon as your agents finish their work.")
        : s.state === "restart"
          ? tr("Prête : redémarre zenith pour l'utiliser.", "Ready: restart zenith to use it.")
          : s.state === "error"
            ? tr(`Échec : ${s.error ?? "inconnu"}. La version actuelle continue de tourner.`, `Failed: ${s.error ?? "unknown"}. The current version keeps running.`)
            : s.behind
              ? tr(`${s.behind} nouveauté${s.behind > 1 ? "s" : ""} sur GitHub`, `${s.behind} new change${s.behind === 1 ? "" : "s"} on GitHub`)
              : tr("À jour", "Up to date");

  return (
    <div className="divide-y divide-line overflow-hidden rounded-xl border border-line bg-surface">
      <div className="flex flex-wrap items-center gap-x-6 gap-y-2 px-4 py-3">
        <div className="min-w-0 flex-1">
          <div className="text-[13px] text-ink">{status}</div>
          <div className="mt-0.5 truncate text-xs text-ink-3" suppressHydrationWarning>
            {s.current ? `${s.current.sha} · ${s.current.subject}` : tr("Version inconnue", "Unknown version")}
            {s.checkedAt && ` · ${tr("vérifié", "checked")} ${ago(s.checkedAt)}`}
          </div>
        </div>
        <div className="flex items-center gap-2">
          <Button onClick={() => act("check")} disabled={!!busy || moving}>
            {busy === "check" ? <LoaderCircle className="size-3.5 animate-spin" /> : <RefreshCw className="size-3.5" />}
            {tr("Vérifier", "Check")}
          </Button>
          {!!s.behind && !moving && s.state !== "restart" && (
            <Button variant="primary" onClick={() => act("apply")} disabled={!!busy || !!s.blocked}>
              {busy === "apply" ? <LoaderCircle className="size-3.5 animate-spin" /> : <ArrowDownToLine className="size-3.5" />}
              {tr("Mettre à jour", "Update")}
            </Button>
          )}
        </div>
      </div>
      {!!s.incoming?.length && (
        <ul className="max-h-48 space-y-1 overflow-y-auto px-4 py-3 text-xs text-ink-2">
          {s.incoming.map((c) => (
            <li key={c.sha} className="flex gap-2">
              <span className="font-mono text-ink-3">{c.sha}</span>
              <span className="min-w-0 truncate">{c.subject}</span>
            </li>
          ))}
        </ul>
      )}
      {(s.blocked || error) && <p className="px-4 py-2.5 text-xs text-bad">{error ?? tr(`Pas de mise à jour automatique ici : ${s.blocked}.`, `No automatic update here: ${s.blocked}.`)}</p>}
      <div className="flex items-center justify-between gap-6 px-4 py-3">
        <div className="min-w-0">
          <div className="text-[13px] text-ink">{tr("Automatiques", "Automatic")}</div>
          <div className="mt-0.5 text-xs text-ink-3">
            {selfRestarts
              ? tr("Vérifie toutes les 6 heures, construit à côté de la version en cours, redémarre quand aucun agent ne travaille. Jamais par-dessus tes modifications.", "Checks every 6 hours, builds beside the running version, restarts when no agent is working. Never over your own changes.")
              : tr("L'app installée (npm run mac:install) se met à jour seule ; ici, tu lances la mise à jour et tu redémarres toi-même.", "The installed app (npm run mac:install) updates itself; here, you start the update and restart yourself.")}
          </div>
        </div>
        <Switch
          on={on}
          label={tr("Mises à jour automatiques", "Automatic updates")}
          onChange={async (v) => {
            setOn(v);
            try {
              await saveConfig({ "updates.auto": v });
              router.refresh();
            } catch (e) {
              setOn(!v);
              setError(e instanceof Error ? e.message : String(e));
            }
          }}
        />
      </div>
    </div>
  );
}
