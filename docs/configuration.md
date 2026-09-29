# Configuration

Everything personal zenith knows about you (who you are, where you live, your projects, your subscriptions) lives in **one file**: `zenith.config.json`. zenith looks for it in this order: the path in `ZENITH_CONFIG`, then `perso/zenith.config.json` (next to your [private extensions](extensions.md)), then `zenith.config.json` next to `package.json`. Git ignores both default locations. Secrets (API keys, tokens) do **not** go here: they go in `.env.local` (see `.env.example`), or can be pasted from the **Data sources** page (`/reglages`).

```bash
cp zenith.config.example.json zenith.config.json
# edit it, then restart zenith (npm run dev, or npm run mac:install for the Mac app)
```

- **Another location**: set `ZENITH_CONFIG=/path/to/my-config.json`. The Data sources page shows which file was read.
- **Autocompletion**: keep `"$schema": "./zenith.schema.json"` at the top of the file; VS Code, Zed, JetBrains… then complete and check every field. `npm run config:schema` regenerates `zenith.schema.json` from `src/lib/config-schema.ts` (the source of truth for this page).
- **Read once**: the file is read at startup. Restart zenith after editing it.
- **Errors**: if the file is not valid JSON or does not match the schema, zenith starts with an empty configuration and the Data sources page lists every error (`projects.0.id: lowercase letters, digits and dashes`…).
- **Every field is optional.** Without a config, zenith starts empty, in English, in your computer's time zone, and the Data sources page explains how to fill it in.

## Top level

