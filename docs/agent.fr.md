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

## Son équipe

Un agent principal (celui de « Ma vie ») et, si tu veux, des **bots** : des agents avec un prénom, un métier et une tête (une forme vive avec deux yeux et un accessoire, comme les Dots et les Grok Bots), chacun avec son dossier, sa personnalité et sa mémoire, qui tournent sur **ton abonnement Claude** (par Claude Code) ou **ton abonnement ChatGPT** (par Codex). C'est l'équipe des ChatGPT Dots et des Grok Bots, sur tes propres abonnements, sur ton Mac.

```json
"agent": {
  "name": "Céleste", "shape": "circle", "color": "#F5A524", "accessory": "star",
  "bots": [
    { "id": "courrier", "name": "Margot", "title": "Courrier", "shape": "pill", "color": "#EC4899", "accessory": "bow", "provider": "claude", "role": "Tient ma boîte mail et mon agenda : trie, prépare les réponses en brouillon, n'envoie jamais rien." },
    { "id": "atelier", "name": "Hugo", "title": "Atelier", "shape": "square", "color": "#F97316", "accessory": "antenna", "provider": "codex", "role": "Veille à la santé technique de mes projets : CI, dépendances, PR. Répare dans une branche, jamais sur main." }
  ]
}
```

- **Leur parler** : `@margot …` dans la barre, ⌘J ou ⌘K ; ou le menu de destination ; ou leur carte dans **Agents IA → Équipe**. La puce Claude/Codex suit l'abonnement du bot.
- **Ils se parlent** : `zenith_message` permet à un agent d'écrire à un coéquipier et de recevoir sa réponse (Iris sur Codex demande à Margot sur Claude ce qu'il y a dans ta boîte) ; chaque paire garde sa conversation. `zenith_delegate` confie une tâche longue, `zenith_team` dit qui fait quoi et qui est occupé. Une chaîne d'agents qui se relancent s'arrête à trois.
- Leurs conversations sont dans la barre latérale, sous **Conversations**.

## Ce qu'il sait, ce qu'il apprend

Chaque agent a un dossier (`~/.zenith/life` pour le principal, `~/.zenith/bots/<id>` pour les bots), à la manière de Hermes Agent :

| Fichier | |
| --- | --- |
| `SOUL.md` | Sa personnalité : ton, habitudes, ce qu'il fait toujours ou jamais. Écrit une fois (le rôle d'un bot), puis à toi. |
| `USER.md` | Ce que l'équipe sait de toi : préférences, façon d'écrire, personnes importantes. Un seul, partagé, dans le dossier principal. |
| `MEMORY.md` | Sa mémoire : décisions, leçons, où en sont les choses. |
| `skills/` | Les savoir-faire de l'équipe, un `SKILL.md` chacun (dossier principal, partagé). |
| `AGENTS.md` | Ses consignes, réécrites par zenith à chaque demande, avec tout ce qui précède recopié dedans : Codex le lit, Claude Code l'importe par `CLAUDE.md`. Ne le modifie pas. |

Les consignes lui disent qui tu es, où sont ton brief et tes documents, tes projets et leurs dossiers, son équipe, ses skills, et les règles :

1. **Faire, pas décrire**, puis résumer en une à trois lignes.
2. **Demander avant ce qui sort du Mac ou ne se défait pas** : envoyer un e-mail ou un message, publier, payer, acheter, répondre à une invitation, supprimer, pousser sur une branche principale, déployer en production. Il prépare (brouillon, branche, PR), montre le contenu exact, et demande.
3. Pas de numéro de carte, mot de passe, adresse ou téléphone dans un fichier ; tes données n'entrent jamais dans le dépôt (public) de zenith.

Et d'**apprendre sans qu'on le lui demande** : une correction ou une préférence va dans `USER.md`, une décision dans `MEMORY.md`, une démarche qu'il refera devient un skill. Ces fichiers restent courts (au-delà d'une limite, zenith tronque et lui demande de consolider).

