"use client";

import { Command } from "cmdk";
import { useRouter } from "next/navigation";
import { useEffect, useState } from "react";
import { ArrowUpRight, Bot, Contact, LayoutGrid, Radar, RefreshCw, Settings2, SquareTerminal, Sun, Wallet } from "lucide-react";
import { tr } from "@/lib/i18n";

type NavProject = { id: string; name: string; href: string; glow: string; tagline: string };
type ExtLink = { label: string; url: string; project: string; color: string };

export function CommandMenu({ projects, links, code = true }: { projects: NavProject[]; links: ExtLink[]; code?: boolean }) {
  const [open, setOpen] = useState(false);
  const router = useRouter();

  useEffect(() => {
    const key = (e: KeyboardEvent) => {
      if (e.key === "k" && (e.metaKey || e.ctrlKey)) {
        e.preventDefault();
        setOpen((o) => !o);
      }
    };
    const toggle = () => setOpen(true);
    window.addEventListener("keydown", key);
    window.addEventListener("zenith:command", toggle);
    return () => {
      window.removeEventListener("keydown", key);
      window.removeEventListener("zenith:command", toggle);
    };
  }, []);

  const go = (fn: () => void) => {
    setOpen(false);
    fn();
  };

  const item =
    "flex cursor-pointer items-center gap-3 rounded-xl px-3 py-2.5 text-sm text-ink-2 data-[selected=true]:bg-white/[0.07] data-[selected=true]:text-ink";

  return (
    <Command.Dialog
      open={open}
      onOpenChange={setOpen}
      label={tr("Aller à", "Go to")}
      overlayClassName="fixed inset-0 z-50 bg-black/60 backdrop-blur-sm"
      contentClassName="fixed left-1/2 top-[18vh] z-50 w-[min(640px,calc(100vw-2rem))] -translate-x-1/2 overflow-hidden rounded-3xl border border-white/10 bg-[#100e1c]/95 shadow-2xl shadow-black/60 backdrop-blur-xl"
    >
      <Command.Input
        autoFocus
        placeholder={tr("Projet, lien, action…", "Project, link, action…")}
        className="w-full border-b border-line bg-transparent px-5 py-4 text-base outline-none placeholder:text-ink-3"
      />
      <Command.List className="max-h-[50vh] overflow-y-auto p-2">
        <Command.Empty className="px-4 py-6 text-center text-sm text-ink-3">{tr("Rien sous ce ciel.", "Nothing under this sky.")}</Command.Empty>
        <Command.Group heading={tr("Projets", "Projects")} className="[&_[cmdk-group-heading]]:px-3 [&_[cmdk-group-heading]]:py-2 [&_[cmdk-group-heading]]:text-[11px] [&_[cmdk-group-heading]]:uppercase [&_[cmdk-group-heading]]:tracking-widest [&_[cmdk-group-heading]]:text-ink-3">
          <Command.Item className={item} onSelect={() => go(() => router.push("/"))}>
            <LayoutGrid className="size-4" /> {tr("Vue d'ensemble", "Overview")}
          </Command.Item>
          <Command.Item value={tr("Ma vie agenda météo mails colis dépenses rythme", "My life calendar weather mail parcels spending rhythm")} className={item} onSelect={() => go(() => router.push("/vie"))}>
            <Sun className="size-4" /> {tr("Ma vie", "My life")}
            <span className="ml-auto text-xs text-ink-3">{tr("agenda · météo · à faire", "calendar · weather · to do")}</span>
          </Command.Item>
          {projects.map((p) => (
            <Command.Item key={p.id} value={`${p.name} ${p.tagline}`} className={item} onSelect={() => go(() => router.push(p.href))}>
              <span className="size-2.5 rounded-full" style={{ background: p.glow, boxShadow: `0 0 10px ${p.glow}` }} />
              {p.name}
              <span className="ml-auto text-xs text-ink-3">{p.tagline}</span>
            </Command.Item>
          ))}
          {code && (
            <Command.Item value={tr("zenith code coder agents terminal éditeur", "zenith code coding agents terminal editor")} className={item} onSelect={() => go(() => router.push("/code"))}>
              <SquareTerminal className="size-4" /> zenith code
              <span className="ml-auto text-xs text-ink-3">{tr("coder avec les agents", "code with agents")}</span>
            </Command.Item>
          )}
          <Command.Item value={tr("Veille mentions GitHub notifications actualité Hacker News marchés crypto Mac Homebrew", "Radar mentions GitHub notifications news Hacker News markets crypto Mac Homebrew")} className={item} onSelect={() => go(() => router.push("/veille"))}>
            <Radar className="size-4" /> {tr("Veille", "Radar")}
            <span className="ml-auto text-xs text-ink-3">{tr("mentions · actu · ce Mac", "mentions · news · this Mac")}</span>
          </Command.Item>
          <Command.Item value={tr("Agents IA Claude Codex sessions", "AI agents Claude Codex sessions")} className={item} onSelect={() => go(() => router.push("/agents"))}>
            <Bot className="size-4" /> {tr("Agents IA", "AI agents")}
            <span className="ml-auto text-xs text-ink-3">Claude Code · Codex</span>
          </Command.Item>
          <Command.Item value={tr("Annuaire réseaux sociaux e-mails domaines comptes", "Directory social networks emails domains accounts")} className={item} onSelect={() => go(() => router.push("/annuaire"))}>
            <Contact className="size-4" /> {tr("Annuaire", "Directory")}
            <span className="ml-auto text-xs text-ink-3">{tr("réseaux · e-mails · domaines", "socials · emails · domains")}</span>
          </Command.Item>
          <Command.Item value={tr("Abonnements frais dépenses factures ChatGPT Claude limites", "Subscriptions fees spending bills ChatGPT Claude limits")} className={item} onSelect={() => go(() => router.push("/abonnements"))}>
            <Wallet className="size-4" /> {tr("Abonnements", "Subscriptions")}
            <span className="ml-auto text-xs text-ink-3">{tr("frais · limites IA", "fees · AI limits")}</span>
          </Command.Item>
        </Command.Group>
        {links.length > 0 && (
            <Command.Group heading={tr("Ouvrir", "Open")} className="[&_[cmdk-group-heading]]:px-3 [&_[cmdk-group-heading]]:py-2 [&_[cmdk-group-heading]]:text-[11px] [&_[cmdk-group-heading]]:uppercase [&_[cmdk-group-heading]]:tracking-widest [&_[cmdk-group-heading]]:text-ink-3">
            {links.map((l) => (
              <Command.Item key={l.url + l.project} value={`${l.project} ${l.label}`} className={item} onSelect={() => go(() => window.open(l.url, "_blank", "noopener"))}>
                <ArrowUpRight className="size-4" style={{ color: l.color }} />
                {l.label}
                <span className="ml-auto text-xs text-ink-3">{l.project}</span>
              </Command.Item>
            ))}
          </Command.Group>
        )}
        <Command.Group heading={tr("Actions", "Actions")} className="[&_[cmdk-group-heading]]:px-3 [&_[cmdk-group-heading]]:py-2 [&_[cmdk-group-heading]]:text-[11px] [&_[cmdk-group-heading]]:uppercase [&_[cmdk-group-heading]]:tracking-widest [&_[cmdk-group-heading]]:text-ink-3">
          <Command.Item className={item} onSelect={() => go(() => router.refresh())}>
            <RefreshCw className="size-4" /> {tr("Rafraîchir les données", "Refresh data")}
          </Command.Item>
          <Command.Item className={item} onSelect={() => go(() => router.push("/reglages"))}>
            <Settings2 className="size-4" /> {tr("Sources de données", "Data sources")}
          </Command.Item>
        </Command.Group>
      </Command.List>
    </Command.Dialog>
  );
}
