"use client";

import { useRouter } from "next/navigation";
import { useState } from "react";
import { Check, KeyRound } from "lucide-react";
import { tr } from "@/lib/i18n";

/** Paste a key: it only goes to the local server, which writes it to .env.local. */
export function KeyForm({ name, placeholder, secret = true }: { name: string; placeholder?: string; secret?: boolean }) {
  const router = useRouter();
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

  return (
    <form onSubmit={save} className="mt-3 flex gap-2">
      <label className="flex flex-1 items-center gap-2 rounded-xl border border-line bg-black/30 px-3 focus-within:border-white/25">
        <KeyRound className="size-3.5 shrink-0 text-ink-3" />
        <input
          type={secret ? "password" : "text"}
          autoComplete="off"
          spellCheck={false}
          value={value}
          onChange={(e) => {
            setValue(e.target.value);
            setState("idle");
          }}
          placeholder={placeholder ?? tr(`Coller ${name}`, `Paste ${name}`)}
          className="w-full bg-transparent py-2 font-mono text-xs outline-none placeholder:text-ink-3"
        />
      </label>
      <button
        disabled={!value.trim() || state === "saving"}
        className="rounded-xl bg-sun px-3.5 text-xs font-semibold text-[#1a1204] transition hover:brightness-110 disabled:opacity-40"
      >
        {state === "saving" ? "…" : state === "ok" ? <Check className="size-4" /> : tr("Brancher", "Connect")}
      </button>
      {state === "error" && <span className="self-center text-xs text-bad">{error}</span>}
    </form>
  );
}
