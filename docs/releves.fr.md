# Relevés faits par Claude

*[English](releves.md)*

Certaines données passent par les connecteurs de Claude (Google Agenda, Gmail) ou par l'app Claude elle-même : zenith n'y a pas d'accès direct. Claude les relève et les écrit dans `.data/` (ignoré par git), zenith les lit à chaque affichage et indique leur âge.

| Fichier | Page | Demande à faire à Claude |
| --- | --- | --- |
| `.data/life.json` | Ma vie, vue d'ensemble | « mets à jour ma vie dans zenith » |
| `.data/claude-plan.json` | Abonnements, Agents | « mets à jour mes limites Claude dans zenith » |
| `zenith.config.json` (`subscriptions`) | Abonnements | « relis mes reçus et mets à jour mes abonnements dans zenith » |

## Ma vie (`.data/life.json`)

En lecture seule, depuis Google Agenda et Gmail :

- `agenda` : événements des 14 prochains jours, tous agendas confondus.
- `inbox` : non lus, non lus importants, et au plus 8 messages de vraies personnes ou de services importants qui attendent une action (acheteurs, invitations, administrations…), sans newsletters ni notifications.
- `deliveries` : colis en route depuis 14 jours.
- `sales` : objets en vente et messages d'acheteurs sur 14 jours.
- `spending` : dépenses des 3 derniers mois d'après les reçus, hors abonnements (livraison de repas, courses en VTC, restaurants, shopping, autres), dans la devise indiquée par `spending.currency` (de préférence celle de `currency` dans la config).
- `civic` : obligations et administratif (impôts, courriers officiels, service civil…).
- `notes` : 3 à 6 observations utiles.

Le schéma exact est le type `Life` de `src/lib/sources/life.ts`. Aucune adresse, numéro de téléphone, de carte ou de transaction.

## Limites Claude (`.data/claude-plan.json`)

Relevé par l'outil d'usage de l'app Claude : `{ plan, windows: [{ label, percentUsed, resetsAt }], capturedAt }`.

## Relevés faits par zenith.app

`.data/apple.json`, envoyé toutes les 5 minutes par l'app native à `/api/apple` :

- Calendrier et Rappels (EventKit).
- Mail (AppleScript, seulement si Mail tourne).
- Anniversaires (Contacts) : nom et date seulement.
- Musique et Spotify (AppleScript, chaque minute, seulement s'ils tournent) : morceau en cours et 40 dernières écoutes.
- Temps d'écran : l'app au premier plan est notée toutes les 20 s, sauf écran verrouillé ou 3 minutes sans clavier ni souris ; 14 jours gardés dans les préférences de l'app.

Les autorisations se gèrent dans Réglages Système → Confidentialité et sécurité (Calendriers, Rappels, Contacts, Automatisation). Après une mise à jour du code, `npm run mac:install` recompile l'app.

## Ce qui est déjà en direct

Obsidian, météo, air, UV et pollens (Open-Meteo, pour `location`), rivières et lacs suisses (OFEV, pour `water`), départs de transports publics suisses (transport.opendata.ch, pour `transit`), jours fériés (Nager.Date, pour `location.country`), tes flux RSS (`news`), Hacker News et mentions de tes projets (`watch`), rythme de travail (sessions `~/.claude` et `~/.codex`, commits), limites Codex, domaines, App Store (fiche et avis), Railway, RevenueCat, GitHub (dont notifications, étoiles, contributions, mentions et Sponsors), OpenRouter, cryptos (Kraken), change (BCE), ce Mac (disque, mémoire, serveurs de dev, Homebrew), et tes propres extensions (voir [extensions.md](extensions.md)).
