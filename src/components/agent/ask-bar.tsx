"use client";

import { useRouter } from "next/navigation";
import { useEffect, useMemo, useRef, useState } from "react";
import { ArrowUp, Check, ChevronDown, CornerDownLeft, LoaderCircle, Wand2 } from "lucide-react";
import { cn } from "@/lib/utils";
import { tr } from "@/lib/i18n";
import { LIFE, guessTarget, type AgentTarget, type Provider } from "@/lib/agent/target";
import { ProviderIcon } from "./provider-icon";
import { askZenith } from "./client";

export type AskBarProps = {
  targets: AgentTarget[];
  provider: Provider;
  /** Sentences shown in turn as the placeholder. */
  examples?: string[];
  /** One-tap starters under the box. */
  suggestions?: string[];
  /** Always this target (a project page). */
  fixedTarget?: string;
  autoFocus?: boolean;
  /** Prefill (the ask dialog opened from ⌘K). */
  initialText?: string;
  onLaunched?: () => void;
  /** No card of its own: the ask dialog draws the surface. */
  bare?: boolean;
  className?: string;
};

const PROVIDERS: { id: Provider; name: string }[] = [
  { id: "claude", name: "Claude" },
  { id: "codex", name: "Codex" },
];

/**
 * "Ask zenith": say what you want, in your words. The destination follows what you name
 * (a project, else your life), the agent starts at once and its thread opens.
 */
