import { ArrowUpRight } from "lucide-react";
import type { Project } from "@/lib/projects";
import { tr } from "@/lib/i18n";
import { projectVersion } from "@/lib/sources/git";
import { uptime } from "@/lib/sources/uptime";
import { LiveDot, Status } from "@/components/z/status";
import { Meteors } from "@/components/ui/meteors";

/** A project page's hero: status, version, name, tagline, links. `actions` go before the links. */
export async function ProjectHeader({ project: p, actions, children }: { project: Project; actions?: React.ReactNode; children?: React.ReactNode }) {
  const [version, probes] = await Promise.all([projectVersion(p.id), uptime()]);
  const mine = probes.filter((u) => u.project === p.id);
  const allUp = mine.length > 0 && mine.every((u) => u.up);
  const anyDown = mine.some((u) => u.up === false);
  return (
    <header className="relative mb-8 overflow-hidden rounded-[2rem] border border-line px-6 py-8 sm:px-10 sm:py-10">
      <div aria-hidden className="absolute inset-0" style={{ background: `radial-gradient(120% 140% at 0% 0%, ${p.glow}33 0%, transparent 55%), radial-gradient(80% 120% at 100% 100%, ${p.color}26 0%, transparent 60%)` }} />
      <div aria-hidden className="absolute inset-0 overflow-hidden opacity-60">
        <Meteors number={10} />
      </div>
      <div className="relative flex flex-col gap-6 lg:flex-row lg:items-end lg:justify-between">
        <div>
          <div className="mb-3 flex flex-wrap items-center gap-3 text-sm">
            {mine.length > 0 && (
              <span className="inline-flex items-center gap-2 rounded-full border border-white/10 bg-black/30 px-3 py-1">
                <LiveDot health={anyDown ? "down" : allUp ? "up" : "unknown"} />
                <Status
                  health={anyDown ? "down" : allUp ? "up" : "unknown"}
                  label={anyDown ? tr("Incident", "Incident") : allUp ? tr("Tout répond", "All systems up") : tr("Mesure en cours", "Measuring…")}
                />
              </span>
            )}
            {version && <span className="rounded-full border border-white/10 bg-black/30 px-3 py-1 font-mono text-xs text-ink-2">v{version}</span>}
          </div>
          <h1 className="font-display text-5xl font-black tracking-tight sm:text-7xl" style={{ textShadow: `0 0 60px ${p.glow}66` }}>
            <span className="mr-3 align-middle text-4xl sm:text-5xl">{p.emoji}</span>
            {p.name}
          </h1>
          {p.tagline && <p className="mt-2 font-serif text-xl italic text-ink-2 sm:text-2xl">{p.tagline}</p>}
        </div>
        {(actions || p.links.length > 0) && (
          <div className="flex flex-wrap gap-2">
            {actions}
            {p.links.map((l) => (
              <a
                key={l.url}
                href={l.url}
                target="_blank"
                rel="noopener noreferrer"
                className="group inline-flex items-center gap-1.5 rounded-full border border-white/10 bg-black/30 px-3.5 py-1.5 text-sm text-ink-2 transition hover:border-white/25 hover:text-ink"
              >
                {l.label}
                <ArrowUpRight className="size-3.5 transition group-hover:-translate-y-0.5 group-hover:translate-x-0.5" style={{ color: p.glow }} />
              </a>
            ))}
          </div>
        )}
      </div>
      {children && <div className="relative mt-8">{children}</div>}
    </header>
  );
}
