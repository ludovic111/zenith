import "server-only";
import path from "node:path";
import { tr } from "../i18n";

/** Built-in requests: the words zenith uses when it asks an agent on your behalf. */

const ROOT = process.cwd();

/** Captures Gmail and Google Calendar into .data/life.json, the "My life" snapshot. */
export const refreshLifePrompt = () => {
  const file = path.join(ROOT, ".data", "life.json");
  const doc = path.join(ROOT, "docs", tr("releves.fr.md", "releves.md"));
  const type = path.join(ROOT, "src", "lib", "sources", "life.ts");
  return tr(
    `Actualise « Ma vie » dans zenith. Avec les connecteurs Gmail et Google Agenda, relève mes 14 prochains jours d'agenda, mes e-mails qui attendent vraiment une réponse, mes colis, mes ventes, mes dépenses des 3 derniers mois et l'administratif, puis écris ${file} exactement au format du type Life de ${type} (règles dans ${doc}). Garde les champs d'avant quand un connecteur ne répond pas. Termine par 3 lignes : ce qui a changé, ce qui presse.`,
    `Refresh "My life" in zenith. With the Gmail and Google Calendar connectors, capture my next 14 days of calendar, emails that really need a reply, parcels, sales, the last 3 months of spending and paperwork, then write ${file} exactly in the shape of the Life type in ${type} (rules in ${doc}). Keep the previous fields when a connector doesn't answer. End with 3 lines: what changed, what is pressing.`,
  );
};

export const TASKS = {
  "refresh-life": { name: () => tr("Actualiser ma vie", "Refresh my life"), prompt: refreshLifePrompt },
} as const;

export type TaskId = keyof typeof TASKS;
