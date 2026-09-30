"use client";

import { usePathname } from "next/navigation";
import { Menu, PanelLeft } from "lucide-react";
import { tr } from "@/lib/i18n";
import { PAGES, SETTINGS, isSettingsPath } from "@/lib/nav";
import { toggleSidebar, useSidebarHidden } from "./sidebar-state";

/**
 * The top edge of every page, as in zenith code: where you are, and the sidebar button
 * when the sidebar is away. In zenith.app it is the window's title bar (drag it, double-click
 * it); the traffic lights sit on its left when the sidebar is hidden.
 */
export function TitleBar({ titles }: { titles: Record<string, string[]> }) {
  const path = usePathname();
  const hidden = useSidebarHidden();
  const crumbs = trail(path, titles);
  return (
    <div
      data-drag
      className="sticky top-0 z-20 flex h-[var(--titlebar-h)] shrink-0 items-center gap-2 border-b border-line bg-background/85 px-4 backdrop-blur-md select-none lg:px-5 [html[data-sidebar=hidden]_&]:mac:pl-[84px] [html[data-sidebar=hidden][data-fullscreen]_&]:mac:pl-4"
    >
      <button type="button" onClick={toggleSidebar} aria-label={tr("Menu", "Menu")} className="-ml-1 grid size-7 place-items-center rounded-md text-ink-3 transition hover:bg-hover hover:text-ink lg:hidden">
        <Menu className="size-4" />
      </button>
      {hidden && (
        <button type="button" onClick={toggleSidebar} title={tr("Afficher la barre latérale (⌘B)", "Show sidebar (⌘B)")} className="-ml-1 hidden size-7 place-items-center rounded-md text-ink-3 transition hover:bg-hover hover:text-ink lg:grid">
          <PanelLeft className="size-4" />
        </button>
      )}
      <nav aria-label={tr("Fil d'Ariane", "Breadcrumb")} className="flex min-w-0 items-center gap-1.5 text-[13px]">
        {crumbs.map((c, i) => (
          <span key={i} className="flex min-w-0 items-center gap-1.5">
            {i > 0 && <span className="text-ink-3">/</span>}
            <span className={i === crumbs.length - 1 ? "truncate font-medium text-ink" : "truncate text-ink-3"}>{c}</span>
          </span>
        ))}
      </nav>
    </div>
  );
}

/** Section, then page: "Projects / My app", "Settings / Data sources". */
function trail(path: string, titles: Record<string, string[]>): string[] {
  if (isSettingsPath(path)) {
    for (const g of SETTINGS()) {
      const s = g.items.find((x) => x.href === path);
      if (s) return [tr("Réglages", "Settings"), ...(g.label === "Code" ? ["Code"] : []), s.name];
    }
    return [tr("Réglages", "Settings")];
  }
  const page = PAGES().find((p) => p.href === path);
  if (page) return [page.name];
  if (titles[path]) return titles[path];
  return ["zenith"];
}
