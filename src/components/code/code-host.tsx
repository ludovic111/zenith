"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { usePathname } from "next/navigation";
import { LoaderCircle, SquareTerminal } from "lucide-react";
import type { CodeStatus } from "@/lib/code/manager";
import { BRAND, CODE_BRAND } from "@/lib/code/brand";
import { tr } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import { appPathOf, codeHref, codeStore, focusCode, MSG, sendToCode, useCode, type CodeSnapshot } from "./store";

const DESKTOP = "(min-width: 1024px)";
// On other pages, the app loads once the dashboard has settled.
const IDLE_MOUNT_MS = 1_500;

/**
 * zenith code, mounted once for the whole dashboard: one iframe that survives page
 * changes (threads keep streaming, terminals keep their state), shown full-bleed on
 * /code and hidden elsewhere. zenith's sidebar lists its threads, so on desktop the
 * app hides its own sidebar. zenith's URL mirrors the app's (`/code/<env>/<thread>`).
 */
export function CodeHost() {
  const pathname = usePathname();
  const appPath = appPathOf(pathname);
  const onCode = appPath !== null;
  const { status, target, ready, snapshot } = useCode();
  const [idle, setIdle] = useState(false);
  const [visited, setVisited] = useState(onCode);
  if (onCode && !visited) setVisited(true);
  const mounted = visited || idle;
  const [frameKey, setFrameKey] = useState(0);
  const frame = useRef<HTMLIFrameElement>(null);
  const lastAppPath = useRef<string | null>(null);
  const origin = status?.origin ?? null;

  const refresh = useCallback(async () => {
    const res = await fetch("/api/code", { cache: "no-store" }).catch(() => null);
    if (res?.ok) codeStore.set({ status: (await res.json()) as CodeStatus });
  }, []);

  // Status: quickly while starting, slowly once running or when off.
  useEffect(() => {
    if (!status) void refresh();
    if (status && !status.enabled) return;
    const every = status?.running ? 15_000 : status?.starting ? 2_000 : 30_000;
    const id = setInterval(refresh, every);
    return () => clearInterval(id);
  }, [refresh, status]);

  useEffect(() => {
    const id = setTimeout(() => setIdle(true), IDLE_MOUNT_MS);
    return () => clearTimeout(id);
  }, []);

  const post = useCallback(
    (message: Record<string, unknown>) => {
      if (origin) frame.current?.contentWindow?.postMessage(message, origin);
    },
    [origin],
  );

  // Messages from our iframe only, from its origin only.
  useEffect(() => {
    if (!origin) return;
    const onMessage = async (event: MessageEvent) => {
      if (event.origin !== origin || event.source !== frame.current?.contentWindow) return;
      const data = event.data as { type?: unknown; snapshot?: CodeSnapshot } | null;
      if (data?.type === MSG.pairRequest) {
        const res = await fetch("/api/code/pair", { method: "POST", cache: "no-store" }).catch(() => null);
        const body = (await res?.json().catch(() => null)) as { token?: string; error?: string } | null;
        if (res?.ok && body?.token) post({ type: MSG.pairToken, token: body.token });
        else post({ type: MSG.pairError, error: body?.error ?? tr(`${BRAND} n'a pas pu créer de jeton.`, `${BRAND} could not create a token.`) });
      } else if (data?.type === MSG.ready) {
        post({ type: MSG.chrome, ownSidebar: !window.matchMedia(DESKTOP).matches });
        codeStore.attach(post);
      } else if (data?.type === MSG.sidebar && data.snapshot) {
        codeStore.set({ snapshot: data.snapshot });
      }
    };
    window.addEventListener("message", onMessage);
    return () => window.removeEventListener("message", onMessage);
  }, [origin, post]);

  // Below lg, zenith's sidebar is a bottom bar: the app brings its own back.
  useEffect(() => {
    const mq = window.matchMedia(DESKTOP);
    const onChange = () => sendToCode({ type: MSG.chrome, ownSidebar: !mq.matches });
    mq.addEventListener("change", onChange);
    return () => mq.removeEventListener("change", onChange);
  }, []);

  // /code?project=<id>: focus that project (its latest thread, or a new one).
  useEffect(() => {
    if (!target || !ready || !status?.running) return;
    codeStore.set({ target: null });
    sendToCode({ type: MSG.openProject, path: target.dir });
  }, [target, ready, status?.running]);

  // zenith URL → app: links, back and forward.
  useEffect(() => {
    if (!appPath || appPath === lastAppPath.current || !ready) return;
    lastAppPath.current = appPath;
    sendToCode({ type: MSG.navigate, request: { to: "path", path: appPath } });
    focusCode();
  }, [appPath, ready]);

  // Focus follows the user into the app once it is on screen.
  useEffect(() => {
    const focus = () => requestAnimationFrame(() => frame.current?.focus());
    window.addEventListener("zenith:code-focus", focus);
    return () => window.removeEventListener("zenith:code-focus", focus);
  }, []);

  // App → zenith URL: the app navigated on its own (new thread, settings…), or zenith
  // shows bare /code and the app is already somewhere.
  const snapshotPath = snapshot?.pathname ?? null;
  const syncedPath = useRef<string | null>(null);
  useEffect(() => {
    if (!snapshotPath || snapshotPath.startsWith("/pair")) return;
    const changed = syncedPath.current !== snapshotPath;
    syncedPath.current = snapshotPath;
    lastAppPath.current = snapshotPath;
    if (appPath === null || (!changed && appPath !== "")) return;
    const next = codeHref(snapshotPath);
    if (next !== window.location.pathname) window.history.replaceState(null, "", next);
  }, [snapshotPath, appPath]);

  const restart = useCallback(async () => {
    const res = await fetch("/api/code/restart", { method: "POST" }).catch(() => null);
    if (res?.ok) codeStore.set({ status: (await res.json()) as CodeStatus });
    setFrameKey((k) => k + 1);
  }, []);

  useEffect(() => {
    window.addEventListener("zenith:code-restart", restart);
    return () => window.removeEventListener("zenith:code-restart", restart);
  }, [restart]);

  if (!status?.enabled) return onCode && status ? <Overlay visible><EmptyState status={status} /></Overlay> : null;

  return (
    <Overlay visible={onCode}>
      {status.running && mounted ? (
        <Frame key={frameKey} frame={frame} origin={status.origin} appPath={appPath} />
      ) : (
        onCode && <EmptyState status={status} />
      )}
    </Overlay>
  );
}

