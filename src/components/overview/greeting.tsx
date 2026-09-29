"use client";

import { useSyncExternalStore } from "react";
import { l10n, tr } from "@/lib/i18n";

const hourIn = (d: Date) => Number(new Intl.DateTimeFormat("en-US", { timeZone: l10n().timeZone, hour: "numeric", hourCycle: "h23" }).format(d));

function hello(h: number) {
  if (h < 5) return tr("Bonne nuit", "Good night");
  if (h < 12) return tr("Bonjour", "Good morning");
  if (h < 18) return tr("Bon après-midi", "Good afternoon");
  return tr("Bonsoir", "Good evening");
}

const subscribe = (cb: () => void) => {
  const id = setInterval(cb, 1000);
  return () => clearInterval(id);
};

/** Live clock and a greeting that follows the time of day, in your time zone. */
export function Greeting({ name, place }: { name: string; place?: string }) {
  const t = useSyncExternalStore(subscribe, () => Math.floor(Date.now() / 1000), () => null);
  const now = t == null ? null : new Date(t * 1000);
  const { locale, timeZone } = l10n();
  const word = now ? hello(hourIn(now)) : tr("Salut", "Hello");
  return (
    <>
      <div className="mb-4 flex items-center gap-3 font-mono text-xs uppercase tracking-[0.25em] text-ink-3" suppressHydrationWarning>
        {now ? (
          <>
            <span>{new Intl.DateTimeFormat(locale, { timeZone, weekday: "long", day: "numeric", month: "long" }).format(now)}</span>
            <span className="h-px w-6 bg-white/20" />
            <span className="tabular text-ink-2">{new Intl.DateTimeFormat(locale, { timeZone, hour: "2-digit", minute: "2-digit", second: "2-digit" }).format(now)}</span>
            {place && <span className="text-ink-3">{place}</span>}
          </>
        ) : (
          <span>&nbsp;</span>
        )}
      </div>
      <h1 className="font-display text-5xl font-black leading-[0.95] tracking-tight sm:text-7xl xl:text-8xl" suppressHydrationWarning>
        {name ? (
          <>
            {word},
            <br />
            <span className="text-gradient">{name}.</span>
          </>
        ) : (
          <span className="text-gradient">{word}.</span>
        )}
      </h1>
    </>
  );
}
