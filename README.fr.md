# zenith

**Un ciel privé au-dessus de tes projets et de ta journée.** zenith est un tableau de bord local pour qui mène plusieurs projets à la fois : l'état de chaque projet, ses déploiements, son code et ses agents IA, ton agenda, tes mails, ton argent et l'actualité — et **zenith code**, un vrai espace de code pour Claude Code et Codex, intégré.

Et il agit. **Demande à zenith** n'importe quoi en une phrase (⌘J) : un agent IA s'y met aussitôt, dans le bon projet ou sur toute ta vie, avec tout ce que zenith sait. La liste **Maintenant** montre ce qui t'attend — un paiement en échec, des acheteurs à qui répondre, une CI cassée, un anniversaire — et confie chaque chose à un agent d'un geste. Il prépare, tu valides.

Il tourne sur ta machine, ne répond que sur `127.0.0.1` et n'envoie tes données nulle part. [English version](README.md).

## Ce qu'il montre

| Page | Contenu |
| --- | --- |
| **Demande à zenith** | Une seule boîte, partout (vue d'ensemble, pages projet, ⌘J, ⌘K) : dis ce que tu veux, un agent le fait et s'ouvre en thread. Va au projet que tu nommes, sinon à ton agent de vie |
| **Maintenant** | Ce qui t'attend, le plus pressant d'abord, avec pour chaque chose *Confier* à un agent, *fait* et *plus tard*. Et les routines : des agents qui travaillent seuls chaque matin |
| Vue d'ensemble | Système orbital des projets (état en direct), chiffres clés, cartes projet, six mois de commits, fil de tout ce qui se passe |
| Ma vie | Météo, qualité de l'air, UV et pollens, agenda (14 jours, fériés, anniversaires), ce qui t'attend, morceau en cours, temps d'écran, dépenses, rythme de travail |
| Projets | Une page par projet : disponibilité, latence, trafic et déploiements (Railway), CI, issues et PR (GitHub), releases et téléchargements, note et avis App Store, MRR RevenueCat, sessions d'agents, notes Obsidian, carte d'identité |
| **Code** | **zenith code** : parle à Claude Code, Codex et d'autres agents dans n'importe lequel de tes projets — diffs, terminaux, worktrees, validations. Ses threads vivent dans la barre latérale de zenith, sous leur projet |
| **Claude, ChatGPT** | Leurs apps de bureau arrimées dans la fenêtre de zenith (zenith.app), plugins et connecteurs compris |
| Veille | Qui parle de tes projets (Hacker News, GitHub), notifications, nouvelles étoiles, contributions, actualité, marchés, état du Mac |
| Agents IA | Toutes les sessions Claude Code et Codex : en cours, coût, lignes écrites, PR, commande pour reprendre |
| Abonnements | Tout ce que tu paies, total mensuel dans ta devise, prochains prélèvements, paiements en échec, jauges Claude / ChatGPT |
| Annuaire | Noms, identifiants, domaines (registraire, renouvellement, certificat, e-mail), stores et services de chaque projet |

**⌘J** pour demander à zenith, **⌘K** pour aller partout (ou tape une phrase pour demander).

## Démarrage

Il faut macOS ou Linux, Node.js 22.16+ (24+ conseillé) et git. Facultatif : la [CLI GitHub](https://cli.github.com) connectée, Claude Code et/ou Codex.

```bash
git clone https://github.com/ludovic111/zenith.git
cd zenith
npm install
cp zenith.config.example.json zenith.config.json   # puis édite-le : projets, ville, langue ("locale": "fr-FR")
npm run dev                                         # http://127.0.0.1:4748
```

Pour construire zenith code (l'espace de code), une fois :

```bash
npm run code:build
```

Avec zenith code construit et Claude Code ou Codex installé, tu peux sauter l'édition du fichier : la première page a un bouton **Laisser zenith se configurer**. Un agent lit tes dossiers de projets, écrit ta config, et ne te demande que ce qu'il ne peut pas deviner.

### App Mac

```bash
npm run mac:install
```

Construit tout, installe **zenith.app** (fenêtre native) et fait tourner le serveur en arrière-plan (LaunchAgent sur `127.0.0.1:4747`). À relancer après une mise à jour ; `npm run mac:uninstall` retire tout. L'app lit aussi Calendrier, Rappels, Contacts (anniversaires seulement), Mail, Musique/Spotify et le temps d'écran — en lecture seule, après la demande de macOS.

## Configuration

Tout ce qui te concerne vit dans **`zenith.config.json`** — à la racine ou dans `perso/`, tous deux ignorés par git —, validé au démarrage ; `zenith.schema.json` donne l'autocomplétion dans ton éditeur. Chaque champ est décrit dans [docs/configuration.md](docs/configuration.md).

Les clés d'API se collent dans **Sources de données** (`/reglages`) : elles sont écrites dans `.env.local` (ignoré par git) et appliquées sans redémarrage. Sans aucune clé, zenith montre déjà tes dépôts locaux, tes sessions d'agents, la météo, l'actualité et le reste.

## Pour les agents IA

Toutes les 10 minutes, zenith réécrit un brief Markdown de tout ce qu'il sait (`context/brief.md`, un fichier par projet, `vie.md`, `argent.md`…), aussi dans ton vault Obsidian. Pour le donner à tes agents :

- **MCP** : `claude mcp add zenith --scope user -- node /chemin/vers/zenith/scripts/mcp/zenith-mcp.mjs` (Codex : `codex mcp add zenith -- node …`). Pour lire : `zenith_brief`, `zenith_project`, `zenith_document`, `zenith_search_notes`, `zenith_read_note`. Pour agir (zenith lancé) : `zenith_now`, `zenith_delegate` (lancer un autre agent dans un projet), `zenith_agent`, `zenith_done`.
- **HTTP** : `http://127.0.0.1:4747/api/context[/<doc>]` et `/llms.txt`.

## L'agent zenith

Demande à zenith, Maintenant et les routines passent par zenith code, avec ton abonnement Claude Code ou Codex et tes permissions habituelles. Les demandes de vie tournent dans le dossier de l'agent (`~/.zenith/life`), où zenith écrit ses consignes (qui tu es, où tout se trouve, ce qu'il doit demander avant d'agir) et branche son serveur MCP ; il atteint tes mails et ton agenda par tes connecteurs Claude. Il rédige, crée des branches et propose ; il demande avant d'envoyer, payer, supprimer ou déployer. Voir [docs/agent.fr.md](docs/agent.fr.md).

## zenith code

`code/` contient zenith code, un fork de [T3 Code](https://github.com/pingdotgg/t3code) (MIT) renommé, habillé aux couleurs de zenith et branché dessus : il démarre avec zenith, connaît tes projets, s'appaire tout seul dans le tableau de bord et partage sa barre latérale, son ⌘K et ses URL (`/code/<environnement>/<thread>`) : une seule app, pas une app dans l'app. Son état vit dans `~/.zenith/code`. Voir [code/ZENITH.md](code/ZENITH.md).

## Étendre

Les intégrations fournies (GitHub, Railway, RevenueCat, App Store, Obsidian, Claude Code, Codex…) s'activent depuis la config. Pour ce qui ne concerne que tes projets — ta base de données, ton API, des pages entières — écris une extension dans `perso/` (ignoré par git, branché tout seul s'il existe) : voir [docs/extensions.md](docs/extensions.md). Garde `perso/` dans son propre dépôt privé si tu veux une sauvegarde.

`npm run privacy` vérifie qu'aucun fichier suivi ne contient une valeur identifiante de ta config (noms, e-mails, domaines, identifiants…) ni un secret ; `npm run privacy -- --install` le lance avant chaque `git push`.

## Licence

MIT. zenith code est basé sur T3 Code de T3 Tools Inc. (MIT).
