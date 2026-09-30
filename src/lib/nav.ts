import { BarChart3, Code2, Contact, Home, LayoutGrid, MessagesSquare, Radar, Sun, Wallet, type LucideIcon } from "lucide-react";
import { tr } from "@/lib/i18n";

/**
 * zenith has three spaces, one switcher at the top of the sidebar:
 * - Aperçu (home): your day, your projects, your money — what zenith sees.
 * - Équipe (team): your agents — talk to them, see what they do.
 * - Code: zenith code — coding threads, pull requests, sessions.
 */
export type Space = "home" | "team" | "code";
export type SpaceInfo = { id: Space; name: string; icon: LucideIcon; href: string; kbd: string };

export const SPACES = (): SpaceInfo[] => [
  { id: "home", name: tr("Aperçu", "Overview"), icon: LayoutGrid, href: "/", kbd: "⌘1" },
  { id: "team", name: tr("Équipe", "Team"), icon: MessagesSquare, href: "/equipe", kbd: "⌘2" },
  { id: "code", name: "Code", icon: Code2, href: "/code", kbd: "⌘3" },
];

/** The space a path belongs to. A thread in zenith code is the team's when it runs in an agent's folder. */
export function spaceOf(path: string, agentThread = false): Space {
  if (path === "/equipe" || path.startsWith("/equipe/")) return "team";
  if (path === "/code" || path.startsWith("/code/")) return agentThread ? "team" : "code";
  if (path === "/agents" || path.startsWith("/agents/")) return "code";
  return "home";
}

/** zenith's own pages, by space. Shared by the sidebar, the title bar and ⌘K. */
export type NavPage = { href: string; name: string; icon: LucideIcon; keywords: string; space: Space; /** In the sidebar, when the name would repeat the space's. */ short?: string };

export const PAGES = (): NavPage[] => [
  { href: "/", name: tr("Accueil", "Home"), icon: Home, keywords: tr("vue d'ensemble maintenant", "overview now"), space: "home" },
  { href: "/vie", name: tr("Ma vie", "My life"), icon: Sun, keywords: tr("agenda météo mails colis dépenses rythme", "calendar weather mail parcels spending rhythm"), space: "home" },
  { href: "/veille", name: tr("Veille", "Radar"), icon: Radar, keywords: tr("mentions GitHub notifications actualité marchés Mac", "mentions GitHub notifications news markets Mac"), space: "home" },
  { href: "/abonnements", name: tr("Argent", "Money"), icon: Wallet, keywords: tr("abonnements frais factures limites IA", "subscriptions fees bills AI limits"), space: "home" },
  { href: "/annuaire", name: tr("Annuaire", "Directory"), icon: Contact, keywords: tr("réseaux e-mails domaines comptes", "socials emails domains accounts"), space: "home" },
  { href: "/equipe", name: tr("Équipe", "Team"), short: tr("Toute l'équipe", "Whole team"), icon: MessagesSquare, keywords: tr("agents conversations routines skills bots demander", "agents conversations routines skills bots ask"), space: "team" },
  { href: "/agents", name: tr("Sessions", "Sessions"), icon: BarChart3, keywords: "Claude Code Codex sessions usage limites coût", space: "code" },
];

/** One settings window for zenith and zenith code: `/reglages/*` is zenith's, `/code/settings/*` the app's. */
export type SettingsSection = { href: string; name: string };

export const SETTINGS = (): { label: string; items: SettingsSection[] }[] => [
  {
    label: "zenith",
    items: [
      { href: "/reglages", name: tr("Général", "General") },
      { href: "/reglages/sources", name: tr("Sources de données", "Data sources") },
    ],
  },
  {
    label: "Code",
    items: [
      { href: "/code/settings/general", name: tr("Général", "General") },
      { href: "/code/settings/providers", name: tr("Fournisseurs", "Providers") },
      { href: "/code/settings/projects", name: tr("Projets", "Projects") },
      { href: "/code/settings/source-control", name: tr("Contrôle de source", "Source control") },
      { href: "/code/settings/integrations", name: tr("Intégrations", "Integrations") },
      { href: "/code/settings/connections", name: tr("Connexions", "Connections") },
      { href: "/code/settings/keybindings", name: tr("Raccourcis", "Keybindings") },
      { href: "/code/settings/storage", name: tr("Stockage", "Storage") },
      { href: "/code/settings/archived", name: tr("Archivés", "Archived") },
    ],
  },
];

export const isSettingsPath = (path: string) => path === "/reglages" || path.startsWith("/reglages/") || path === "/code/settings" || path.startsWith("/code/settings/");