Le MCP donne accès au brief, aux documents de zenith et aux notes Obsidian locales. Le shell (`git`, `gh`…), le web et les connecteurs de comptes dépendent des outils réellement exposés dans la session ; les connecteurs Claude ne sont pas automatiquement disponibles dans Codex. L'agent vérifie chaque accès par une lecture et signale ce qui manque.

Un MCP local configuré dans Codex ne prouve pas qu'une session Kira ou ChatGPT dans le cloud peut atteindre ce Mac. Vérifie que les outils `zenith_*` sont exposés dans cette session, puis appelle `zenith_brief`. Les lectures basculent sur `context/` après trois secondes si le serveur ne répond pas, avec une mention du repli et la date du document. Agir exige le serveur lancé et son autorisation locale existante. Toute création de token, tunnel ou accès persistant, ou modification des permissions, demande ton accord précis.

Le dossier branche aussi le serveur MCP de zenith pour Claude Code (`.mcp.json`) et Codex (`.codex/config.toml`).

## Agir sur tout

En plus des outils de zenith, chaque agent sait agir sur le Mac (`open`, AppleScript pour Mail, Calendrier, Notes, Rappels, Messages…, `shortcuts run` pour tes Raccourcis), sur le web (les outils de navigateur de sa session : navigateur et *computer use* de Codex, connecteurs de Claude et Claude in Chrome) et sur tes services (`gh`, `railway`…). Tout ce qui a un serveur MCP peut être donné à toute l'équipe d'un coup :

```json
"agent": {
  "mcp": {
    "linear": { "url": "https://mcp.linear.app/mcp" },
    "playwright": { "command": "npx", "args": ["@playwright/mcp@latest"] }
  }
}
```

zenith les écrit dans le dossier de chaque agent, pour Claude Code comme pour Codex.

## Skills

Des savoir-faire écrits, dans `~/.zenith/life/skills/<id>/SKILL.md` (le format agentskills.io que Claude Code et Codex lisent tous deux ; zenith les relie dans `.claude/skills` et `.agents/skills` de chaque dossier). zenith en pose cinq au départ — `plan-day`, `reply-email`, `weekly-review`, `watch`, `write-skill` — puis l'équipe en écrit d'autres au fil du travail. Tu peux les modifier ou les supprimer : zenith ne réécrit jamais un skill qu'il a déjà posé. Ils sont listés dans **Agents IA → Skills**, et une routine peut en suivre un (`"skill": "weekly-review"`).

## Des agents qui en appellent d'autres

