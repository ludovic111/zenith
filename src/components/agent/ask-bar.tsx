"use client";

import { useRouter } from "next/navigation";
import { useEffect, useMemo, useRef, useState } from "react";
import { AnimatePresence, motion } from "motion/react";
import { ArrowUp, Check, ChevronDown, CornerDownLeft, LoaderCircle, Sparkles } from "lucide-react";
import { cn } from "@/lib/utils";
import { tr } from "@/lib/i18n";
import { LIFE, guessTarget, type AgentTarget, type Provider } from "@/lib/agent/target";
import { AssistantIcon } from "@/components/assistants/assistant-icon";
import { BorderBeam } from "@/components/ui/border-beam";
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
export function AskBar({ targets, provider: initialProvider, examples = [], suggestions = [], fixedTarget, autoFocus, initialText = "", onLaunched, className }: AskBarProps) {
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
        className={cn(
          "group relative overflow-visible rounded-[26px] border bg-[#0d0b18]/80 backdrop-blur-xl transition duration-300",
          "shadow-[0_30px_80px_-40px_rgb(255_209_102/0.45),inset_0_1px_0_0_rgb(255_255_255/0.06)]",
          busy ? "border-sun/40" : "border-white/10 focus-within:border-sun/35 hover:border-white/20",
        )}
      >
        <div aria-hidden className="pointer-events-none absolute -inset-px -z-10 rounded-[26px] opacity-60 blur-2xl transition group-focus-within:opacity-100" style={{ background: "radial-gradient(60% 120% at 10% 0%, #FFD16633, transparent 60%), radial-gradient(50% 120% at 100% 100%, #B18CFF2e, transparent 60%)" }} />
        {busy && <BorderBeam size={120} duration={3} colorFrom="#FFD166" colorTo="#B18CFF" />}

        <label className="flex items-start gap-3 px-5 pb-1 pt-4">
          <Sparkles className={cn("mt-1 size-5 shrink-0 text-sun transition", busy && "animate-pulse")} />
          <span className="sr-only">{tr("Demande à zenith", "Ask zenith")}</span>
          <div className="relative min-w-0 flex-1">
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
              className="block w-full resize-none bg-transparent text-lg leading-7 text-ink outline-none [scrollbar-width:thin] disabled:opacity-60"
            />
            {!text && (
              <AnimatePresence mode="wait" initial={false}>
                <motion.span
                  key={placeholder}
                  initial={{ opacity: 0, y: 6 }}
                  animate={{ opacity: 1, y: 0 }}
                  exit={{ opacity: 0, y: -6 }}
                  transition={{ duration: 0.35 }}
                  className="pointer-events-none absolute inset-x-0 top-0 truncate text-lg leading-7 text-ink-3"
                >
                  {placeholder}
                </motion.span>
              </AnimatePresence>
            )}
          </div>
        </label>

        <div className="flex items-center gap-2 px-3 pb-3 pt-2">
          <div ref={menuRef} className="relative">
            <button
              type="button"
              disabled={!!fixedTarget}
              onClick={() => setMenu((m) => !m)}
              title={tr("Où l'agent travaille", "Where the agent works")}
              className="inline-flex items-center gap-2 rounded-full border border-white/10 bg-white/[0.04] py-1 pl-2.5 pr-2 text-xs text-ink-2 transition hover:border-white/20 hover:text-ink disabled:cursor-default disabled:hover:border-white/10"
            >
              <span className="size-2 rounded-full" style={{ background: target.glow, boxShadow: `0 0 8px ${target.glow}` }} />
              <span className="max-w-[12rem] truncate">{target.name}</span>
              {!fixedTarget && (auto ? <span className="text-ink-3">· auto</span> : null)}
              {!fixedTarget && <ChevronDown className="size-3 text-ink-3" />}
            </button>
            <AnimatePresence>
              {menu && (
                <motion.ul
                  initial={{ opacity: 0, y: 4, scale: 0.98 }}
                  animate={{ opacity: 1, y: 0, scale: 1 }}
                  exit={{ opacity: 0, y: 4, scale: 0.98 }}
                  transition={{ duration: 0.14 }}
                  className="absolute left-0 top-full z-50 mt-2 w-60 overflow-hidden rounded-2xl border border-white/10 bg-[#121020]/95 p-1.5 shadow-2xl shadow-black/60 backdrop-blur-xl"
                >
                  <li>
                    <MenuRow
                      active={auto}
                      onClick={() => {
                        setChosen(null);
                        setMenu(false);
                      }}
                    >
                      <Sparkles className="size-3.5 text-sun" />
                      <span className="flex-1">{tr("Automatique", "Automatic")}</span>
                      <span className="text-[11px] text-ink-3">{tr("selon ta phrase", "from your words")}</span>
                    </MenuRow>
                  </li>
                  <li className="my-1 h-px bg-white/[0.06]" />
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
                        <span className="size-2 rounded-full" style={{ background: t.glow, boxShadow: `0 0 8px ${t.glow}` }} />
                        <span className="flex-1 truncate">{t.name}</span>
                        {t.id === LIFE && <span className="text-[11px] text-ink-3">{tr("tout le reste", "everything else")}</span>}
                      </MenuRow>
                    </li>
                  ))}
                </motion.ul>
              )}
            </AnimatePresence>
          </div>

          <div className="inline-flex rounded-full border border-white/10 bg-white/[0.03] p-0.5" role="radiogroup" aria-label={tr("Agent", "Agent")}>
            {PROVIDERS.map((p) => (
              <button
                key={p.id}
                type="button"
                role="radio"
                aria-checked={provider === p.id}
                onClick={() => setProvider(p.id)}
                title={p.id === "claude" ? "Claude Code" : "Codex"}
                className={cn(
                  "inline-flex items-center gap-1.5 rounded-full px-2.5 py-0.5 text-xs transition",
                  provider === p.id ? "bg-white/[0.09] text-ink" : "text-ink-3 hover:text-ink-2",
                )}
              >
                <AssistantIcon id={p.id === "claude" ? "claude" : "chatgpt"} size={12} />
                {p.name}
              </button>
            ))}
          </div>

          <span className="ml-auto hidden items-center gap-1 text-[11px] text-ink-3 sm:inline-flex">
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
            className="grid size-9 shrink-0 place-items-center rounded-full bg-sun text-[#1a1204] shadow-[0_0_24px_-4px_#FFD166] transition hover:scale-105 disabled:scale-100 disabled:bg-white/10 disabled:text-ink-3 disabled:shadow-none max-sm:ml-auto"
          >
            {busy ? <LoaderCircle className="size-4 animate-spin" /> : <ArrowUp className="size-4" />}
          </button>
        </div>
      </form>

      {error && <p className="mt-2 px-2 text-sm text-bad">{error}</p>}

      {suggestions.length > 0 && (
        <div className="mt-3 flex flex-wrap gap-2 px-1">
          {suggestions.map((s) => (
            <button
              key={s}
              type="button"
              disabled={busy}
              onClick={() => {
                setText(s);
                area.current?.focus();
              }}
              className="rounded-full border border-white/[0.08] bg-white/[0.025] px-3 py-1 text-xs text-ink-2 transition hover:border-sun/30 hover:bg-sun/[0.06] hover:text-ink"
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
    <button type="button" onClick={onClick} className={cn("flex w-full items-center gap-2.5 rounded-xl px-2.5 py-2 text-left text-sm transition", active ? "bg-white/[0.07] text-ink" : "text-ink-2 hover:bg-white/[0.04] hover:text-ink")}>
      {children}
      {active && <Check className="size-3.5 text-ink-3" />}
    </button>
  );
}
