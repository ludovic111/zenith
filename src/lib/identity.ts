import "server-only";
import { config, type BrandName, type IdentityConfig, type NetworkName } from "./config";
import type { ProjectId } from "./projects";
import { liveObject } from "./live";

/**
 * Each project's identity card: what no API can guess (names, handles, ids, services).
 * Filled in zenith.config.json, under `owner` and each project's `identity`.
 */

export type Network = NetworkName;
export type Brand = BrandName;

export type Identity = IdentityConfig;
export type Social = Identity["socials"][number];
export type Email = Identity["emails"][number];
export type Link = Identity["services"][number];
export type Field = Identity["ids"][number];

const EMPTY: Identity = { names: [], domains: [], emails: [], socials: [], stores: [], ids: [], services: [], todo: [] };

export const OWNER = liveObject(() => {
  const c = config();
  return {
    name: c.owner.name,
    firstName: c.owner.firstName ?? c.owner.name.split(/\s+/)[0] ?? "",
    place: c.location?.name ?? "",
    emails: c.owner.emails,
    socials: c.owner.socials,
    accounts: c.owner.accounts,
  };
});

export const IDENTITY: Record<ProjectId, Identity> = liveObject(() => Object.fromEntries(config().projects.map((p) => [p.id, p.identity ?? EMPTY])));

export const identityOf = (id: ProjectId): Identity => IDENTITY[id] ?? EMPTY;
