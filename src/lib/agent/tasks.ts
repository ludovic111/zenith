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

/** First run: an agent writes zenith.config.json from your project folders. */
export const setupPrompt = (projectsRoot: string) =>
  tr(
    `Configure zenith pour moi. Lis docs/configuration.md et zenith.config.example.json, puis parcours mes dossiers de projets dans ${projectsRoot} (remote git, package.json, README, sites) et écris perso/zenith.config.json : langue et devise d'après ce Mac, ma ville, et chaque vrai projet avec id, nom, accroche, emoji, dossier, dépôt GitHub, site et sondes. Demande-moi seulement ce que tu ne peux pas deviner (ma ville, quels projets garder). Vérifie que le fichier respecte zenith.schema.json. Ne committe rien : ce fichier est privé. Termine en me disant de relancer zenith.`,
    `Set zenith up for me. Read docs/configuration.md and zenith.config.example.json, then go through my project folders in ${projectsRoot} (git remote, package.json, README, sites) and write perso/zenith.config.json: language and currency from this Mac, my city, and every real project with id, name, tagline, emoji, folder, GitHub repository, site and probes. Only ask me what you can't guess (my city, which projects to keep). Check the file against zenith.schema.json. Don't commit anything: this file is private. End by telling me to restart zenith.`,
  );

export const TASKS = {
  "refresh-life": { name: () => tr("Actualiser ma vie", "Refresh my life"), prompt: refreshLifePrompt },
} as const;

export type TaskId = keyof typeof TASKS;
