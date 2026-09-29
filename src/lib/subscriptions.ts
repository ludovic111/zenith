import "server-only";
import { config, type SubscriptionConfig } from "./config";
import { tr } from "./i18n";

/**
 * Subscriptions and recurring fees, listed in zenith.config.json (`subscriptions`).
 * Railway's bill is replaced live by its API when RAILWAY_TOKEN is set.
 */

export type Category = SubscriptionConfig["category"];

export type Subscription = SubscriptionConfig;

export const CATEGORIES = (): { id: Category; label: string }[] => [
  { id: "ia", label: tr("IA", "AI") },
  { id: "infra", label: tr("Infra & dev", "Infra & dev") },
  { id: "domains", label: tr("Domaines & e-mail", "Domains & email") },
  { id: "tools", label: tr("Outils", "Tools") },
  { id: "music", label: tr("Musique & création", "Music & creative") },
  { id: "personal", label: tr("Perso", "Personal") },
];

/** Cost per month, in the original currency. One-off usage and unknown prices count as zero. */
export function monthly(s: Pick<Subscription, "amount" | "period">) {
  if (s.amount == null) return 0;
  if (s.period === "year") return s.amount / 12;
  if (s.period === "week") return (s.amount * 52) / 12;
  if (s.period === "usage") return 0;
  return s.amount;
}

export const SUBSCRIPTIONS: Subscription[] = config().subscriptions;

/** What needs action: failing payments, most urgent first. */
export const urgent = () =>
  SUBSCRIPTIONS.filter((s) => s.status === "failing").sort((a, b) => (a.next_renewal ?? "9999").localeCompare(b.next_renewal ?? "9999"));
