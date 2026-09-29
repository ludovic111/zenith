# L'agent zenith

*[English](agent.md)*

zenith voit tout : tes projets, ta journée, ton argent, tes messages. L'agent, c'est ce qui lui permet d'**agir**. Tu dis ce que tu veux en une phrase ; un agent IA (Claude Code ou Codex) s'y met aussitôt, au bon endroit, avec tout ce que zenith sait, et tu le suis dans un thread que tu peux lire, auquel tu peux répondre et dont tu approuves les actions.

## Demande à zenith

Trois chemins, une seule boîte :

- **La vue d'ensemble** : la barre sous le bonjour.
- **Partout** : **⌘J**, ou le bouton *Demander à zenith* en haut de la barre latérale.
- **⌘K** : tape une phrase ; quand rien d'autre ne correspond, *Demander à zenith* est la réponse. Entrée.

La destination se décide pendant que tu tapes, et s'affiche sur la pastille sous la boîte :

- **Un projet** quand ta phrase en nomme exactement un (son id, son nom, son dossier, son dépôt ou un nom d'identité en un mot) : l'agent travaille dans son dossier. *« corrige le bug de connexion dans my-app »* → My App.
- **Ma vie** sinon : le dossier de l'agent (`~/.zenith/life`), d'où il voit toute ta vie et peut confier du travail aux agents des projets.
- `@id` au début force la destination : `@zenith rends la barre latérale plus étroite`. La pastille est aussi un menu.

L'autre pastille choisit **Claude** ou **Codex**. Le modèle : `agent.model` s'il est renseigné, sinon celui par défaut de zenith code, sinon celui de ton dernier thread. Les permissions sont celles de zenith code (son mode par défaut, ou celui du projet).

Chaque demande est un thread de zenith code : il s'écrit en direct, demande des approbations, montre diffs et terminaux comme les autres. Tes conversations de vie sont sous **zenith → Conversations** dans la barre latérale, les threads de projet sous leur projet.

## Maintenant

La liste **Maintenant** de la vue d'ensemble, c'est ce qui t'attend, le plus pressant d'abord, tiré de ce que zenith sait déjà :

| | D'où |
| --- | --- |
| Un site qui ne répond plus (deux mesures de suite) | tes `probes` |
| Un paiement en échec | `subscriptions` avec `"status": "failing"` |
| Un anniversaire aujourd'hui ou dans les deux jours | Contacts (zenith.app) |
| Des acheteurs qui attendent pour un objet en vente, regroupés par objet | Ma vie (`sales`, `inbox.needsReply`) |
| Un e-mail qui attend ta réponse | Ma vie (`inbox.needsReply`) |
| De l'administratif dû dans le mois ou reçu ces dix derniers jours | Ma vie (`civic`) |
| Un workflow en échec sur une branche principale | notifications GitHub |
| E-mails et agenda pas relevés depuis 20 h | Ma vie (`capturedAt`) |

Les agents qui attendent ta réponse ou ton approbation passent devant.

Chaque élément a un seul geste : **Confier**. zenith lance un agent avec une consigne précise écrite pour cet élément (*lis le fil, rédige une réponse dans mon ton en brouillon Gmail, ne l'envoie pas*), et tu arrives dans son thread. L'élément montre ensuite où en est l'agent. **✓** le classe, **🕑** le cache jusqu'à demain ; les deux s'annulent. Un élément qui change (un nouveau message, un nouvel échec) revient comme un nouveau.

## Ce que l'agent sait et peut faire

zenith écrit les consignes de l'agent dans son dossier : `AGENTS.md` (lu par Codex, importé par `CLAUDE.md` pour Claude Code). Elles lui disent qui tu es, où sont ton brief et tes documents, tes projets et leurs dossiers, et les règles :

1. **Faire, pas décrire**, puis résumer en une à trois lignes.
2. **Demander avant ce qui sort du Mac ou ne se défait pas** : envoyer un e-mail ou un message, publier, payer, acheter, répondre à une invitation, supprimer, pousser sur une branche principale, déployer en production. Il prépare (brouillon, branche, PR), montre le contenu exact, et demande.
3. Pas de numéro de carte, mot de passe, adresse ou téléphone dans un fichier ; tes données n'entrent jamais dans le dépôt (public) de zenith.

`MEMORY.md`, à côté, est la mémoire de l'agent : il y ajoute ce qui doit durer (préférences, personnes, décisions) ; tu peux la modifier aussi. Le dossier branche aussi le serveur MCP de zenith pour Claude Code (`.mcp.json`) et Codex (`.codex/config.toml`).

Ce qu'il atteint : le brief et les documents de zenith, tes notes Obsidian, le shell (`git`, `gh`…), le web, et **tes connecteurs Claude** (Gmail, Google Agenda, Drive… tout ce que tu as branché sur claude.ai). Quand il en manque un, il dit lequel.

## Des agents qui en appellent d'autres

Le [serveur MCP](../README.fr.md#pour-les-agents-ia) permet à n'importe quel agent d'agir par zenith, pas seulement de le lire :

| Outil | |
| --- | --- |
| `zenith_now` | Ce qui attend, avec les ids. |
| `zenith_delegate` | Lance un autre agent dans un projet (ou dans `life`, ou dans `zenith`) avec une consigne complète. Il travaille en parallèle et apparaît dans la barre latérale. |
| `zenith_agent` | L'état et les derniers messages d'un agent lancé ainsi. |
| `zenith_done` | Classe un élément de Maintenant, ou le reporte. |

L'agent de vie peut ainsi découper *« prépare my-app pour la review App Store »* en une tâche de code dans My App et un e-mail à Apple, et suivre les deux. Un plafond de 12 délégations par heure empêche une boucle de s'emballer.

## Routines

Des agents qui travaillent seuls, une fois par jour à heure fixe, listés dans **Agents IA → Routines** avec leur dernier passage et un bouton **Lancer**. Dans `zenith.config.json` :

```json
"agent": {
  "routines": [
    { "id": "matin", "at": "07:30", "task": "refresh-life" },
    { "id": "vendredi", "at": "18:00", "days": [5], "prompt": "Fais le bilan de ma semaine : ce qui est sorti, ce qui a glissé, quoi faire lundi." }
  ]
}
```

`refresh-life` est intégrée : elle relève Gmail et Google Agenda dans Ma vie, pour que la vue d'ensemble, Maintenant et le brief soient frais à ton réveil. Un Mac endormi à l'heure dite rattrape dans les trois heures ; chaque routine tourne au plus une fois par jour, même avec deux serveurs zenith. Tous les champs : [configuration](configuration.md#agent).

## Sécurité

- Seules les pages de zenith (même origine, JSON) et les programmes locaux qui lisent `.data/agent-token` (créé en mode 600) peuvent lancer un agent. Une page web ne le peut pas, même ouverte sur ce Mac.
- Les agents ont les permissions de zenith code. Pour plus de prudence, mets son mode par défaut sur *approbation requise* dans les réglages de zenith code : l'agent demande alors avant chaque commande et modification.
- Chaque demande, sa destination et son thread sont notés dans `.data/agent.json`.

## Sous le capot

zenith parle à l'API HTTP de zenith code avec une session qu'il émet lui-même (`auth session issue`, renouvelée avant expiration) : `thread.create`, puis `thread.turn.start`. Tout est dans `src/lib/agent/` : `ask.ts` (destination, modèle, lancement), `now.ts`, `routines.ts`, `workspace.ts` (le dossier de l'agent), `tasks.ts` (consignes intégrées), `target.ts` (règles de destination, partagées avec le navigateur).
