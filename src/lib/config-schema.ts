import { z } from "zod";

/** The shape of zenith.config.json. Kept free of server imports so scripts can use it. */

export const PALETTE = [
  ["#1B9CD8", "#00D2FF"],
  ["#E85A26", "#FF8A4C"],
  ["#8F5CFF", "#B18CFF"],
  ["#6CA81A", "#B6F23A"],
  ["#E5418A", "#FF6FB5"],
  ["#D9A21B", "#FFD166"],
  ["#14A38B", "#3EF0CF"],
  ["#5B6CFF", "#8FA0FF"],
] as const;

const Link = z.object({ label: z.string(), url: z.string() });
const Field = z.object({ label: z.string(), value: z.string(), mono: z.boolean().optional() });
const Email = z.object({ address: z.string(), role: z.string().default("") });
const Network = z.enum(["x", "instagram", "youtube", "tiktok", "github", "telegram", "linkedin", "bluesky", "mastodon", "threads"]);
const Brand = z.union([
  Network,
  z.enum(["appstore", "googleplay", "railway", "supabase", "revenuecat", "openrouter", "solana", "gmail", "apple", "vercel", "cloudflare", "stripe", "web"]),
]);
const Social = z.object({ network: Network, handle: z.string(), url: z.string(), note: z.string().optional() });
const BrandLink = z.object({ brand: Brand, label: z.string(), value: z.string().optional(), url: z.string().optional() });

const Identity = z.object({
  names: z.array(Field).default([]),
  domains: z.array(z.string()).default([]),
  emails: z.array(Email).default([]),
  socials: z.array(Social).default([]),
  stores: z.array(BrandLink).default([]),
  ids: z.array(Field).default([]),
  services: z.array(BrandLink).default([]),
  todo: z.array(z.string()).default([]),
});

export const ProjectInput = z.object({
  id: z.string().regex(/^[a-z0-9][a-z0-9-]*$/, "lowercase letters, digits and dashes"),
  name: z.string(),
  tagline: z.string().default(""),
  color: z.string().optional(),
  glow: z.string().optional(),
  emoji: z.string().default("✦"),
  /** Page of the project in zenith. Defaults to the generic page /p/<id>. */
  href: z.string().optional(),
  /** GitHub repository, "owner/name". */
  repo: z.string().optional(),
  /** Local folder: a name under projectsRoot, or an absolute path. */
  dir: z.string().optional(),
  /** Other folders whose agent sessions count for this project (names under projectsRoot or absolute). */
  extraDirs: z.array(z.string()).default([]),
  site: z.string().optional(),
  probes: z.array(z.object({ label: z.string(), url: z.string() })).default([]),
  /** Screenshot shown on the overview card, relative to projectsRoot. */
  shot: z.string().optional(),
  railway: z.object({ projectId: z.string(), serviceId: z.string(), environmentId: z.string() }).optional(),
  links: z.array(Link).default([]),
  /** Regular expression matched against Obsidian note paths. Defaults to the id and the name. */
  notes: z.string().optional(),
  /** Where to read the version: a file of the repo, and optionally a regex whose first group is the version. */
  version: z.object({ file: z.string(), pattern: z.string().optional(), json: z.string().optional() }).optional(),
  /** App Store app id (numbers only), for ratings, version and written reviews. */
  appStore: z.object({ id: z.string(), countries: z.array(z.string()).optional() }).optional(),
  /** RevenueCat project id (REVENUECAT_API_KEY must be set). */
  revenuecat: z.object({ projectId: z.string() }).optional(),
  /** Count GitHub release downloads (archives only, demos excluded). */
  releases: z.boolean().default(false),
  /** Short chips and a sentence shown on the project page. */
  highlights: z.array(z.string()).default([]),
  about: z.string().optional(),
  /** Title of the traffic panel (Railway HTTP logs). */
  trafficTitle: z.string().optional(),
  /** Show every project's screenshot on this project's page (a portfolio, typically). */
  showcase: z.boolean().default(false),
  identity: Identity.optional(),
});