/**
 * The iframe. Its URL is fixed when it mounts (the path zenith shows, the app's chrome, a
 * project to focus); later changes go through postMessage so the app never reloads.
 * Remounted (`key`) to restart it.
 */
function Frame({
  frame,
  origin,
  appPath,
}: {
  frame: React.RefObject<HTMLIFrameElement | null>;
  origin: string;
  appPath: string | null;
}) {
  const [boot] = useState(() => {
    const url = new URL(appPath || "/", origin);
    url.searchParams.set("zenithChrome", window.matchMedia(DESKTOP).matches ? "bare" : "full");
    const target = codeStore.get().target;
    if (target) url.searchParams.set("zenithProject", target.dir);
    return { src: url.toString(), target };
  });

  useEffect(() => {
    if (boot.target && codeStore.get().target === boot.target) codeStore.set({ target: null });
    // Until the new app says ready, nothing can receive messages.
    return () => codeStore.attach(null);
  }, [boot]);

  return (
    <iframe ref={frame} src={boot.src} title={CODE_BRAND} allow="clipboard-read; clipboard-write; fullscreen" className="absolute inset-0 size-full border-0" />
  );
}

/** The area right of the sidebar (above the bottom bar on phones). Hidden, it keeps its size. */
function Overlay({ visible, children }: { visible: boolean; children: React.ReactNode }) {
  return (
    <div
      aria-hidden={!visible}
      inert={!visible}
      className={cn(
        "fixed inset-x-0 top-0 bottom-[76px] z-30 bg-[var(--surface)] lg:bottom-0 lg:left-[var(--zenith-sidebar-w)]",
        !visible && "pointer-events-none invisible",
      )}
    >
      {children}
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
  } else if (status.starting || status.running) {
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
  const busy = status.starting || status.running;
  return (
    <div className="grid h-full place-items-center p-6">
      <div className="max-w-md text-center">
        <div className="mx-auto mb-4 grid size-12 place-items-center rounded-2xl border border-line bg-white/[0.04] text-ink-2">
          {busy ? <LoaderCircle className="size-5 animate-spin" /> : <SquareTerminal className="size-5" />}
        </div>
        <h2 className="font-display text-base font-medium text-ink">{title}</h2>
        <p className="mt-2 text-sm leading-relaxed text-ink-3">{body}</p>
        {status.enabled && status.built && !busy && (
          <button
            type="button"
            onClick={() => window.dispatchEvent(new Event("zenith:code-restart"))}
            className="mt-5 rounded-full border border-line px-4 py-1.5 text-sm text-ink-2 transition hover:bg-white/[0.06] hover:text-ink"
          >
            {tr("Redémarrer", "Restart")}
          </button>
        )}
      </div>
    </div>
  );
}
