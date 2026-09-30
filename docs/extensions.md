# Private extensions

zenith's shared pages (overview, money, agent brief, Data sources) never know about any particular project. Everything project-specific reaches them through **extensions**:

- **Built-in** ones live in `src/lib/integrations.ts` and switch on from `zenith.config.json`: RevenueCat (`projects[].revenuecat`), GitHub release downloads (`projects[].releases`), App Store ratings and reviews (`projects[].appStore`), GitHub Sponsors (`owner.sponsors`).
- **Yours** live in `perso/index.ts`, at the root of the repository: your own database, your trading bot, your analytics, your own pages, anything no built-in integration covers.

`perso/` is yours only: git ignores it (it can also hold your `zenith.config.json`), so your code, your URLs and your project names never leave your machine. When the folder exists, `next.config.ts` points the `@perso` alias at `perso/index.ts`; otherwise it falls back to `src/lib/perso-empty.ts`, an empty list. The smallest valid `perso/index.ts`:

```ts
import "server-only";
import type { Extension } from "@/lib/extensions";

export const PERSO: Extension[] = [];
```

Before pushing to a public fork, `npm run privacy` scans every tracked file for the identifying values of your config, the words in `perso/denylist.txt` (with `perso/allowlist.txt` for false positives) and secret formats; `npm run privacy -- --install` runs it as a git pre-push hook.

Inside `perso/`, import zenith's code with the usual `@/` alias (`@/lib/source`, `@/lib/format`…) and your own files with relative paths.

## The `Extension` interface

Defined in `src/lib/extensions.ts`. Every field is optional except `id`: implement only what you need.

```ts
type Extension = {
  id: string;
  /** Overview: KPI tiles. */
  kpis?: () => Promise<Kpi[]>;
  /** Overview: up to three numbers on a project's card. The first extension that answers wins. */
  card?: (p: Project) => Promise<CardStat[] | null>;
  /** Overview: ticker and feed. */
  events?: () => Promise<Event[]>;
  /** Overview: money in and out. */
  money?: () => Promise<MoneyRow[]>;
  /** Agent brief: one-line facts about a project. */
  facts?: (p: Project) => Promise<string[]>;
  /** Agent brief: extra Markdown for a project's document. */
  context?: (p: Project) => Promise<string>;
  /** Settings page: the data sources this extension reads. */
  sources?: () => Promise<SourceRow[]>;
  /** Env vars the settings page may write to .env.local. */
  keys?: string[];
  /** Whole pages, served at /<slug> (a project's `href` can point there). */
  pages?: { slug: string; title: string; Page: ComponentType }[];
  /** API routes, served at /api/perso/<path> (GET only, same-origin). */
  routes?: { path: string; GET: (request: Request) => Promise<Response> | Response }[];
};
```

| Hook | Where it shows up | Shape |
| --- | --- | --- |
| `kpis` | Tiles at the top of the overview | `{ label, value, project?, format?, suffix?, hint? }`: `value` null means "unavailable"; `format` is passed to `Intl.NumberFormat`. |
| `card` | The numbers on a project's card | `{ label, value }[]` (formatted strings), or null to let the next extension answer. |
| `events` | Overview ticker and feed | `{ project, at, kind, text, href? }`; `at` in ms; `kind` is `commit`, `signup`, `release`, `deploy`, `trade`, `agent`, `review` or `sale`. |
| `money` | Money panel and the brief's money section | `{ label, value, currency, sign, hint, project? }`: `sign` 1 for income, -1 for spending. |
| `facts` | One bullet each under the project in `brief.md` and `projets/<id>.md` | Short sentences. Return `[]` for projects that are not yours to describe. |
| `context` | A block appended to `projets/<id>.md` | Markdown, usually a `## Heading` and a list. Return `""` when there is nothing to say. |
| `sources` | A row on Settings → Data sources (`/reglages/sources`) | `SourceRow`: `group` (`projects`, `around`, `app` or `claude`), `name`, `feeds`, `vars`, `how`, `url?`, `src` (the result of `source(...)`), and `key?`, `placeholder?`, `secret?` to show a paste-your-key form. |
| `keys` | Env vars Settings → Data sources is allowed to write to `.env.local` | Names only. Anything not listed (here or built in) is refused. |
| `pages` | A whole page at `/<slug>` | `{ slug, title, Page }`: `Page` is a React server component. Point a project's `href` at `/<slug>` to use it as the project's page. |
| `routes` | An API route at `/api/perso/<path>` | `{ path, GET }`: GET only, same-origin, for your pages' client components. |

