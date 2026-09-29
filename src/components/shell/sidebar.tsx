"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import { motion } from "motion/react";
import { Bot, Command, Contact, LayoutGrid, Radar, Settings2, SquareTerminal, Sun, Wallet } from "lucide-react";
import { cn } from "@/lib/utils";
import { tr } from "@/lib/i18n";
import { ZenithMark } from "./logo";

type NavProject = { id: string; name: string; href: string; color: string; glow: string; emoji: string; tagline: string };

export function Sidebar({ projects, code = true }: { projects: NavProject[]; code?: boolean }) {
  const path = usePathname();
  const items = [
    { href: "/", name: tr("Vue d'ensemble", "Overview"), glow: "#FFD166", icon: <LayoutGrid className="size-4" /> },
    { href: "/vie", name: tr("Ma vie", "My life"), glow: "#FDBA74", icon: <Sun className="size-4" /> },
    ...projects.map((p) => ({
      href: p.href,
      name: p.name,
      glow: p.glow,
      icon: <span className="size-3 rounded-full" style={{ background: p.glow, boxShadow: `0 0 12px ${p.glow}` }} />,
    })),
    ...(code ? [{ href: "/code", name: "Code", glow: "#A5B4FC", icon: <SquareTerminal className="size-4" /> }] : []),
    { href: "/veille", name: tr("Veille", "Radar"), glow: "#7dd3fc", icon: <Radar className="size-4" /> },
    { href: "/agents", name: tr("Agents IA", "AI agents"), glow: "#D4724F", icon: <Bot className="size-4" /> },
    { href: "/annuaire", name: tr("Annuaire", "Directory"), glow: "#FFD166", icon: <Contact className="size-4" /> },
    { href: "/abonnements", name: tr("Abonnements", "Subscriptions"), glow: "#34d399", icon: <Wallet className="size-4" /> },
  ];

  return (
    <>
      <aside className="sticky top-0 hidden h-screen w-64 shrink-0 flex-col border-r border-line bg-black/20 px-4 py-6 backdrop-blur-xl lg:flex">
        <Link href="/" className="group mb-10 flex items-center gap-3 px-2">
          <ZenithMark size={38} />
          <div>
            <div className="font-display text-lg font-black tracking-[0.18em]">zenith</div>
            <div className="font-serif text-sm italic text-ink-3">{tr("tout ce qui brille au-dessus", "everything that shines above")}</div>
          </div>
        </Link>

        <nav className="flex flex-col gap-1">
          {items.map((it) => {
            const active = it.href === "/" ? path === "/" : path.startsWith(it.href);
            return (
              <Link
                key={it.href}
                href={it.href}
                className={cn(
                  "relative flex items-center gap-3 rounded-xl px-3 py-2.5 text-sm transition-colors",
                  active ? "text-ink" : "text-ink-2 hover:bg-white/[0.04] hover:text-ink",
                )}
              >
                {active && (
                  <motion.span
                    layoutId="nav-active"
                    className="absolute inset-0 rounded-xl border border-white/10"
                    style={{ background: `linear-gradient(90deg, ${it.glow}26, transparent 80%)` }}
                    transition={{ type: "spring", stiffness: 380, damping: 32 }}
                  />
                )}
                <span className="relative grid size-5 place-items-center">{it.icon}</span>
                <span className="relative">{it.name}</span>
              </Link>
            );
          })}
        </nav>

        <div className="mt-auto flex flex-col gap-1">
          <button
            onClick={() => window.dispatchEvent(new Event("zenith:command"))}
            className="flex items-center gap-3 rounded-xl px-3 py-2.5 text-sm text-ink-2 hover:bg-white/[0.04] hover:text-ink"
          >
            <Command className="size-4" /> {tr("Aller à…", "Go to…")}
            <kbd className="ml-auto rounded-md border border-line px-1.5 py-0.5 font-mono text-[10px] text-ink-3">⌘K</kbd>
          </button>
          <Link
            href="/reglages"
            className={cn(
              "flex items-center gap-3 rounded-xl px-3 py-2.5 text-sm hover:bg-white/[0.04]",
              path.startsWith("/reglages") ? "text-ink" : "text-ink-2",
            )}
          >
            <Settings2 className="size-4" /> {tr("Sources de données", "Data sources")}
          </Link>
        </div>
      </aside>

      {/* Mobile: bottom bar */}
      <nav className="fixed inset-x-3 bottom-3 z-40 flex items-center gap-1 overflow-x-auto rounded-2xl border border-line bg-[#0b0a14]/90 p-2 backdrop-blur-xl [scrollbar-width:none] lg:hidden">
        {items.map((it) => {
          const active = it.href === "/" ? path === "/" : path.startsWith(it.href);
          return (
            <Link key={it.href} href={it.href} aria-label={it.name} className={cn("grid size-10 shrink-0 place-items-center rounded-xl", active && "bg-white/10")}>
              {it.icon}
            </Link>
          );
        })}
      </nav>
    </>
  );
}
