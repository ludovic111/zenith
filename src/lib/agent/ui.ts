import "server-only";
import { config } from "../config";
import { tr } from "../i18n";
import { targets } from "./ask";
import { agentHome } from "./workspace";
import { agentAvatar, agentName, bots } from "./team";
import type { NowItem } from "./now";
import type { AgentTarget, Provider } from "./target";
import type { Avatar } from "./avatar";

/** What the ask bar needs from the server: destinations, provider, and words to start from. */

export type AgentUi = {
  enabled: boolean;
  targets: AgentTarget[];
  provider: Provider;
  home: string;
  /** The main agent's name and face. */
  name: string;
  avatar: Avatar;
  bots: { id: string; name: string; title?: string; home: string; color: string; avatar: Avatar }[];
};

export function agentUi(): AgentUi {
  const c = config();
  return {
    enabled: c.agent.enabled && c.code.enabled,
    targets: targets(),
    provider: c.agent.provider,
    home: agentHome(),
    name: agentName(),
    avatar: agentAvatar(),
    bots: bots().map(({ id, name, title, home, color, avatar }) => ({ id, name, title, home, color, avatar })),
  };
}

/** Placeholders drawn from what is waiting for you, then things anyone might ask. */
export function examplesFrom(items: NowItem[], names: Record<string, string>): string[] {
  const out: string[] = [];
  for (const i of items.slice(0, 4)) {
    const project = i.project ? names[i.project] ?? i.project : "";
    if (i.kind === "sale") out.push(tr(`Réponds aux acheteurs · ${i.title.split("·").pop()?.trim()}`, `Answer the buyers · ${i.title.split("·").pop()?.trim()}`));
    else if (i.kind === "reply") out.push(tr(`Prépare une réponse à ${i.title.replace(/^Répondre à /, "")}`, `Draft a reply to ${i.title.replace(/^Reply to /, "")}`));
    else if (i.kind === "ci") out.push(tr(`Répare la CI de ${project}`, `Fix ${project}'s CI`));
    else if (i.kind === "birthday") out.push(tr(`Trouve une idée de cadeau · ${i.title}`, `Find a gift idea · ${i.title}`));
    else if (i.kind === "payment") out.push(tr(`Comment régler ${i.title.split("·").slice(1).join("·").trim()} ?`, `How do I fix ${i.title.split("·").slice(1).join("·").trim()}?`));
    else if (i.kind === "down") out.push(tr(`Pourquoi ${project} ne répond plus ?`, `Why is ${project} down?`));
  }
  const first = config().projects[0]?.name;
  out.push(
    tr("Qu'est-ce qui m'attend cette semaine ?", "What's ahead of me this week?"),
    ...(first ? [tr(`Écris les notes de version de ${first}`, `Write ${first}'s release notes`)] : []),
    tr("Où est-ce que je dépense trop ?", "Where am I spending too much?"),
  );
  return out;
}

export const SUGGESTIONS = () => [
  tr("Prépare ma journée", "Plan my day"),
  tr("Où en sont mes projets ?", "How are my projects doing?"),
  tr("Trie ma boîte mail", "Triage my inbox"),
  tr("Qu'est-ce que je devrais faire maintenant ?", "What should I do next?"),
];
