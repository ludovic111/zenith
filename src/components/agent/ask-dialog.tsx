"use client";

import { useEffect, useState } from "react";
import { AnimatePresence, motion } from "motion/react";
import { tr } from "@/lib/i18n";
import type { AgentTarget, Provider } from "@/lib/agent/target";
import { AskBar } from "./ask-bar";

/**
 * "Ask zenith" from anywhere: ⌘J, the sidebar, or ⌘K with a sentence typed in. The same
 * ask bar as the overview's, over the current page.
 */
export function AskDialog({ targets, provider, suggestions }: { targets: AgentTarget[]; provider: Provider; suggestions: string[] }) {
  const [open, setOpen] = useState(false);
  const [text, setText] = useState("");
  const [key, setKey] = useState(0);

  useEffect(() => {
    const onAsk = (e: Event) => {
      setText((e as CustomEvent<{ text?: string }>).detail?.text ?? "");
      setKey((k) => k + 1);
      setOpen(true);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
      if (e.key.toLowerCase() === "j" && (e.metaKey || e.ctrlKey) && !e.altKey && !e.shiftKey) {
        e.preventDefault();
        setText("");
        setKey((k) => k + 1);
        setOpen((o) => !o);
      }
    };
    window.addEventListener("zenith:ask", onAsk);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("zenith:ask", onAsk);
      window.removeEventListener("keydown", onKey);
    };
  }, []);

  return (
    <AnimatePresence>
      {open && (
        <motion.div
          className="fixed inset-0 z-50 flex items-start justify-center bg-black/60 px-4 pt-[16vh] backdrop-blur-sm"
          initial={{ opacity: 0 }}
          animate={{ opacity: 1 }}
          exit={{ opacity: 0 }}
          transition={{ duration: 0.15 }}
          onMouseDown={(e) => {
            if (e.target === e.currentTarget) setOpen(false);
          }}
          role="dialog"
          aria-modal="true"
          aria-label={tr("Demande à zenith", "Ask zenith")}
        >
          <motion.div
            className="w-[min(680px,100%)]"
            initial={{ opacity: 0, y: 12, scale: 0.98 }}
            animate={{ opacity: 1, y: 0, scale: 1 }}
            exit={{ opacity: 0, y: 8, scale: 0.98 }}
            transition={{ type: "spring", stiffness: 420, damping: 34 }}
          >
            <div className="mb-3 flex items-baseline justify-between px-2">
              <span className="font-display text-sm font-medium tracking-wide text-ink">{tr("Demande à zenith", "Ask zenith")}</span>
              <span className="text-[11px] text-ink-3">
                <kbd className="rounded border border-line px-1 font-mono">esc</kbd> {tr("pour fermer", "to close")}
              </span>
            </div>
            <AskBar key={key} targets={targets} provider={provider} suggestions={suggestions} initialText={text} autoFocus onLaunched={() => setOpen(false)} />
          </motion.div>
        </motion.div>
      )}
    </AnimatePresence>
  );
}
