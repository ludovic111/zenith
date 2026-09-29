import "server-only";
import { resolveMx } from "node:dns/promises";
import tls from "node:tls";
import { tr } from "../i18n";
import { cached, getJson } from "../source";

export type DomainInfo = {
  domain: string;
  registrar: string | null;
  created: string | null;
  expires: string | null;
  mail: { provider: string; hosts: string[] } | null;
  tlsExpires: string | null;
  tlsIssuer: string | null;
  status: number | null;
};

type Rdap = {
  events?: { eventAction: string; eventDate: string }[];
  entities?: { roles?: string[]; vcardArray?: [string, [string, unknown, string, string][]] }[];
};

function mailProvider(hosts: string[]) {
  const h = hosts.join(" ").toLowerCase();
  if (h.includes("google")) return "Google Workspace";
  if (h.includes("porkbun")) return tr("Redirection Porkbun", "Porkbun forwarding");
  if (h.includes("outlook") || h.includes("protection.outlook")) return "Microsoft 365";
  if (h.includes("icloud")) return "iCloud+";
  if (h.includes("zoho")) return "Zoho";
  if (h.includes("protonmail")) return "Proton";
  return hosts[0] ?? tr("inconnu", "unknown");
}

function certificate(domain: string): Promise<{ expires: string; issuer: string } | null> {
  return new Promise((resolve) => {
    const socket = tls.connect({ host: domain, port: 443, servername: domain, timeout: 8000 }, () => {
      const cert = socket.getPeerCertificate();
      socket.end();
      resolve(cert?.valid_to ? { expires: new Date(cert.valid_to).toISOString(), issuer: String(cert.issuer?.O ?? cert.issuer?.CN ?? "") } : null);
    });
    socket.on("error", () => resolve(null));
    socket.on("timeout", () => {
      socket.destroy();
      resolve(null);
    });
  });
}

/** Official RDAP server of each TLD, from IANA's bootstrap registry. */
const rdapBase = () =>
  cached("rdap:bootstrap", 86400, async () => {
    const boot = await getJson<{ services: [string[], string[]][] }>("https://data.iana.org/rdap/dns.json");
    const map = new Map<string, string>();
    for (const [tlds, urls] of boot.services) for (const t of tlds) map.set(t, urls.find((u) => u.startsWith("https")) ?? urls[0]);
    return map;
  });

async function rdap(domain: string) {
  const base = (await rdapBase()).get(domain.split(".").pop()!);
  if (!base) return null;
  return getJson<Rdap>(`${base.replace(/\/?$/, "/")}domain/${domain}`, { timeout: 12000, headers: { Accept: "application/rdap+json" } });
}

/** A domain's registry data (RDAP), email (MX), certificate and HTTP answer. */
export const domainInfo = (domain: string) =>
  cached(`domain:${domain}`, 6 * 3600, async (): Promise<DomainInfo> => {
    const [registry, mx, cert, res] = await Promise.all([
      rdap(domain).catch(() => null),
      resolveMx(domain).catch(() => []),
      certificate(domain),
      fetch(`https://${domain}`, { redirect: "manual", cache: "no-store", signal: AbortSignal.timeout(8000) }).catch(() => null),
    ]);
    await res?.body?.cancel();
    const events = Object.fromEntries((registry?.events ?? []).map((e) => [e.eventAction, e.eventDate]));
    const registrar = registry?.entities?.find((e) => e.roles?.includes("registrar"))?.vcardArray?.[1].find((v) => v[0] === "fn")?.[3] ?? null;
    const hosts = mx.sort((a, b) => a.priority - b.priority).map((m) => m.exchange);
    return {
      domain,
      registrar,
      created: events.registration ?? null,
      expires: events.expiration ?? null,
      mail: hosts.length ? { provider: mailProvider(hosts), hosts } : null,
      tlsExpires: cert?.expires ?? null,
      tlsIssuer: cert?.issuer ?? null,
      status: res?.status ?? null,
    };
  });

export type AppStoreInfo = { version: string; rating: number | null; ratings: number; released: string; updated: string; price: string; url: string };

/** Public App Store listing (iTunes API, no key). */
export const appStore = (id: string, country = "us") =>
  cached(`appstore:${id}`, 3600, async (): Promise<AppStoreInfo | null> => {
    const res = await getJson<{ results: Record<string, unknown>[] }>(`https://itunes.apple.com/lookup?id=${id}&country=${country}`);
    const r = res.results[0];
    if (!r) return null;
    return {
      version: String(r.version),
      rating: typeof r.averageUserRating === "number" ? r.averageUserRating : null,
      ratings: Number(r.userRatingCount ?? 0),
      released: String(r.releaseDate),
      updated: String(r.currentVersionReleaseDate),
      price: String(r.formattedPrice ?? ""),
      url: String(r.trackViewUrl),
    };
  });

export type Review = { country: string; author: string; rating: number; title: string; body: string; version: string; at: string };

const REVIEW_COUNTRIES = ["ch", "fr", "be", "ca", "us", "gb", "de", "it"];

/** Latest written App Store reviews across the watched countries (Apple's public RSS feed, no key). */
export const appReviews = (id: string, countries: string[] = REVIEW_COUNTRIES) =>
  cached(`appstore:reviews:${id}:${countries.join(",")}`, 3600, async (): Promise<Review[]> => {
    type Entry = Record<string, { label: string } & Record<string, unknown>> & { author?: { name: { label: string } } };
    const lists = await Promise.all(
      countries.map((c) => c.toLowerCase()).map(async (country) => {
        const d = await getJson<{ feed: { entry?: Entry | Entry[] } }>(`https://itunes.apple.com/${country}/rss/customerreviews/id=${id}/sortBy=mostRecent/json`).catch(() => null);
        const entries = d?.feed.entry ? ([] as Entry[]).concat(d.feed.entry) : [];
        return entries
          .filter((e) => e["im:rating"])
          .map((e) => ({
            country: country.toUpperCase(),
            author: e.author?.name.label ?? "",
            rating: Number(e["im:rating"].label),
            title: e.title?.label ?? "",
            body: e.content?.label ?? "",
            version: e["im:version"]?.label ?? "",
            at: e.updated?.label ? new Date(e.updated.label).toISOString() : "",
          }));
      }),
    );
    return lists.flat().sort((a, b) => b.at.localeCompare(a.at));
  });
