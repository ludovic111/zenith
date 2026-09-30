import { Bot, Contact, Home, Radar, Sun, Wallet, type LucideIcon } from "lucide-react";
import { tr } from "@/lib/i18n";

/** zenith's own pages, in sidebar order. Shared by the sidebar, the title bar and ⌘K. */
export type NavPage = { href: string; name: string; icon: LucideIcon; keywords: string };

export const PAGES = (): NavPage[] => [
  { href: "/", name: tr("Accueil", "Home"), icon: Home, keywords: tr("vue d'ensemble maintenant", "overview now") },
  { href: "/vie", name: tr("Ma vie", "My life"), icon: Sun, keywords: tr("agenda météo mails colis dépenses rythme", "calendar weather mail parcels spending rhythm") },
  { href: "/veille", name: tr("Veille", "Radar"), icon: Radar, keywords: tr("mentions GitHub notifications actualité marchés Mac", "mentions GitHub notifications news markets Mac") },
  { href: "/agents", name: tr("Agents", "Agents"), icon: Bot, keywords: "Claude Code Codex sessions routines" },
  { href: "/abonnements", name: tr("Abonnements", "Subscriptions"), icon: Wallet, keywords: tr("frais factures limites IA argent", "fees bills AI limits money") },
  { href: "/annuaire", name: tr("Annuaire", "Directory"), icon: Contact, keywords: tr("réseaux e-mails domaines comptes", "socials emails domains accounts") },
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