Rules of thumb:

- **Never throw for a missing key.** Call `need("MY_KEY")` from `src/lib/source.ts`: it throws a `MissingConfig` that `source()` turns into a friendly "to connect (MY_KEY)" state. `collect()` also catches any error an extension throws, logs it and moves on: a broken extension never breaks a page.
- **Cache network calls** with `cached(key, seconds, fn)`: the overview, the brief and the settings page all ask at once.
- **Filter by project** in `card`, `facts` and `context`: they are called for every project in the config.
- **Localize** with `tr("French", "English")` from `src/lib/i18n.ts` if you want both languages; if you only use one, plain strings are fine: it is your code.
- Extensions run on the server only (`import "server-only"`). Keys never reach the browser.

## Example

A private analytics service that counts visitors of one of your sites, reads `ANALYTICS_API_KEY`, and adds a KPI tile, a fact for the brief, a settings row and a paste-your-key form.

```ts
// perso/index.ts
import "server-only";
import type { Extension } from "@/lib/extensions";
import { cached, getJson, need, source } from "@/lib/source";
import { findProject } from "@/lib/projects";
import { nf } from "@/lib/format";
import { tr } from "@/lib/i18n";

const PROJECT = "my-app"; // a project id from zenith.config.json

/** Visitors over the last 7 days. */
const visitors = () =>
  cached("analytics:visitors", 900, async () => {
    const [key] = need("ANALYTICS_API_KEY");
    const d = await getJson<{ visitors: number }>("https://analytics.example.com/api/v1/stats?period=7d", {
      headers: { Authorization: `Bearer ${key}` },
    });
    return d.visitors;
  });

const analytics: Extension = {
  id: "analytics",
  kpis: async () => {
    const v = await source(visitors);
    return [{ label: tr("Visiteurs 7 j", "Visitors 7 d"), project: PROJECT, value: v.ok ? v.data : null, hint: v.ok ? undefined : tr("Clé à brancher", "Connect the key") }];
  },
  card: async (p) => {
    if (p.id !== PROJECT) return null;
    const v = await source(visitors);
    return [{ label: tr("visiteurs 7 j", "visitors 7 d"), value: v.ok ? nf(v.data) : "—" }];
  },
  facts: async (p) => {
    if (p.id !== PROJECT) return [];
    const v = await source(visitors);
    return v.ok ? [tr(`${nf(v.data)} visiteurs sur 7 jours`, `${nf(v.data)} visitors over 7 days`)] : [];
  },
  sources: async () => [
    {
      group: "projects",
      name: `Analytics · ${findProject(PROJECT)?.name ?? PROJECT}`,
      feeds: tr("Visiteurs du site", "Site visitors"),
      vars: "ANALYTICS_API_KEY",
      how: tr("Réglages → API → créer une clé en lecture seule.", "Settings → API → create a read-only key."),
      url: "https://analytics.example.com/settings/api",
      src: await source(visitors),
      key: "ANALYTICS_API_KEY",
      placeholder: "an_…",
    },
  ],
  keys: ["ANALYTICS_API_KEY"],
};

export const PERSO: Extension[] = [analytics];
```

Restart zenith after adding an extension. Its tile appears on the overview, its fact in `context/brief.md` (rewritten every 10 minutes, and served live at `/api/context`), and its row on Settings → Data sources, where pasting the key writes it to `.env.local` and lights the source up without a restart.

## Custom pages

A project's page defaults to the generic `/p/<id>`. To give one of your projects a page of its own, write it as a server component in `perso/` (say `perso/pages/my-app.tsx`), declare it in your extension, and point the project's `href` at it:

```ts
import MyAppPage from "./pages/my-app";

const myApp: Extension = {
  id: "my-app",
  pages: [{ slug: "my-app", title: "My App", Page: MyAppPage }],
};
```

```json
{ "id": "my-app", "name": "My App", "href": "/my-app" }
```