| Field | Type | Default | What it does |
| --- | --- | --- | --- |
| `$schema` | string | — | `"./zenith.schema.json"`, for editor autocompletion. |
| `locale` | string | `"en-US"` | BCP 47 locale. Any `fr-*` locale (`fr-FR`, `fr-CH`, `fr-CA`…) switches the interface and the agent brief to French; anything else is English. Also used to format numbers and dates. |
| `timezone` | string | this computer's | IANA time zone (`"Europe/Paris"`, `"America/New_York"`). Used for every date, "today", work rhythm and the brief. |
| `currency` | string | `"USD"` | ISO 4217 code. Every total (subscriptions, revenue) is converted to it with the day's ECB rates. |
| `owner` | object | empty | You. See [owner](#owner). |
| `location` | object | — | Your city. Turns on weather, air quality, UV, pollen and public holidays. See [location](#location). |
| `projectsRoot` | string | parent folder of zenith | Folder holding your repositories. `~` is expanded. `PROJECTS_ROOT` is used when this is not set. |
| `projects` | array | `[]` | Your projects, in display order (the order also picks each one's color). See [projects](#projects). |
| `subscriptions` | array | `[]` | Subscriptions and recurring fees. See [subscriptions](#subscriptions). |
| `news` | array | `[]` | RSS feeds for the news panel. See [news](#news). |
| `watch` | array | `[]` | Words that mean you or your projects, searched on Hacker News and GitHub. See [watch](#watch). |
| `transit` | object | — | Public transport departures (Switzerland only). See [transit](#transit). |
| `water` | object | — | Swiss rivers and lakes. See [water](#water). |
| `obsidian` | object | `{ "exportDir": "zenith" }` | Your Obsidian vault. See [obsidian](#obsidian). |
| `mac` | object | `{ "bundleId": "dev.zenith.app" }` | The native Mac app. See [mac](#mac). |
| `assistants` | array | `["claude", "chatgpt"]` | Claude and ChatGPT in the sidebar. See [assistants](#assistants). |
| `code` | object | `{ "enabled": true, "port": 4749 }` | zenith code, the coding workspace. See [code](#code). |
| `agent` | object | `{ "enabled": true, "provider": "claude" }` | The zenith agent: Ask zenith, Now, routines. See [agent](#agent). |

```json
{
  "$schema": "./zenith.schema.json",
  "locale": "fr-FR",
  "timezone": "Europe/Paris",
  "currency": "EUR"
}
```

## owner

| Field | Type | What it does |
| --- | --- | --- |
| `name` | string | Your full name: page titles, the brief, the directory. |
| `firstName` | string | How the greeting calls you. Defaults to the first word of `name`. |
| `emails` | `{ address, role }[]` | Your addresses and what each one is for. |
| `socials` | `{ network, handle, url, note? }[]` | Your accounts. `network` is one of `x`, `instagram`, `youtube`, `tiktok`, `github`, `telegram`, `linkedin`, `bluesky`, `mastodon`, `threads`. Your `github` handle is also used as your GitHub login in the brief and to exclude your own repositories from mentions. |
| `accounts` | `{ brand, label, value?, url? }[]` | Other accounts (developer consoles, stores…). `brand` picks the icon: a network above, or `appstore`, `googleplay`, `railway`, `supabase`, `revenuecat`, `openrouter`, `solana`, `gmail`, `apple`, `vercel`, `cloudflare`, `stripe`, `web`. |
| `sponsors` | string | GitHub login whose GitHub Sponsors income is counted on the money panel. |

```json
"owner": {
  "name": "Ada Lovelace",
  "firstName": "Ada",
  "emails": [{ "address": "ada@example.com", "role": "Personal" }],
  "socials": [{ "network": "github", "handle": "ada", "url": "https://github.com/ada" }],
  "accounts": [{ "brand": "appstore", "label": "App Store Connect", "url": "https://appstoreconnect.apple.com" }],
  "sponsors": "ada"
}
```

## location

| Field | Type | What it does |
| --- | --- | --- |
| `name` | string (required) | City name, as shown ("Lyon", "New York"). |
| `latitude`, `longitude` | number (required) | For weather, air quality, UV and pollen (Open-Meteo, no key). |
| `country` | string | ISO country code (`FR`, `US`, `CH`…) for public holidays (Nager.Date, no key). |
| `region` | string | ISO subdivision (`US-NY`, `CH-GE`, `DE-BY`…) to add regional holidays. |

```json
"location": { "name": "Lyon", "latitude": 45.764, "longitude": 4.8357, "country": "FR" }
```

## projects

Each project gets a card on the overview, a page at `/p/<id>`, a document for agents (`context/projets/<id>.md`) and whatever integrations you switch on.

| Field | Type | What it does |
| --- | --- | --- |
| `id` | string (required) | Lowercase letters, digits and dashes. Stable: used in URLs and file names. |
| `name` | string (required) | Display name. |
| `tagline` | string | One line under the name. |
| `emoji` | string | Default `✦`. |
| `color`, `glow` | string | CSS colors. Default: taken from the palette in order. |
| `href` | string | zenith page of the project. Default `/p/<id>`. |
| `repo` | string | GitHub repository, `"owner/name"`: CI, issues, pull requests, stars, releases. |
| `dir` | string | Local folder: a name under `projectsRoot`, or an absolute path (`~` allowed). Commits, branch, uncommitted work, version, agent sessions. |
| `extraDirs` | string[] | Other folders whose Claude Code / Codex sessions count for this project. |
| `site` | string | Public URL. |
| `probes` | `{ label, url }[]` | URLs checked every minute for uptime (401 counts as up). |
| `shot` | string | Screenshot shown on the overview card, relative to `projectsRoot`. |
| `railway` | `{ projectId, serviceId, environmentId }` | Railway deployments and HTTP traffic (needs `RAILWAY_TOKEN`). |
| `links` | `{ label, url }[]` | Buttons on the project page. |
| `notes` | string | Regular expression matched against Obsidian note paths to find the project's notes. Default: the id or the name. |
| `version` | `{ file, pattern?, json? }` | Where to read the version: a file of the repository, and either a JSON path (`"expo.version"`) or a regex whose first group is the version. Default: `package.json`, `Cargo.toml` or `pyproject.toml`. |
| `appStore` | `{ id, countries? }` | App Store app id (digits only): rating, live version and written reviews (`countries`: ISO codes to read reviews from). No key. |
| `revenuecat` | `{ projectId }` | RevenueCat project id: MRR, revenue, subscribers (needs `REVENUECAT_API_KEY`). |
| `releases` | boolean | Count GitHub release downloads (archives only, demos excluded). Needs `repo`. |
| `highlights` | string[] | Short chips on the project page. |
| `about` | string | A sentence on the project page. |
| `trafficTitle` | string | Title of the traffic panel (Railway HTTP logs). |
| `showcase` | boolean | Show every project's screenshot on this project's page (for a showcase site). |
| `identity` | object | The project's identity card, for the directory and agents. See below. |

`identity` holds what no API can guess:

| Field | Type | What it does |
| --- | --- | --- |
| `names` | `{ label, value, mono? }[]` | Names in use (store name, legal name…). |
| `domains` | string[] | Domains; registrar, renewal date and mail provider are checked live. |
| `emails` | `{ address, role }[]` | Project addresses. |
| `socials` | `{ network, handle, url, note? }[]` | Project accounts. |
| `stores` | `{ brand, label, value?, url? }[]` | Store listings. |
| `ids` | `{ label, value, mono? }[]` | Identifiers (bundle id, package name, URL scheme…). |
| `services` | `{ brand, label, value?, url? }[]` | Services the project runs on. |
| `todo` | string[] | Things to do, shown on the project page and in the brief. |

```json
"projects": [
  {
    "id": "my-app",
    "name": "My App",
    "tagline": "Habit tracker for iOS",
    "emoji": "🚀",
    "repo": "ada/my-app",
    "dir": "my-app",
    "site": "https://myapp.example",
    "probes": [{ "label": "myapp.example", "url": "https://myapp.example" }],
    "railway": { "projectId": "…", "serviceId": "…", "environmentId": "…" },
    "appStore": { "id": "1234567890", "countries": ["us", "gb", "fr"] },
    "revenuecat": { "projectId": "a1b2c3d4" },
    "version": { "file": "app.json", "json": "expo.version" },
    "links": [{ "label": "Site", "url": "https://myapp.example" }],
    "identity": {
      "domains": ["myapp.example"],
      "ids": [{ "label": "Bundle id", "value": "com.example.myapp", "mono": true }],
      "todo": ["Answer the latest App Store review"]
    }
  }
]
```

## subscriptions

| Field | Type | Default | What it does |
| --- | --- | --- | --- |
| `name` | string (required) | — | What you pay for. |
| `vendor` | string | `""` | Who you pay. |
| `category` | `ia`, `infra`, `domains`, `tools`, `music`, `personal` | `tools` | Grouping (`ia` = AI). |
| `amount` | number or null | `null` | Price per period; null when unknown. |
| `currency` | string | `"USD"` | Currency of `amount`; totals are converted to `currency`. |
| `period` | `month`, `year`, `week`, `usage` | `month` | `usage` counts as zero in monthly totals. |
| `last_charge`, `next_renewal` | `"YYYY-MM-DD"` or null | `null` | Dates shown on the page. |
| `status` | `active`, `failing`, `cancelled`, `unknown` | `active` | `failing` shows up as urgent everywhere. |
| `project` | string or null | `null` | Project id this cost belongs to. |
| `evidence` | string | `""` | Where the information comes from, or what is wrong (shown next to failing payments). |
| `manage_url` | string | — | Link to the billing page. |

```json
"subscriptions": [
  { "name": "Claude Pro", "vendor": "Anthropic", "category": "ia", "amount": 20, "currency": "USD", "period": "month", "manage_url": "https://claude.ai/settings/billing" },
  { "name": "Domain myapp.example", "vendor": "Registrar", "category": "domains", "amount": 12, "currency": "USD", "period": "year", "next_renewal": "2027-03-01", "project": "my-app" }
]
```

Claude can keep this list up to date from your receipts: see [releves.md](releves.md).

## news

RSS 2.0 feeds for the news panel and the watch document.

| Field | Type | Default |
| --- | --- | --- |
| `name` | string (required) | — |
| `url` | string (required) | — |
| `limit` | number | `8` |

```json
"news": [{ "name": "Le Monde", "url": "https://www.lemonde.fr/rss/une.xml", "limit": 6 }]
```

## watch

Terms searched on Hacker News (Algolia) and in other people's GitHub issues and pull requests, over 90 days. Pick distinctive terms: a common word brings noise.

| Field | Type | What it does |
| --- | --- | --- |
| `term` | string (required) | The word to look for. |
| `label` | string (required) | What it means (a project name, "Me"…). |

```json
"watch": [{ "term": "myapp", "label": "My App" }, { "term": "adalovelace", "label": "Me" }]
```

## transit

Next departures from one stop, through transport.opendata.ch. **Switzerland only.** `TRANSIT_STOP` in `.env.local` overrides it.

```json
"transit": { "stop": "Lausanne, Flon" }
```

## water

Temperature and flow of rivers (or level of lakes) from FOEN hydrology stations, served by existenz.ch. **Switzerland only.**

| Field | Type | What it does |
| --- | --- | --- |
| `stations[].id` | string | FOEN station number. |
| `stations[].name` | string | Station name, as shown. |
| `stations[].water` | string | River or lake name. |
| `stations[].level` | boolean | `true` to show the water level instead of the flow (lakes). |

```json
"water": { "stations": [{ "id": "2135", "name": "Bern, Schönau", "water": "Aare" }] }
```

## obsidian

| Field | Type | Default | What it does |
| --- | --- | --- | --- |
| `vault` | string | — | Path to your vault. `OBSIDIAN_VAULT` wins over it. |
| `fallback` | string | — | Vault used when `vault` is not set and no vault is open in Obsidian. |
| `exportDir` | string | `"zenith"` | Folder of the vault where zenith writes its summary every 10 minutes (the only place it ever writes). `OBSIDIAN_EXPORT=0` turns writing off. |

Without `vault`, zenith uses the vault currently open in Obsidian.

```json
"obsidian": { "vault": "~/Documents/Notes", "exportDir": "zenith" }
```

## mac

| Field | Type | Default | What it does |
| --- | --- | --- | --- |
| `bundleId` | string | `"dev.zenith.app"` | Bundle identifier of zenith.app. Changing it resets the macOS permissions (Calendars, Reminders, Contacts, Automation). |

## assistants

`"claude"` and `"chatgpt"`, in sidebar order; `[]` hides them. Each gets a page at `/assistants/<id>`.

In zenith.app, the page docks the app's **desktop** app in zenith's window: zenith moves the real Claude.app / ChatGPT.app window over the page, keeps it there when you move or resize zenith, and hides it when you leave. Its plugins, connectors and desktop extensions are all there. This uses the Accessibility API: the first time, allow zenith in System Settings → Privacy & Security → Accessibility (the page has a button). Rebuilding zenith.app (`npm run mac:install`) can ask for it again, since the app is signed locally.

Until then, or if the desktop app isn't installed, or if you pick **Web** in the page's toolbar, zenith shows claude.ai / chatgpt.com in a native web view instead (sign in once; Safari's cookie store keeps you signed in). **Detach** gives the app its own window back. In a browser, neither can be embedded: the page opens the app.

## code

| Field | Type | Default | What it does |
| --- | --- | --- | --- |
| `enabled` | boolean | `true` | Turns zenith code (the coding workspace at `/code`) on or off. |
| `port` | number | `4749` | Local port of its server. |
| `home` | string | — | Where zenith code keeps its state. |

## agent

The zenith agent (see [agent.md](agent.md)). It runs through zenith code, so `code.enabled` must stay on.

| Field | Type | Default | What it does |
| --- | --- | --- | --- |
| `enabled` | boolean | `true` | Shows Ask zenith (⌘J), the Now list and the routines. |
| `home` | string | `"~/.zenith/life"` | The agent's own folder, where conversations about your life run. zenith writes its instructions there. |
| `provider` | `"claude"` \| `"codex"` | `"claude"` | Who answers by default (switchable in the ask bar). |
| `model` | string | — | Model id for that provider, e.g. `"claude-opus-5-5"`. Default: zenith code's default model, else the one of your latest thread. |
| `routines` | array | `[]` | Agents that run on their own, once a day. |

Each routine:

| Field | Type | What it does |
| --- | --- | --- |
| `id` | string | Lowercase letters, digits and dashes. |
| `at` | `"HH:MM"` | Local time. A Mac asleep then catches up within three hours. |
| `days` | number[] | ISO weekdays, 1 = Monday … 7 = Sunday. Default: every day. |
| `task` | `"refresh-life"` | A built-in request: capture Gmail and Google Calendar into My life. |
| `prompt` | string | Or your own request, in your words. |
| `project` | string | A project id to run it in. Default: the agent's folder. |
| `name` | string | Shown in AI agents → Routines. |
| `enabled` | boolean | `false` pauses it. |

```json
"agent": {
  "routines": [
    { "id": "morning", "at": "07:30", "task": "refresh-life" },
    { "id": "weekly-review", "at": "18:00", "days": [5], "prompt": "Review my week: what shipped, what slipped, what to do Monday. Keep it to 10 lines." }
  ]
}
```

## A complete example

[`zenith.config.example.json`](../zenith.config.example.json) is a small, valid configuration to start from.
