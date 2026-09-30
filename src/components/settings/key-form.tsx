"use client";

import { useRouter } from "next/navigation";
import { useState } from "react";
import { Check, KeyRound, LoaderCircle } from "lucide-react";
import { tr } from "@/lib/i18n";

/**
 * Paste a key: it only goes to the local server, which writes it to .env.local.
 * `collapsed` shows a quiet "Replace the key" link first (for a source already connected).
 */
export function KeyForm({ name, placeholder, secret = true, collapsed = false }: { name: string; placeholder?: string; secret?: boolean; collapsed?: boolean }) {
  const router = useRouter();
  const [open, setOpen] = useState(!collapsed);
  const [value, setValue] = useState("");
  const [state, setState] = useState<"idle" | "saving" | "ok" | "error">("idle");
  const [error, setError] = useState<string | null>(null);

  const save = async (e: React.FormEvent) => {
    e.preventDefault();
    setState("saving");
    const res = await fetch("/api/settings", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ name, value }) });
    const j = await res.json().catch(() => ({}));
    if (!res.ok) {
      setError(j.error ?? tr("Échec", "Failed"));
      setState("error");
      return;
    }
    setValue("");
    setState("ok");
    router.refresh();
  };

  if (!open)
    return (
      <button type="button" onClick={() => setOpen(true)} className="inline-flex items-center gap-1.5 pt-0.5 text-xs text-ink-3 transition-colors hover:text-ink">
        <KeyRound className="size-3" />
        {tr("Remplacer la clé", "Replace the key")}
      </button>
    );

  return (
    <form onSubmit={save} className="flex max-w-lg flex-wrap items-center gap-2 pt-1.5">
      <label className="flex h-8 min-w-0 flex-1 items-center gap-2 rounded-md border border-line bg-surface px-2.5 transition-colors focus-within:border-primary">
        <KeyRound className="size-3.5 shrink-0 text-ink-3" />
        <input
          type={secret ? "password" : "text"}
          autoComplete="off"
          spellCheck={false}
          aria-label={name}
          autoFocus={collapsed}
          value={value}
          onChange={(e) => {
            setValue(e.target.value);
            setState("idle");
          }}
          placeholder={placeholder ?? tr(`Coller ${name}`, `Paste ${name}`)}
          className="w-full min-w-0 bg-transparent font-mono text-xs text-ink outline-none placeholder:text-ink-3"
        />
      </label>
      <button
        type="submit"
        disabled={!value.trim() || state === "saving"}
        className="inline-flex h-8 items-center gap-1.5 rounded-md bg-primary px-3 text-xs font-medium text-primary-foreground transition hover:brightness-110 disabled:opacity-40"
      >
        {state === "saving" ? <LoaderCircle className="size-3.5 animate-spin" /> : state === "ok" ? <Check className="size-3.5" /> : null}
        {state === "ok" ? tr("Branchée", "Connected") : tr("Brancher", "Connect")}
      </button>
      {state === "error" && <span className="w-full text-xs text-bad">{error}</span>}
    </form>
  );
}
