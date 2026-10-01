# zenith

Fait partie de [lsuite](https://lsuite.xyz), la suite créative libre et gratuite que ton IA peut piloter.
Site : **[lsuite.xyz/zenith](https://lsuite.xyz/zenith)**.

**Une app Mac pour coder avec des agents.** zenith fait tourner Claude Code, Codex et d'autres agents de code dans tes projets : des fils que tu suis et orientes, les validations, les plans, les diffs, les terminaux, les worktrees, git et les pull requests — et ce que tout ça coûte. [English version](README.md).

Il tourne sur ta machine : le serveur ne répond que sur `127.0.0.1`, utilise tes propres abonnements Claude et ChatGPT, et n'envoie ton code nulle part ailleurs.

## Ce que tu as

| | |
| --- | --- |
| **Fils** | Une conversation par tâche, avec Claude Code ou Codex. Validations, mode plan, interruptions, relances pendant qu'il travaille, images dans tes messages |
| **Code** | Diffs par tour et par fil, points de reprise où revenir, un worktree par fil, recherche de fichiers, terminaux intégrés |
| **Git et PR** | Commit, push et pull request en un geste (messages écrits pour toi), relecture des pull requests GitHub, GitLab, Azure DevOps, Forgejo ou Bitbucket |
| **Sessions et coûts** | Chaque session Claude Code et Codex, dans zenith ou dans ton terminal : coût et tokens, dépenses par jour et par modèle, les limites de tes forfaits |
| **Pilotable par les agents** | Chaque fil donne à son agent le serveur MCP de zenith (`/mcp`), pour lier à son fil les pull requests qu'il ouvre |
| **App Mac** | Une vraie fenêtre : barre latérale translucide, feux dans la barre de titre, apparence claire et sombre, menus, notifications |

## Installer

Prérequis : macOS, [Rust](https://rustup.rs), Node.js 22.16+ (24+ conseillé, pour construire l'interface), git. En option : la [CLI GitHub](https://cli.github.com) connectée, Claude Code et/ou Codex.

```bash
git clone https://github.com/ludovic111/zenith.git
cd zenith
npm install
npm run mac:install
```

Ça construit zenith, installe **zenith.app** et garde son serveur en marche en arrière-plan (un LaunchAgent sur `127.0.0.1:4747`), pour que les agents continuent quand la fenêtre est fermée. À relancer après une mise à jour ; `npm run mac:uninstall` retire tout. Tes fils et réglages vivent dans `~/.zenith/code`.

Le serveur est celui en Rust. `ZENITH_SERVER=node npm run mac:install` installe à la place le serveur TypeScript d'origine ; les deux lisent et écrivent les mêmes données, tu peux passer de l'un à l'autre.

## Comment c'est fait

- `crates/zenith-code` — le serveur, en Rust : le RPC WebSocket et l'API HTTP de l'interface, la base SQLite à événements, les pilotes Claude Code et Codex, git, worktrees, points de reprise, terminaux, pull requests, usage, MCP. C'est un portage du serveur TypeScript de zenith code, vérifié contre lui ([plan](docs/zenith-code-rust-plan.md), [vérification](docs/zenith-code/verification.md)).
- `crates/zenith-app` — zenith.app, une fenêtre [Tauri](https://tauri.app) autour de l'interface.
- `code/` — zenith code, un fork de [T3 Code](https://github.com/pingdotgg/t3code) (MIT) : l'interface web (`code/apps/web`, React) et le serveur TypeScript contre lequel celui en Rust est testé. Voir [code/ZENITH.md](code/ZENITH.md).

```bash
cargo test --workspace                  # le serveur Rust et l'app
cargo clippy --workspace --all-targets
```

## Vie privée

- Le serveur n'écoute que sur `127.0.0.1` ; l'interface se connecte avec un jeton à usage unique que l'app crée elle-même.
- Les agents tournent avec tes propres CLI et abonnements (`claude`, `codex`).
- `npm run privacy -- --install` vérifie chaque push contre les secrets.

## Licence

MIT. Basé sur T3 Code de T3 Tools Inc. (MIT).
