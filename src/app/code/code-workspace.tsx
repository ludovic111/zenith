"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { ExternalLink, LoaderCircle, RotateCw, SquareTerminal } from "lucide-react";
import type { CodeStatus } from "@/lib/code/manager";
import type { CodeTarget } from "@/lib/code/target";
import { LiveDot, type Health } from "@/components/z/status";
import { Chip } from "@/components/z/panel";
import { tr } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import { BRAND, CODE_BRAND } from "@/lib/code/brand";

// Messages exchanged with the embedded app (code/apps/web/src/zenith/embed.ts).
const MSG = {
  pairRequest: "zenith-code:pair-request",
  pairToken: "zenith-code:pair-token",
  pairError: "zenith-code:pair-error",
  openProject: "zenith-code:open-project",
  ready: "zenith-code:ready",
} as const;

function health(s: CodeStatus): Health {
  if (s.running) return "up";
  if (s.starting) return "busy";
  if (s.enabled && s.built) return "down";
  return "unknown";
}

function statusLabel(s: CodeStatus) {
  if (!s.enabled) return tr("Désactivé", "Disabled");
  if (!s.built) return tr("Pas encore construit", "Not built yet");
  if (s.running) return s.version ?? tr("En marche", "Running");
  if (s.starting) return s.restarts ? tr("Redémarrage…", "Restarting…") : tr("Démarrage…", "Starting…");
  return tr("Arrêté", "Stopped");
}

const iframeSrc = (s: CodeStatus, target: CodeTarget | null) =>
  target ? `${s.origin}/?zenithProject=${encodeURIComponent(target.dir)}` : `${s.origin}/`;

export function CodeWorkspace({ initialStatus, target }: { initialStatus: CodeStatus; target: CodeTarget | null }) {
  const [status, setStatus] = useState(initialStatus);
  const [restarting, setRestarting] = useState(false);
  // The iframe keeps its first URL; later project changes go through postMessage.
  const [src, setSrc] = useState(() => iframeSrc(initialStatus, target));
  const [frameKey, setFrameKey] = useState(0);
  const frame = useRef<HTMLIFrameElement>(null);
  const shownTarget = useRef<string | null>(target?.dir ?? null);
  const origin = status.origin;

  const refresh = useCallback(async () => {
    const res = await fetch("/api/code", { cache: "no-store" }).catch(() => null);
    if (res?.ok) setStatus((await res.json()) as CodeStatus);
  }, []);

  // Poll quickly while starting, slowly once running.
  useEffect(() => {
    const id = setInterval(refresh, status.running ? 15_000 : 2_000);
    return () => clearInterval(id);
  }, [refresh, status.running]);

  const post = useCallback(
    (message: Record<string, unknown>) => frame.current?.contentWindow?.postMessage(message, origin),
    [origin],
  );

  // Pairing requests from the embedded app: only from our iframe, only from its origin.
  useEffect(() => {
    const onMessage = async (event: MessageEvent) => {
      if (event.origin !== origin || event.source !== frame.current?.contentWindow) return;
      const type = (event.data as { type?: unknown } | null)?.type;
      if (type === MSG.pairRequest) {
        const res = await fetch("/api/code/pair", { method: "POST", cache: "no-store" }).catch(() => null);
        const body = (await res?.json().catch(() => null)) as { token?: string; error?: string } | null;
        if (res?.ok && body?.token) post({ type: MSG.pairToken, token: body.token });
        else post({ type: MSG.pairError, error: body?.error ?? tr(`${BRAND} n'a pas pu créer de jeton.`, `${BRAND} could not create a token.`) });
      } else if (type === MSG.ready && target && shownTarget.current !== target.dir) {
        shownTarget.current = target.dir;
        post({ type: MSG.openProject, path: target.dir });
      }
    };
    window.addEventListener("message", onMessage);
    return () => window.removeEventListener("message", onMessage);
  }, [origin, post, target]);

  // /code?project=a → /code?project=b keeps the iframe and asks it to switch project.
  useEffect(() => {
    if (!target || shownTarget.current === target.dir) return;
    shownTarget.current = target.dir;
    post({ type: MSG.openProject, path: target.dir });
  }, [target, post]);

  const restart = async () => {
    setRestarting(true);
    const res = await fetch("/api/code/restart", { method: "POST" }).catch(() => null);
    if (res?.ok) setStatus((await res.json()) as CodeStatus);
    setRestarting(false);
    setSrc(iframeSrc(status, target));
    setFrameKey((k) => k + 1);
  };

  const openHref = `/api/code/open${target ? `?project=${encodeURIComponent(target.id)}` : ""}`;

  return (
    <div className="-mx-4 -mb-16 -mt-4 flex h-dvh flex-col sm:-mx-6 lg:-mx-10 lg:-mt-8">
      <header className="flex h-12 shrink-0 items-center gap-3 border-b border-line bg-[#0b0a14]/80 px-4 backdrop-blur-md">
        <LiveDot health={health(status)} />
        <h1 className="font-display text-sm font-medium tracking-wide text-ink">{CODE_BRAND}</h1>
        {target && <Chip>{target.name}</Chip>}
        <span className="hidden truncate text-xs text-ink-3 sm:inline">{statusLabel(status)}</span>
        <div className="ml-auto flex items-center gap-1">
          {status.running && (
            <a
              href={openHref}
              target="_blank"
              rel="noopener"
              className="inline-flex items-center gap-1.5 rounded-full px-3 py-1.5 text-xs text-ink-2 transition hover:bg-white/[0.06] hover:text-ink"
            >
              <ExternalLink className="size-3.5" />
              <span className="hidden md:inline">{tr("Ouvrir dans sa propre fenêtre", "Open in its own window")}</span>
            </a>
          )}
          {status.enabled && status.built && (
            <button
              type="button"
              onClick={restart}
              disabled={restarting}
              className="inline-flex items-center gap-1.5 rounded-full px-3 py-1.5 text-xs text-ink-2 transition hover:bg-white/[0.06] hover:text-ink disabled:opacity-50"
            >
              <RotateCw className={cn("size-3.5", restarting && "animate-spin")} />
              <span className="hidden md:inline">{tr("Redémarrer", "Restart")}</span>
            </button>
          )}
        </div>
      </header>

      <div className="relative min-h-0 flex-1 bg-[#0b0a14]">
        {status.running ? (
          <iframe
            key={frameKey}
            ref={frame}
            src={src}
            title={CODE_BRAND}
            allow="clipboard-read; clipboard-write; fullscreen"
            className="absolute inset-0 size-full border-0"
          />
        ) : (
          <EmptyState status={status} />
        )}
      </div>
    </div>
  );
}

