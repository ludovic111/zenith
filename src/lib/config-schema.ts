import { z } from "zod";
import { AVATAR_ACCESSORIES, AVATAR_SHAPES } from "./agent/avatar.ts";

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

const Slug = z.string().regex(/^[a-z0-9][a-z0-9-]*$/, "lowercase letters, digits and dashes");

/** What the Now list can hold; a routine with `on` hands new ones to an agent. */
export const NOW_KINDS = ["down", "payment", "birthday", "sale", "reply", "civic", "ci", "refresh"] as const;

const Routine = z
  .object({
    id: Slug,
    name: z.string().optional(),
    /** Local time, "HH:MM" (24 h). */
    at: z.string().regex(/^([01]\d|2[0-3]):[0-5]\d$/, "HH:MM, 24-hour").optional(),
    /** ISO weekdays it runs on (1 = Monday … 7 = Sunday). Default: every day. */
    days: z.array(z.number().int().min(1).max(7)).default([1, 2, 3, 4, 5, 6, 7]),
    /** Instead of a time: each new Now item of these kinds is handed to the agent, as it appears. */
    on: z.array(z.enum(NOW_KINDS)).min(1).optional(),
    /** A built-in task: "refresh-life" captures Gmail and Google Calendar into My life. */
    task: z.enum(["refresh-life"]).optional(),
    /** A skill of the agent's folder to follow (skills/<id>/SKILL.md). */
    skill: z.string().optional(),
    /** What to ask, in your words (or added to the task, the skill or the Now item). */
    prompt: z.string().optional(),
    /** A bot id (agent.bots) to run it. */
    bot: z.string().optional(),
    /** A project id to run it in; default: the agent's own folder. */
    project: z.string().optional(),
    enabled: z.boolean().default(true),
  })
  .refine((r) => r.at || r.on, { message: "a routine needs a time (at) or Now kinds (on)" })
  .refine((r) => r.on || r.task || r.prompt || r.skill, { message: "a routine needs a task, a skill or a prompt" });

const Shape = z.enum(AVATAR_SHAPES);
const Accessory = z.enum(AVATAR_ACCESSORIES);
const Hex = z.string().regex(/^#[0-9a-fA-F]{6}$/, "a color like #8B5CF6");

/** A named agent of your team: its own folder, memory and personality, on Claude or Codex. */
const Bot = z.object({
  id: Slug,
  /** Its first name ("Margot"). `@name` calls it too. */
  name: z.string(),
  /** Its job, shown next to its name ("Mail"). */
  title: z.string().optional(),
  /** Its avatar: a shape, a color and an accessory. Default: drawn from its id. */
  shape: Shape.optional(),
  color: Hex.optional(),
  accessory: Accessory.optional(),
  emoji: z.string().optional(),
  /** What it is for, in a sentence or two. Seeds its SOUL.md, which you can then edit. */
  role: z.string(),
  /** Claude Code (your Claude subscription) or Codex (your ChatGPT subscription). Default: agent.provider. */
  provider: z.enum(["claude", "codex"]).optional(),
  /** Model id; default: as for the main agent. */
  model: z.string().optional(),
  enabled: z.boolean().default(true),
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
  /** The zenith agent: "Ask zenith", the Now list, delegation to agents and routines. Runs through zenith code. */
  agent: z
    .object({
      enabled: z.boolean().default(true),
      /** The agent's own folder, where your life conversations live. */
      home: z.string().default("~/.zenith/life"),
      /** Your agent's name: how it calls itself, and how the interface names it. */
      name: z.string().default("zenith"),
      /** Its avatar: shape, color and accessory. */
      shape: Shape.optional(),
      color: Hex.optional(),
      accessory: Accessory.optional(),
      /** Who answers by default: Claude Code or Codex. */
      provider: z.enum(["claude", "codex"]).default("claude"),
      /** Model id, e.g. "claude-opus-5-5". Default: zenith code's default, else your latest thread's. */
      model: z.string().optional(),
      /** Agents that run on their own at a given time, once a day. */
      routines: z
        .array(Routine)
        .refine((list) => new Set(list.map((r) => r.id)).size === list.length, { message: "routine ids must be unique" })
        .default([]),
      /** Your team: named agents with a role, each on Claude or Codex, that your agent hands work to. */
      bots: z
        .array(Bot)
        .refine((list) => new Set(list.map((b) => b.id)).size === list.length, { message: "bot ids must be unique" })
        .default([]),
      /**
       * MCP servers the whole team can use (Claude Code and Codex alike), by name: a local
       * program `{ "command", "args", "env" }` or a remote one `{ "url", "headers" }`.
       * Anything with an MCP server becomes something your agents can act on.
       */
      mcp: z
        .record(
          z.string().regex(/^[\w-]+$/, "letters, digits, dashes"),
          z.union([
            z.object({ command: z.string(), args: z.array(z.string()).default([]), env: z.record(z.string(), z.string()).default({}) }),
            z.object({ url: z.string().url(), headers: z.record(z.string(), z.string()).default({}) }),
          ]),
        )
        .default({}),
      /** Talk to your agent from elsewhere. Telegram: the bot token is TELEGRAM_BOT_TOKEN (.env.local). */
      gateway: z
        .object({
          telegram: z
            .object({
              /** Chat ids allowed to talk to it (send /start to your bot to learn yours). */
              chats: z.array(z.union([z.number().int(), z.string().regex(/^-?\d+$/)])).default([]),
              /** Where messages go: "life" (default), a bot id or a project id. */
              target: z.string().optional(),
            })
            .optional(),
        })
        .default({}),
    })
    .default({ enabled: true, home: "~/.zenith/life", name: "zenith", provider: "claude", routines: [], bots: [], mcp: {}, gateway: {} }),
});

export type Config = z.infer<typeof ConfigSchema>;
export type ProjectConfig = z.infer<typeof ProjectInput>;
export type IdentityConfig = z.infer<typeof Identity>;
export type SubscriptionConfig = z.infer<typeof Subscription>;
export type BrandName = z.infer<typeof Brand>;
export type NetworkName = z.infer<typeof Network>;

export type RoutineConfig = z.infer<typeof Routine>;
export type BotConfig = z.infer<typeof Bot>;
