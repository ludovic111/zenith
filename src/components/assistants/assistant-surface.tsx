"use client";

import { useEffect, useRef, useState, useSyncExternalStore } from "react";
import { ArrowLeft, ArrowRight, ExternalLink, LoaderCircle, PictureInPicture2, RotateCw, ShieldCheck } from "lucide-react";
import { ASSISTANTS, type AssistantId } from "@/lib/assistants";
import { tr } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import { readPref, subscribePrefs, writePref } from "@/lib/prefs";
import { AssistantIcon } from "./assistant-icon";

type Mode = "desktop" | "web";
/** What zenith.app reports (scripts/mac/ZenithApp.swift, `Assistants.report`). */
type ShellState = { app: string; mode: Mode; state: "launching" | "docked" | "detached" | "web" | "not-installed" | "needs-permission" };
type ShellMessage = { type: "assistant"; app: AssistantId | null; mode?: Mode; rect?: { x: number; y: number; width: number; height: number }; action?: string };

const DOWNLOADS: Record<AssistantId, string> = {
  claude: "https://claude.ai/download",
  chatgpt: "https://openai.com/chatgpt/download/",
};

function shell(): { postMessage(message: ShellMessage): void } | undefined {
  return (window as { webkit?: { messageHandlers?: { zenithShell?: { postMessage(message: ShellMessage): void } } } }).webkit?.messageHandlers?.zenithShell;
}

const noSubscribe = () => () => {};

/**
 * Claude or ChatGPT beside zenith's sidebar. In zenith.app, the area under the toolbar is
 * where the shell docks the real desktop app (the page turns transparent there once it is in
 * place), or shows a web view of it. In a browser, neither can be embedded: one click opens it.
 */