const Subscription = z.object({
  name: z.string(),
  vendor: z.string().default(""),
  category: z.enum(["ia", "infra", "domains", "tools", "music", "personal"]).default("tools"),
  amount: z.number().nullable().default(null),
  currency: z.string().default("USD"),
  period: z.enum(["month", "year", "week", "usage"]).default("month"),
  last_charge: z.string().nullable().default(null),
  next_renewal: z.string().nullable().default(null),
  status: z.enum(["active", "failing", "cancelled", "unknown"]).default("active"),
  project: z.string().nullable().default(null),
  evidence: z.string().default(""),
  manage_url: z.string().optional(),
});

export const ConfigSchema = z.object({
  $schema: z.string().optional(),
  /** BCP 47 locale: "fr-CH", "fr-FR", "en-US", "en-GB"… French or English interface. */
  locale: z.string().default("en-US"),
  /** IANA time zone. Defaults to this computer's. */
  timezone: z.string().optional(),
  /** Currency every total is converted to. */
  currency: z.string().default("USD"),
  owner: z
    .object({
      name: z.string().default(""),
      /** How the greeting calls you. Defaults to the first word of name. */
      firstName: z.string().optional(),
      emails: z.array(Email).default([]),
      socials: z.array(Social).default([]),
      accounts: z.array(BrandLink).default([]),
      /** GitHub login whose Sponsors income is counted. */
      sponsors: z.string().optional(),
    })
    .default({ name: "", emails: [], socials: [], accounts: [] }),
  location: z
    .object({
      name: z.string(),
      latitude: z.number(),
      longitude: z.number(),
      /** ISO country code for public holidays (CH, FR, US…). */
      country: z.string().optional(),
      /** ISO subdivision for regional holidays (CH-GE, US-CA…). */
      region: z.string().optional(),
    })
    .optional(),
  /** Folder holding all your repositories. Defaults to the parent of zenith's folder. */
  projectsRoot: z.string().optional(),
  projects: z.array(ProjectInput).default([]),
  subscriptions: z.array(Subscription).default([]),
  /** RSS feeds for the local news panel. */
  news: z.array(z.object({ name: z.string(), url: z.string(), limit: z.number().default(8) })).default([]),
  /** Words that mean you or your projects, searched on Hacker News and GitHub. */
  watch: z.array(z.object({ term: z.string(), label: z.string() })).default([]),
  /** Public transport departures (Switzerland only, transport.opendata.ch). */
  transit: z.object({ stop: z.string() }).optional(),
  /** Swiss rivers and lakes (FOEN hydrology via existenz.ch). */
  water: z
    .object({ stations: z.array(z.object({ id: z.string(), name: z.string(), water: z.string(), level: z.boolean().default(false) })) })
    .optional(),
  /** Obsidian vault: `vault`, else the one open in Obsidian, else `fallback`. zenith writes its notes in `exportDir`. */
  obsidian: z
    .object({ vault: z.string().optional(), fallback: z.string().optional(), exportDir: z.string().default("zenith") })
    .default({ exportDir: "zenith" }),
  /** Native Mac app. Changing the bundle id resets macOS permissions. */
  mac: z.object({ bundleId: z.string().default("dev.zenith.app") }).default({ bundleId: "dev.zenith.app" }),
  /** zenith code, the coding workspace. */
  code: z
    .object({ enabled: z.boolean().default(true), port: z.number().default(4749), home: z.string().optional() })
    .default({ enabled: true, port: 4749 }),
});

export type Config = z.infer<typeof ConfigSchema>;
export type ProjectConfig = z.infer<typeof ProjectInput>;
export type IdentityConfig = z.infer<typeof Identity>;
export type SubscriptionConfig = z.infer<typeof Subscription>;
export type BrandName = z.infer<typeof Brand>;
export type NetworkName = z.infer<typeof Network>;

