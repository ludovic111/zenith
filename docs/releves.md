# Snapshots captured by Claude

*[Français](releves.fr.md)*

Some data comes through Claude's connectors (Google Calendar, Gmail) or from the Claude app itself: zenith has no direct access to it. Claude captures it and writes it to `.data/` (ignored by git); zenith reads it on every page load and shows how old it is.

| File | Page | What to ask Claude |
| --- | --- | --- |
| `.data/life.json` | My life, overview | "update my life in zenith" |
| `.data/claude-plan.json` | Subscriptions, Agents | "update my Claude limits in zenith" |
| `zenith.config.json` (`subscriptions`) | Subscriptions | "go through my receipts and update my subscriptions in zenith" |

## My life (`.data/life.json`)

Read-only, from Google Calendar and Gmail:

- `agenda`: events of the next 14 days, across all calendars.
- `inbox`: unread, unread important, and at most 8 messages from real people or important services that need an action (buyers, invitations, government offices…), no newsletters or notifications.
- `deliveries`: parcels on their way, over the last 14 days.
- `sales`: items for sale and buyers' messages over 14 days.
- `spending`: spending of the last 3 months from receipts, excluding subscriptions (food delivery, ride-hailing, restaurants, shopping, other), in the currency given by `spending.currency` (ideally the config's `currency`).
- `civic`: obligations and paperwork (taxes, official letters, civic duties…).
- `notes`: 3 to 6 useful observations.

The exact schema is the `Life` type in `src/lib/sources/life.ts`. No street address, phone number, card or transaction number.

## Claude limits (`.data/claude-plan.json`)

Captured with the Claude app's usage tool: `{ plan, windows: [{ label, percentUsed, resetsAt }], capturedAt }`.

## Snapshots captured by zenith.app

`.data/apple.json`, sent every 5 minutes by the native app to `/api/apple`:

- Calendar and Reminders (EventKit).
- Mail (AppleScript, only while Mail is running).
- Birthdays (Contacts): name and date only.
- Music and Spotify (AppleScript, every minute, only while they run): now playing and the last 40 tracks.
- Screen time: the frontmost app is noted every 20 s, except when the screen is locked or after 3 minutes without keyboard or mouse; 14 days are kept in the app's preferences.

Permissions are managed in System Settings → Privacy & Security (Calendars, Reminders, Contacts, Automation). After a code update, `npm run mac:install` rebuilds the app.

## Already live

Obsidian, weather, air, UV and pollen (Open-Meteo, for `location`), Swiss rivers and lakes (FOEN, for `water`), Swiss public transport departures (transport.opendata.ch, for `transit`), public holidays (Nager.Date, for `location.country`), your RSS feeds (`news`), Hacker News and mentions of your projects (`watch`), work rhythm (`~/.claude` and `~/.codex` sessions, commits), Codex limits, domains, App Store (listing and reviews), Railway, RevenueCat, GitHub (including notifications, stars, contributions, mentions and Sponsors), OpenRouter, crypto (Kraken), exchange rates (ECB), this Mac (disk, memory, dev servers, Homebrew), and your own extensions (see [extensions.md](extensions.md)).