export function AskBar({ targets, provider: initialProvider, examples = [], suggestions = [], fixedTarget, autoFocus, initialText = "", onLaunched, bare, className }: AskBarProps) {
  const router = useRouter();
  const [text, setText] = useState(initialText);
  const [chosen, setChosen] = useState<string | null>(fixedTarget ?? null);
  const [provider, setProvider] = useState<Provider>(initialProvider);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [menu, setMenu] = useState(false);
  const [tick, setTick] = useState(0);
  const area = useRef<HTMLTextAreaElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);

  const guessed = useMemo(() => guessTarget(text, targets), [text, targets]);
  const targetId = chosen ?? guessed;
  const target = targets.find((t) => t.id === targetId) ?? targets[0];
  const auto = chosen === null;

  const pool = examples.length ? examples : [tr("Dis-moi ce que tu veux…", "Tell me what you want…")];
  const placeholder = pool[tick % pool.length];

  useEffect(() => {
    if (text || pool.length < 2) return;
    const id = window.setInterval(() => setTick((n) => n + 1), 4200);
    return () => window.clearInterval(id);
  }, [text, pool.length]);

  // Grow with the text, up to a point.
  useEffect(() => {
    const el = area.current;
    if (!el) return;
    el.style.height = "0px";
    el.style.height = `${Math.min(el.scrollHeight, 240)}px`;
  }, [text]);

  // Words typed before the page came alive are kept.
  useEffect(() => {
    const typed = area.current?.value;
    if (typed) setText((t) => t || typed);
  }, []);

  useEffect(() => {
    if (!autoFocus) return;
    const el = area.current;
    el?.focus();
    el?.setSelectionRange(el.value.length, el.value.length);
  }, [autoFocus]);

  useEffect(() => {
    if (!menu) return;
    const close = (e: MouseEvent) => {
      if (!menuRef.current?.contains(e.target as Node)) setMenu(false);
    };
    window.addEventListener("mousedown", close);
    return () => window.removeEventListener("mousedown", close);
  }, [menu]);

  async function launch(prompt = text) {
    const clean = prompt.trim();
    if (!clean || busy) return;
    setBusy(true);
    setError(null);
    try {
      const r = await askZenith({ prompt: clean, target: chosen ?? guessTarget(clean, targets), provider, source: "bar" });
      setText("");
      onLaunched?.();
      router.push(r.href);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className={cn("relative", className)}>
      <form
        onSubmit={(e) => {
          e.preventDefault();
          void launch();
        }}
        aria-busy={busy}
        className={cn(
          "relative",
          !bare && "rounded-2xl border border-line bg-surface shadow-xs transition-colors focus-within:border-ink-3/40 hover:border-ink-3/30",
        )}
      >
        <label className="block px-4 pb-1 pt-3.5">
          <span className="sr-only">{tr("Demande à zenith", "Ask zenith")}</span>
          <div className="relative">
            <textarea
              ref={area}
              rows={1}
              value={text}
              disabled={busy}
              onChange={(e) => {
                setText(e.target.value);
                setError(null);
              }}
              onKeyDown={(e) => {
                if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
                  e.preventDefault();
                  void launch();
                }
              }}
              className="block min-h-6 w-full resize-none bg-transparent text-[15px] leading-6 text-ink outline-none disabled:opacity-60"
            />
            {!text && (
              <span key={placeholder} className="pointer-events-none absolute inset-x-0 top-0 truncate text-[15px] leading-6 text-ink-3">
                {placeholder}
              </span>
            )}
          </div>
        </label>

        <div className="flex items-center gap-1.5 px-2.5 pb-2.5 pt-1.5">
          <div ref={menuRef} className="relative">
            <button
              type="button"
              disabled={!!fixedTarget}
              onClick={() => setMenu((m) => !m)}
              title={tr("Où l'agent travaille", "Where the agent works")}
              aria-haspopup="listbox"
              aria-expanded={menu}
              className="inline-flex h-7 items-center gap-1.5 rounded-md px-2 text-xs text-ink-2 transition-colors hover:bg-hover hover:text-ink disabled:cursor-default disabled:hover:bg-transparent"
            >
              <span className="size-2 shrink-0 rounded-full" style={{ background: target.color }} />
              <span className="max-w-[12rem] truncate whitespace-nowrap">{target.name}</span>
              {!fixedTarget && auto && <span className="hidden whitespace-nowrap text-ink-3 sm:inline">· auto</span>}
              {!fixedTarget && <ChevronDown className="size-3 text-ink-3" />}
            </button>
            {menu && (
              <ul role="listbox" className="absolute left-0 top-full z-50 mt-1 w-60 overflow-hidden rounded-lg border border-line bg-popover p-1 shadow-lg">
                <li>
                  <MenuRow
                    active={auto}
                    onClick={() => {
                      setChosen(null);
                      setMenu(false);
                    }}
                  >
                    <Wand2 className="size-3.5 text-ink-3" />
                    <span className="flex-1">{tr("Automatique", "Automatic")}</span>
                    <span className="text-2xs text-ink-3">{tr("selon ta phrase", "from your words")}</span>
                  </MenuRow>
                </li>
                <li className="-mx-1 my-1 h-px bg-line" />
                {targets.map((t) => (
                  <li key={t.id}>
                    <MenuRow
                      active={!auto && chosen === t.id}
                      onClick={() => {
                        setChosen(t.id);
                        setMenu(false);
                        area.current?.focus();
                      }}
                    >
                      <span className="grid size-3.5 place-items-center">
                        <span className="size-2 rounded-full" style={{ background: t.color }} />
                      </span>
                      <span className="flex-1 truncate">{t.name}</span>
                      {t.id === LIFE && <span className="text-2xs text-ink-3">{tr("tout le reste", "everything else")}</span>}
                    </MenuRow>
                  </li>
                ))}
              </ul>
            )}
          </div>

          <div className="inline-flex h-7 items-center rounded-md bg-muted p-0.5" role="radiogroup" aria-label={tr("Agent", "Agent")}>
            {PROVIDERS.map((p) => (
              <button
                key={p.id}
                type="button"
                role="radio"
                aria-checked={provider === p.id}
                onClick={() => setProvider(p.id)}
                title={p.id === "claude" ? "Claude Code" : "Codex"}
                className={cn(
                  "inline-flex h-6 items-center gap-1.5 rounded-[5px] px-2 text-xs transition-colors",
                  provider === p.id ? "bg-surface text-ink shadow-xs dark:bg-selected" : "text-ink-3 hover:text-ink-2",
                )}
              >
                <ProviderIcon id={p.id} size={12} />
                {p.name}
              </button>
            ))}
          </div>

          <span className="ml-auto hidden items-center gap-1 text-2xs text-ink-3 sm:inline-flex">
            {busy ? (
              tr("zenith s'y met…", "zenith is on it…")
            ) : (
              <>
                <CornerDownLeft className="size-3" /> {tr("pour lancer", "to start")}
              </>
            )}
          </span>
          <button
            type="submit"
            disabled={!text.trim() || busy}
            aria-label={tr("Lancer", "Start")}
            className="ml-1 grid size-8 shrink-0 place-items-center rounded-full bg-primary text-primary-foreground transition-colors hover:bg-primary/90 disabled:bg-muted disabled:text-ink-3 max-sm:ml-auto"
          >
            {busy ? <LoaderCircle className="size-4 animate-spin" /> : <ArrowUp className="size-4" />}
          </button>
        </div>
      </form>

      {error && <p className={cn("mt-2 text-xs text-bad", bare ? "px-4" : "px-1")}>{error}</p>}

      {suggestions.length > 0 && (
        <div className={cn("flex flex-wrap gap-1.5", bare ? "border-t border-line px-3 py-2.5" : "mt-2.5")}>
          {suggestions.map((s) => (
            <button
              key={s}
              type="button"
              disabled={busy}
              onClick={() => {
                setText(s);
                area.current?.focus();
              }}
              className="h-7 rounded-md border border-line bg-surface px-2.5 text-xs text-ink-2 transition-colors hover:bg-hover hover:text-ink disabled:opacity-60"
            >
              {s}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}

function MenuRow({ active, onClick, children }: { active: boolean; onClick: () => void; children: React.ReactNode }) {
  return (
    <button
      type="button"
      role="option"
      aria-selected={active}
      onClick={onClick}
      className={cn("flex h-8 w-full items-center gap-2 rounded-md px-2 text-left text-[13px] transition-colors", active ? "bg-selected text-ink" : "text-ink-2 hover:bg-hover hover:text-ink")}
    >
      {children}
      {active && <Check className="size-3.5 text-ink-3" />}
    </button>
  );
}
