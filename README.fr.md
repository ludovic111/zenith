# zenith

Fait partie de [lsuite](https://lsuite.xyz), la suite créative libre et gratuite que ton IA peut piloter.
Site : **[lsuite.xyz/zenith](https://lsuite.xyz/zenith)** · Soutenir : [lsuite.xyz/zenith/support](https://lsuite.xyz/zenith/support).

**Une app pour coder avec des agents.** zenith fait tourner Claude Code, Codex et d'autres agents de code dans tes projets : des fils que tu suis et orientes, les validations, les plans, les diffs, les worktrees, git et les pull requests — et ce que tout ça coûte. [English version](README.md).

Il tourne sur ta machine : le serveur ne répond que sur `127.0.0.1`, utilise tes propres abonnements Claude et ChatGPT, et n'envoie ton code nulle part ailleurs.

## Ce que tu as

| | |
| --- | --- |
| **Une fenêtre native** | En Rust avec [GPUI](https://gpui.rs) : la barre latérale sur la matière de macOS, chaque fil selon son état (épinglé, actif, en pause, classé), la conversation avec le travail de l'agent résumé en une ligne par tour, des images dans tes messages (coller ou joindre), les validations et questions auxquelles tu réponds sur place, les plans à appliquer d'un clic, les fichiers modifiés à chaque tour et leurs diffs, git (modifications, commit, push, pull requests) et des terminaux (⌘J) là où travaille le fil, les scripts du projet, une palette de commandes (⌘K), clair et sombre, sur le design system lsuite |
| **Fils** | Une conversation par tâche, avec Claude Code ou Codex, dans le dossier du projet ou un nouveau worktree. Des modes de validation de « tout demander » à « accès complet », le mode plan, les interruptions, des points de reprise où revenir |
| **Sessions et coûts** | Chaque session Claude Code et Codex de ce Mac, dans zenith ou dans ton terminal : coût et tokens par jour, par projet, et la commande pour reprendre chacune |
| **Tout est une commande** | Chaque action est une commande nommée (`thread.new`, `thread.approve`, `git.commit`, `terminal.run`…) : la fenêtre, `zenith-cli` et `zenith-mcp` passent par le même registre. Voir [docs/COMMANDS.md](docs/COMMANDS.md) et [docs/AI_CONTROL.md](docs/AI_CONTROL.md) |
| **Pilotable par les agents** | `claude mcp add zenith -- /Applications/zenith.app/Contents/MacOS/zenith-mcp --live` laisse n'importe quel agent lancer des fils, les suivre et y répondre (Réglages › Agents décide jusqu'où). Les agents des fils de zenith reçoivent les serveurs MCP des autres apps lsuite installées (musique, vidéo) |
| **Aussi dans un navigateur** | Le serveur sert toujours l'interface web de zenith (Fichier › Ouvrir dans le navigateur) : la recherche de fichiers et la relecture des pull requests y sont pour l'instant |
| **Mises à jour** | Des versions signées depuis GitHub, installées en un clic (Réglages › Mises à jour ; `ZENITH_NO_UPDATE=1` les coupe) |

## Installer

Télécharge zenith sur [lsuite.xyz/zenith](https://lsuite.xyz/zenith) ou dans les [versions](https://github.com/ludovic111/zenith/releases) (macOS Apple Silicon et Intel, signé et notarisé ; Linux x86_64). Sous macOS, mets-le dans Applications et ouvre-le : il installe son serveur (un LaunchAgent sur `127.0.0.1:4747`) et se met à jour tout seul. Sous Linux, voir [plus bas](#linux).

Depuis les sources (macOS) : [Rust](https://rustup.rs), les outils en ligne de commande de Xcode, git ; Node.js 22.16+ pour construire l'interface web (facultatif). En option : la [CLI GitHub](https://cli.github.com) connectée, Claude Code et/ou Codex.

```bash
git clone https://github.com/ludovic111/zenith.git
cd zenith
npm run mac:install
```

Ça construit zenith, installe **zenith.app**, met `zenith-cli` et `zenith-mcp` dans `~/.local/bin` s'il existe, et garde le serveur en marche en arrière-plan, pour que les agents continuent quand la fenêtre est fermée. À relancer après une mise à jour du code ; `npm run mac:uninstall` retire tout. Tes fils et réglages vivent dans `~/.zenith/code`.

Le serveur est celui en Rust. `ZENITH_SERVER=node npm run mac:install` installe à la place le serveur TypeScript d'origine (certains fournisseurs, comme Cursor ou OpenCode, n'existent encore que là) ; les deux lisent et écrivent les mêmes données.

### Linux

L'archive Linux contient le serveur (`zenith-code`), `zenith-cli`, `zenith-mcp`, la fenêtre (`zenith`) et l'interface web (`client`). Extrais-la dans un dossier et lance `zenith-cli setup` ; une machine sans écran convient aussi (tu utilises alors zenith dans un navigateur, ou avec `zenith-cli` et `zenith-mcp`) :

```bash
mkdir -p ~/.local/share/zenith
curl -L https://github.com/ludovic111/zenith/releases/latest/download/zenith-linux-x86_64.tar.gz | tar -xz -C ~/.local/share/zenith
~/.local/share/zenith/zenith-cli setup
```

`setup` écrit un service systemd utilisateur (`~/.config/systemd/user/zenith.service`) qui fait tourner le serveur sur `127.0.0.1:4747`, l'active, et active le « lingering » (`loginctl enable-linger`) pour qu'il démarre avec la machine, avant toute connexion. Le journal du serveur est `~/.local/state/zenith/server.log` ; `systemctl --user status zenith` montre le service. Relance `setup` après avoir déplacé le dossier ou pour changer une option : il réécrit le service et redémarre le serveur (les agents en cours sont interrompus). Pour avoir les commandes dans ton `PATH` : `ln -s ~/.local/share/zenith/zenith-cli ~/.local/share/zenith/zenith-mcp ~/.local/bin/`.

**Depuis tes autres machines, avec Tailscale.** `zenith-cli setup --tailscale-serve` garde le serveur sur `127.0.0.1` et demande à [Tailscale Serve](https://tailscale.com/kb/1312/serve) de le publier en HTTPS sur ton tailnet, et nulle part ailleurs : `https://mon-serveur.exemple-tailnet.ts.net` (`tailscale serve status` montre la vraie adresse ; `--tailscale-serve-port 8443` choisit un autre port que 443). Tailscale doit laisser ton utilisateur le configurer (`sudo tailscale set --operator=$USER`, une fois). Un navigateur sur une autre machine s'associe avec un lien à usage unique :

```bash
zenith-code auth pairing create --admin --base-dir ~/.zenith/code --base-url https://mon-serveur.exemple-tailnet.ts.net
```

**La fenêtre sur tes autres machines, avec ce serveur.** zenith sur un Mac (ou toute autre machine) peut montrer et piloter les fils d'un serveur ailleurs sur ton tailnet au lieu du sien. Sur le serveur, crée un code à usage unique ; sur l'autre machine, associe-la avec :

```bash
# sur le serveur
zenith-code auth pairing create --admin --ttl 10m --base-dir ~/.zenith/code
# sur le Mac
zenith-cli remote https://mon-serveur.exemple-tailnet.ts.net <code>
```

Dès lors la fenêtre, `zenith-cli` et `zenith-mcp` y utilisent ce serveur (son adresse est dans `~/.zenith/app/remote.json`, sa session dans `~/.zenith/app/remote.token`, 0600), et le Mac ne fait plus tourner de serveur à lui. Le coin de la barre latérale montre sur quelle machine est la fenêtre. « Add Project » liste alors les dossiers du serveur, « Open in Browser » ouvre l'interface web du serveur, déjà connectée. `zenith-cli remote` montre le serveur utilisé ; `zenith-cli remote --off` revient au serveur du Mac. `ZENITH_REMOTE_URL=<url>` utilise un autre serveur le temps d'une commande (`local` force celui de la machine).

Si tu lances `zenith-code serve` toi-même, ne passe jamais `--tailscale-serve` sans `--host 127.0.0.1` : sans `--host`, le serveur écoute sur toutes les interfaces réseau (`0.0.0.0`), pas seulement pour Tailscale.

## Comment c'est fait

- `crates/zenith-app` — la fenêtre, en Rust avec GPUI 0.2 : le thème tiré des tokens lsuite, la vibrance native, son propre champ de texte (méthodes de saisie, sélection, annuler), le Markdown, les menus, la palette.
- `crates/zenith-client` — la connexion au serveur que partagent tous les clients : RPC WebSocket avec une session (gardée en 0600 dans `~/.zenith/app`), reconnexion.
- `crates/zenith-model` — ce que les clients tirent des données du serveur (sections et ordre de la barre latérale, le journal de travail, les demandes en attente, la chronologie), porté depuis l'interface web et testé.
- `crates/zenith-commands` — le registre de commandes, `zenith-cli`, `zenith-mcp`, la découverte lsuite (`~/.lsuite/apps/zenith.json`) et la mise à jour signée.
- `crates/zenith-code` — le serveur, en Rust : le RPC WebSocket et l'API HTTP, la base SQLite à événements, les pilotes Claude Code et Codex, git, worktrees, points de reprise, terminaux, pull requests, usage, MCP ([plan](docs/zenith-code-rust-plan.md), [vérification](docs/zenith-code/verification.md)).
- `code/` — zenith code, un fork de [T3 Code](https://github.com/pingdotgg/t3code) (MIT) : l'interface web (`code/apps/web`, React) et le serveur TypeScript contre lequel celui en Rust est testé. Voir [code/ZENITH.md](code/ZENITH.md).

```bash
cargo test --workspace
cargo clippy --workspace --all-targets
zenith-cli list                          # toutes les commandes
```

Publier : pousse un tag `vX.Y.Z` égal à la version de `Cargo.toml` ; [.github/workflows/release.yml](.github/workflows/release.yml) construit, signe (Developer ID et notarisation avec les secrets `APPLE_*` de lsuite) et publie `zenith-macos-arm64.zip`, `zenith-macos-x86_64.zip`, `zenith-linux-x86_64.tar.gz`, `SHA256SUMS` et sa signature (secret `ZENITH_UPDATE_SIGNING_KEY`).

## Vie privée

- Le serveur n'écoute que sur `127.0.0.1` (sous Linux, `zenith-cli setup --tailscale-serve` laisse en plus Tailscale Serve lui passer les requêtes de ton tailnet ; il écoute toujours sur `127.0.0.1`). La fenêtre, `zenith-cli` et `zenith-mcp` se connectent avec une session que crée la ligne de commande du serveur ; le navigateur reçoit un jeton à usage unique.
- Les agents tournent avec tes propres CLI et abonnements (`claude`, `codex`).
- Ni compte ni télémétrie. La recherche de mises à jour demande à GitHub la dernière version, et peut être coupée.
- `npm run privacy -- --install` vérifie chaque push contre les secrets.

## Licence

MIT. Basé sur T3 Code de T3 Tools Inc. (MIT). Polices : Manrope et IBM Plex Mono (SIL OFL). Icônes : Lucide (ISC).