function EmptyState({ status }: { status: CodeStatus }) {
  let title: string;
  let body: React.ReactNode;
  if (!status.enabled) {
    title = tr(`${CODE_BRAND} est désactivé`, `${CODE_BRAND} is disabled`);
    body = (
      <>
        {tr("Active-le avec ", "Turn it on with ")}
        <code className="font-mono text-ink-2">{'"code": { "enabled": true }'}</code>
        {tr(` dans zenith.config.json, puis relance ${BRAND}.`, ` in zenith.config.json, then restart ${BRAND}.`)}
      </>
    );
  } else if (!status.built) {
    title = tr(`${CODE_BRAND} n'est pas encore construit`, `${CODE_BRAND} isn't built yet`);
    body = (
      <>
        {tr("Lance ", "Run ")}
        <code className="rounded-md bg-white/[0.06] px-1.5 py-0.5 font-mono text-ink">npm run code:build</code>
        {tr(
          ` dans le dossier de ${BRAND} (Node 22.16+ ou 24, pnpm passe par npx), puis relance ${BRAND}.`,
          ` in ${BRAND}'s folder (Node 22.16+ or 24; pnpm comes through npx), then restart ${BRAND}.`,
        )}
      </>
    );
  } else if (status.starting) {
    title = tr(`${CODE_BRAND} se lève…`, `${CODE_BRAND} is rising…`);
    body = status.lastError ? tr(`Dernière erreur : ${status.lastError}`, `Last error: ${status.lastError}`) : tr("Quelques secondes.", "A few seconds.");
  } else {
    title = tr(`${CODE_BRAND} est arrêté`, `${CODE_BRAND} is stopped`);
    body = (
      <>
        {status.lastError && <span className="block">{tr(`Dernière erreur : ${status.lastError}`, `Last error: ${status.lastError}`)}</span>}
        {tr("Le journal est dans ", "The log is in ")}
        <code className="font-mono text-ink-2">.data/code.log</code>.
      </>
    );
  }
  return (
    <div className="grid h-full place-items-center p-6">
      <div className="max-w-md text-center">
        <div className="mx-auto mb-4 grid size-12 place-items-center rounded-2xl border border-line bg-white/[0.04] text-ink-2">
          {status.starting ? <LoaderCircle className="size-5 animate-spin" /> : <SquareTerminal className="size-5" />}
        </div>
        <h2 className="font-display text-base font-medium text-ink">{title}</h2>
        <p className="mt-2 text-sm leading-relaxed text-ink-3">{body}</p>
      </div>
    </div>
  );
}
