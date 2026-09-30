import "server-only";
import os from "node:os";
import path from "node:path";
import { config } from "../config";
import { tr } from "../i18n";
import { agentHome } from "./team";

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

/**
 * zenith improves itself: one small, safe improvement, found in what bothers you, built
 * in a separate worktree (never in the running install), checked, then proposed as a
 * pull request or, in "ship" mode, pushed to main for the updater to install.
 */
export const improvePrompt = () => {
  const mode = config().agent.improve;
  const home = agentHome();
  const wt = path.join(path.dirname(home), "worktrees");
  const log = path.join(os.homedir(), "Library", "Logs", "Zenith", "server.log");
  return tr(
    `Améliore zenith toi-même : une amélioration petite et sûre, aujourd'hui.

1. **Trouve ce qui gêne vraiment**, dans cet ordre : les idées notées par l'équipe dans ${path.join(home, "IMPROVE.md")} ; les erreurs des dernières 24 h dans ${log} et ${path.join(ROOT, ".data", "update.log")} ; ce que la personne a demandé ou corrigé plusieurs fois (\`zenith_recall\`, \`zenith_history\`). Pas d'idée solide ? Arrête-toi là et dis-le.
2. **Choisis une seule chose** : un bug, une friction, une lenteur, un manque que la personne a exprimé. Jamais une refonte, jamais supprimer une fonctionnalité.
3. **Travaille à part** : \`git -C ${ROOT} fetch origin\`, puis \`git -C ${ROOT} worktree add ${wt}/<sujet> -b zenith/auto-<sujet> origin/main\` et \`ln -s ${path.join(ROOT, "node_modules")} ${wt}/<sujet>/node_modules\`. Ne touche jamais au dossier ${ROOT} lui-même : c'est l'app qui tourne.
4. **Écris comme le dépôt** (lis AGENTS.md et le code voisin), en français et en anglais pour l'interface.
5. **Vérifie** dans le worktree : \`npx tsc --noEmit -p .\`, \`npx eslint src scripts\`, \`node --test scripts/mcp/*.test.mjs\`, et le contrôle de vie privée (\`ln -s ${path.join(ROOT, "perso")} perso && node scripts/privacy-check.mjs; rm perso\`). Tout doit passer.
6. **Livre** — mode « ${mode} » : ${
      mode === "ship"
        ? `si tout passe et que le diff fait moins de 300 lignes, pousse sur main (\`git push origin HEAD:main\`, avance rapide seulement) : la mise à jour automatique l'installera quand plus personne ne travaille. Sinon, ouvre une pull request.`
        : `pousse la branche et ouvre une pull request (\`gh pr create\`) qui dit pourquoi, pour que la personne décide.`
    }
7. **Jamais** : toucher perso/, zenith.config.json, .env.local ou .data/ ; affaiblir une règle de sécurité (demander avant d'envoyer, payer, supprimer ; le mode auto ; les protections des routes) ; mettre une donnée personnelle dans ce dépôt public.
8. Retire le worktree (\`git -C ${ROOT} worktree remove ${wt}/<sujet>\`), note dans ${path.join(home, "IMPROVE.md")} ce que tu as fait (date, lien) en retirant l'idée traitée, et réponds en 3 lignes.`,
    `Improve zenith yourself: one small, safe improvement, today.

1. **Find what really bothers the person**, in this order: the ideas the team noted in ${path.join(home, "IMPROVE.md")}; the last 24 h of errors in ${log} and ${path.join(ROOT, ".data", "update.log")}; what the person asked for or corrected more than once (\`zenith_recall\`, \`zenith_history\`). No solid idea? Stop there and say so.
2. **Pick one thing**: a bug, a friction, a slowness, something they said was missing. Never a rewrite, never removing a feature.
3. **Work apart**: \`git -C ${ROOT} fetch origin\`, then \`git -C ${ROOT} worktree add ${wt}/<topic> -b zenith/auto-<topic> origin/main\` and \`ln -s ${path.join(ROOT, "node_modules")} ${wt}/<topic>/node_modules\`. Never touch ${ROOT} itself: it is the running app.
4. **Write like the repository** (read AGENTS.md and the neighboring code), French and English for the interface.
5. **Check** in the worktree: \`npx tsc --noEmit -p .\`, \`npx eslint src scripts\`, \`node --test scripts/mcp/*.test.mjs\`, and the privacy check (\`ln -s ${path.join(ROOT, "perso")} perso && node scripts/privacy-check.mjs; rm perso\`). Everything must pass.
6. **Deliver** — "${mode}" mode: ${
      mode === "ship"
        ? `if everything passes and the diff is under 300 lines, push to main (\`git push origin HEAD:main\`, fast-forward only): the automatic update installs it once nobody is working. Otherwise, open a pull request.`
        : `push the branch and open a pull request (\`gh pr create\`) that says why, so the person decides.`
    }
7. **Never**: touch perso/, zenith.config.json, .env.local or .data/; weaken a safety rule (asking before sending, paying, deleting; auto mode; route protections); put personal data in this public repository.
8. Remove the worktree (\`git -C ${ROOT} worktree remove ${wt}/<topic>\`), write in ${path.join(home, "IMPROVE.md")} what you did (date, link) and drop the idea you handled, and answer in 3 lines.`,
  );
};

export const TASKS = {
  "refresh-life": { name: () => tr("Actualiser ma vie", "Refresh my life"), prompt: refreshLifePrompt },
  "improve-zenith": { name: () => tr("Améliorer zenith", "Improve zenith"), prompt: improvePrompt },
} as const;

export type TaskId = keyof typeof TASKS;