Le [serveur MCP](../README.fr.md#pour-les-agents-ia) permet à n'importe quel agent d'agir par zenith, pas seulement de le lire :

| Outil | |
| --- | --- |
| `zenith_now` | Ce qui attend, avec les ids. |
| `zenith_delegate` | Lance un autre agent dans un projet, chez un bot de l'équipe (par son id), dans `life` ou dans `zenith`, avec une consigne complète. Il travaille en parallèle et apparaît dans la barre latérale. |
| `zenith_agent` | L'état et les derniers messages d'un agent lancé ainsi. |
| `zenith_done` | Classe un élément de Maintenant, ou le reporte. |

L'agent de vie peut ainsi découper *« prépare my-app pour la review App Store »* en une tâche de code dans My App et un e-mail à Apple, et suivre les deux. Un plafond de 12 délégations par heure empêche une boucle de s'emballer.

## Routines

Des agents qui travaillent seuls, listés dans **Agents IA → Routines** avec leur dernier passage et un bouton **Lancer**. Deux sortes :

- **À heure fixe** (`at`), une fois par jour, les jours choisis.
- **Sur un événement** (`on`) : chaque nouvel élément de Maintenant de ces sortes (une CI cassée, un e-mail qui attend une réponse, un paiement en échec…) est confié à l'agent dès qu'il apparaît, une seule fois, avec la consigne de l'élément. Ce qui attendait déjà quand tu ajoutes la routine reste à toi ; **Lancer** le lui confie quand même.

Chacune peut être faite par un bot (`bot`) et suivre un skill (`skill`) :

```json
"agent": {
  "routines": [
    { "id": "matin", "at": "07:30", "task": "refresh-life" },
    { "id": "journee", "at": "07:45", "skill": "plan-day" },
    { "id": "vendredi", "at": "18:00", "days": [5], "skill": "weekly-review" },
    { "id": "ci", "on": ["ci"], "bot": "atelier" },
    { "id": "reponses", "on": ["reply", "sale"], "bot": "courrier", "skill": "reply-email" }
  ]
}
```

`refresh-life` est intégrée : elle relève Gmail et Google Agenda dans Ma vie, pour que la vue d'ensemble, Maintenant et le brief soient frais à ton réveil. Un Mac endormi à l'heure dite rattrape dans les trois heures ; chaque routine tourne au plus une fois par jour, même avec deux serveurs zenith. Les événements sont regardés toutes les cinq minutes, trois éléments au plus par routine et douze par heure en tout. Tous les champs : [configuration](configuration.md#agent).

## Depuis ton téléphone (Telegram)

Parle à ton agent depuis Telegram, comme à Hermes Agent :

1. Crée un bot avec [@BotFather](https://t.me/BotFather) et colle son jeton dans **Réglages → Agent zenith → Telegram** (il va dans `.env.local` sous `TELEGRAM_BOT_TOKEN`).
2. Ajoute `"gateway": { "telegram": { "chats": [] } }` dans `agent`, envoie `/start` à ton bot : il te répond l'id de ton chat. Mets-le dans `chats` et relance zenith.

Un message lance une conversation avec ton agent (ou `target` : un bot, un projet ; `@id` au début marche aussi), ou poursuit celle des deux dernières heures ; `/new` repart de zéro. La réponse arrive quand l'agent a fini ; s'il attend ton accord, tu reçois le lien pour le lui donner dans zenith. Seuls les chats listés sont écoutés ; les autres apprennent leur id, rien de plus. zenith doit tourner sur ton Mac.

## Sécurité

- Seules les pages de zenith (même origine, JSON) et les programmes locaux qui lisent `.data/agent-token` (créé en mode 600) peuvent lancer un agent. Une page web ne le peut pas, même ouverte sur ce Mac.
- **Les mots venus d'ailleurs n'ont jamais tous les droits.** Tout ce qui porte un texte que zenith n'a pas écrit — éléments de Maintenant (e-mails, notes, CI), routines, demandes d'autres agents, messages Telegram, et toute demande à ton agent de vie ou à un bot — tourne au plus en mode **auto** de zenith code : l'agent travaille seul, mais les relecteurs de Claude et de Codex bloquent les actions risquées (faire sortir des données, commandes destructrices) qu'un e-mail piégé pourrait demander. Ce que tu tapes pour un projet garde ton mode habituel. Un mode par défaut plus strict (*approbation requise*, *modifications acceptées*) l'emporte toujours.
- Les consignes de l'agent lui disent que les e-mails, pages et messages sont des données, jamais des ordres, et les demandes que zenith écrit le rappellent.
- Chaque demande, sa destination, son origine et son thread sont notés dans `.data/agent.json`, et affichés dans **Agents IA → Activité**.

## Sous le capot

zenith parle à l'API HTTP de zenith code avec une session qu'il émet lui-même (`auth session issue`, renouvelée avant expiration) : `thread.create`, puis `thread.turn.start`. Tout est dans `src/lib/agent/` : `ask.ts` (destination, modèle, lancement), `team.ts` (l'équipe), `workspace.ts` (les dossiers des agents), `skills.ts`, `now.ts`, `routines.ts`, `gateway.ts` (Telegram), `tasks.ts` (consignes intégrées), `target.ts` (règles de destination, partagées avec le navigateur).