export function AssistantSurface({ id }: { id: AssistantId }) {
  const a = ASSISTANTS[id];
  const inShell = useSyncExternalStore(noSubscribe, () => !!shell(), () => null);
  const modeKey = `zenith:assistant-mode:${id}`;
  const mode: Mode = useSyncExternalStore(subscribePrefs, () => (readPref(modeKey) === "web" ? "web" : "desktop"), () => "desktop");
  const [state, setState] = useState<ShellState | null>(null);
  const [attempt, setAttempt] = useState(0);
  const slot = useRef<HTMLDivElement>(null);

  // Where the app goes: the slot's rectangle, sent again whenever the layout moves it.
  useEffect(() => {
    const s = shell();
    const el = slot.current;
    if (!s || !el) return;
    const send = () => {
      const r = el.getBoundingClientRect();
      s.postMessage({ type: "assistant", app: id, mode, rect: { x: r.x, y: r.y, width: r.width, height: r.height } });
    };
    send();
    const ro = new ResizeObserver(send);
    ro.observe(el);
    window.addEventListener("resize", send);
    return () => {
      ro.disconnect();
      window.removeEventListener("resize", send);
      s.postMessage({ type: "assistant", app: null });
      delete document.documentElement.dataset.docked;
    };
  }, [id, mode, attempt]);

  useEffect(() => {
    const onState = (e: Event) => {
      const detail = (e as CustomEvent<ShellState>).detail;
      if (detail?.app !== id) return;
      setState(detail);
      // Transparent only once the app's window sits under the slot.
      if (detail.state === "docked") document.documentElement.dataset.docked = "1";
      else delete document.documentElement.dataset.docked;
    };
    window.addEventListener("zenith-shell", onState);
    return () => window.removeEventListener("zenith-shell", onState);
  }, [id]);

  const act = (action: string) => shell()?.postMessage({ type: "assistant", app: id, action });
  const shown = state?.app === id ? state : null;
  const docked = shown?.state === "docked";
  const web = shown?.mode === "web";

  const openApp = async () => {
    const res = await fetch(`/api/assistants/open?app=${id}`, { method: "POST" }).catch(() => null);
    if (!res?.ok) window.open(a.web, "_blank", "noopener");
  };

  const button = "inline-flex h-7 items-center gap-1.5 rounded-lg px-2 text-xs text-ink-2 transition hover:bg-white/[0.06] hover:text-ink";

  return (
    <div className="fixed inset-x-0 top-0 bottom-[76px] z-30 flex flex-col lg:bottom-0 lg:left-[var(--zenith-sidebar-w)]">
      <header className="flex h-11 shrink-0 items-center gap-2 border-b border-line bg-[var(--surface)] px-3">
        <AssistantIcon id={id} size={16} />
        <h1 className="font-display text-sm font-medium tracking-wide text-ink">{a.name}</h1>
        {shown && <span className="hidden text-xs text-ink-3 sm:inline">{stateLabel(shown, a.name)}</span>}

        <div className="ml-auto flex items-center gap-1">
          {inShell && web && (
            <>
              <button type="button" className={button} onClick={() => act("back")} aria-label={tr("Précédent", "Back")} title={tr("Précédent", "Back")}>
                <ArrowLeft className="size-3.5" />
              </button>
              <button type="button" className={button} onClick={() => act("forward")} aria-label={tr("Suivant", "Forward")} title={tr("Suivant", "Forward")}>
                <ArrowRight className="size-3.5" />
              </button>
              <button type="button" className={button} onClick={() => act("reload")} aria-label={tr("Recharger", "Reload")} title={tr("Recharger", "Reload")}>
                <RotateCw className="size-3.5" />
              </button>
              <button type="button" className={button} onClick={() => act("browser")} title={tr("Ouvrir dans le navigateur", "Open in browser")}>
                <ExternalLink className="size-3.5" />
              </button>
            </>
          )}
          {inShell && docked && (
            <button type="button" className={button} onClick={() => act("detach")} title={tr("Rendre sa propre fenêtre à l'app", "Give the app its own window back")}>
              <PictureInPicture2 className="size-3.5" /> <span className="hidden md:inline">{tr("Détacher", "Detach")}</span>
            </button>
          )}
          {inShell && (
            <div role="radiogroup" aria-label={tr("Affichage", "Display")} className="ml-1 flex rounded-lg border border-line p-0.5 text-xs">
              {(["desktop", "web"] as const).map((m) => (
                <button
                  key={m}
                  type="button"
                  role="radio"
                  aria-checked={mode === m}
                  onClick={() => writePref(modeKey, m)}
                  className={cn("rounded-md px-2 py-1 transition", mode === m ? "bg-white/[0.09] text-ink" : "text-ink-3 hover:text-ink-2")}
                >
                  {m === "desktop" ? "App" : "Web"}
                </button>
              ))}
            </div>
          )}
        </div>
      </header>

      {shown?.state === "needs-permission" && (
        <Notice>
          <ShieldCheck className="size-4 shrink-0 text-sun" />
          <span className="min-w-0 flex-1">
            {tr(
              `Pour intégrer l'app de bureau ${a.name}, autorise zenith dans Réglages Système → Confidentialité et sécurité → Accessibilité. En attendant, voici la version web.`,
              `To dock the ${a.name} desktop app, allow zenith in System Settings → Privacy & Security → Accessibility. Meanwhile, here is the web version.`,
            )}
          </span>
          <button type="button" className={button} onClick={() => act("grant")}>{tr("Autoriser", "Allow")}</button>
          <button type="button" className={button} onClick={() => setAttempt((n) => n + 1)}>{tr("Réessayer", "Retry")}</button>
        </Notice>
      )}
      {shown?.state === "not-installed" && (
        <Notice>
          <span className="min-w-0 flex-1">{tr(`L'app de bureau ${a.name} n'est pas installée : voici la version web.`, `The ${a.name} desktop app isn't installed: here is the web version.`)}</span>
          <a href={DOWNLOADS[id]} target="_blank" rel="noopener noreferrer" className={button}>{tr("Télécharger l'app", "Get the app")}</a>
          <button type="button" className={button} onClick={() => setAttempt((n) => n + 1)}>{tr("Réessayer", "Retry")}</button>
        </Notice>
      )}

      <div ref={slot} className={cn("relative min-h-0 flex-1", !docked && "bg-[var(--surface)]")}>
        {inShell === false && (
          <Center>
            <AssistantIcon id={id} size={28} className="mx-auto" />
            <h2 className="mt-4 font-display text-base font-medium text-ink">{tr(`${a.name} vit dans zenith.app`, `${a.name} lives in zenith.app`)}</h2>
            <p className="mt-2 text-sm leading-relaxed text-ink-3">
              {tr(
                `Dans l'app Mac de zenith, l'app de bureau ${a.name} s'installe ici, à côté de la barre latérale. Un navigateur ne peut pas l'intégrer : ouvre-la à part.`,
                `In zenith's Mac app, the ${a.name} desktop app sits right here, beside the sidebar. A browser can't embed it: open it on its own.`,
              )}
            </p>
            <div className="mt-5 flex justify-center gap-2">
              <button type="button" onClick={openApp} className="rounded-full border border-line px-4 py-1.5 text-sm text-ink transition hover:bg-white/[0.06]">
                {tr(`Ouvrir l'app ${a.name}`, `Open the ${a.name} app`)}
              </button>
              <a href={a.web} target="_blank" rel="noopener noreferrer" className="rounded-full px-4 py-1.5 text-sm text-ink-2 transition hover:bg-white/[0.06] hover:text-ink">
                {tr("Version web", "Web version")}
              </a>
            </div>
          </Center>
        )}
        {inShell && (!shown || shown.state === "launching") && (
          <Center>
            <LoaderCircle className="mx-auto size-5 animate-spin text-ink-3" />
            <p className="mt-3 text-sm text-ink-3">{tr(`${a.name} arrive…`, `${a.name} is coming…`)}</p>
          </Center>
        )}
        {shown?.state === "detached" && (
          <Center>
            <AssistantIcon id={id} size={28} className="mx-auto" />
            <p className="mt-4 text-sm text-ink-3">{tr(`${a.name} est dans sa propre fenêtre.`, `${a.name} is in its own window.`)}</p>
            <div className="mt-4 flex justify-center gap-2">
              <button type="button" onClick={() => setAttempt((n) => n + 1)} className="rounded-full border border-line px-4 py-1.5 text-sm text-ink transition hover:bg-white/[0.06]">
                {tr("Rattacher ici", "Dock it here")}
              </button>
              <button type="button" onClick={() => act("focus")} className="rounded-full px-4 py-1.5 text-sm text-ink-2 transition hover:bg-white/[0.06] hover:text-ink">
                {tr("Aller à la fenêtre", "Go to its window")}
              </button>
            </div>
          </Center>
        )}
      </div>
    </div>
  );
}

function stateLabel(s: ShellState, name: string) {
  switch (s.state) {
    case "docked":
      return tr("app de bureau", "desktop app");
    case "launching":
      return tr(`ouverture de ${name}…`, `opening ${name}…`);
    case "detached":
      return tr("fenêtre séparée", "separate window");
    default:
      return tr("version web", "web version");
  }
}

function Notice({ children }: { children: React.ReactNode }) {
  return <div className="flex shrink-0 items-center gap-3 border-b border-line bg-[#121020] px-4 py-2 text-xs text-ink-2">{children}</div>;
}

function Center({ children }: { children: React.ReactNode }) {
  return (
    <div className="grid h-full place-items-center p-6">
      <div className="max-w-md text-center">{children}</div>
    </div>
  );
}
