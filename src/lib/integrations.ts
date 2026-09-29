import "server-only";
import type { Extension, Event, Kpi, MoneyRow, CardStat } from "./extensions";
import { config } from "./config";
import { PROJECTS, type Project } from "./projects";
import { source } from "./source";
import { tr } from "./i18n";
import { ago, base, date, nf } from "./format";
import * as rc from "./sources/revenuecat";
import { releases, sponsors, type Release } from "./sources/github";
import { appReviews, appStore } from "./sources/domains";
import { projectVersion } from "./sources/git";

/**
 * Built-in integrations, switched on per project in zenith.config.json:
 * `revenuecat`, `appStore`, `releases`, and `owner.sponsors`.
 */

const val = <T,>(s: { ok: true; data: T } | { ok: false }) => (s.ok ? s.data : null);

/** Downloads of real archives, demos excluded. */
export const downloads = (list: Release[]) =>
  list.reduce((a, r) => a + r.assets.filter((x) => /\.(zip|tar\.gz|dmg|exe|msi|appimage|deb|rpm|pkg)$/i.test(x.name) && !/demo/i.test(x.name)).reduce((s, x) => s + x.download_count, 0), 0);

const withRc = () => PROJECTS.filter((p) => p.revenuecat);
const withReleases = () => PROJECTS.filter((p) => p.releases && p.repo);
const withAppStore = () => PROJECTS.filter((p) => p.appStore);

const revenuecat: Extension = {
  id: "revenuecat",
  kpis: async () =>
    Promise.all(
      withRc().map(async (p): Promise<Kpi> => {
        const m = await source(() => rc.overview(p.revenuecat!.projectId));
        return {
          label: tr(`Revenu 28 j · ${p.name}`, `Revenue 28 d · ${p.name}`),
          project: p.id,
          value: m.ok ? (m.data.revenue?.value ?? 0) : null,
          format: { style: "currency", currency: config().currency },
          hint: m.ok ? `MRR ${base(m.data.mrr?.value ?? 0)}` : tr("RevenueCat à brancher", "Connect RevenueCat"),
        };
      }),
    ),
  card: async (p) => {
    if (!p.revenuecat) return null;
    const m = val(await source(() => rc.overview(p.revenuecat!.projectId)));
    return [
      { label: "MRR", value: m ? base(m.mrr?.value ?? 0, 0) : "—" },
      { label: tr("abonnés", "subscribers"), value: m ? nf(m.active_subscriptions?.value ?? 0) : "—" },
    ];
  },
  money: async () =>
    Promise.all(
      withRc().map(async (p): Promise<MoneyRow> => {
        const m = await source(() => rc.overview(p.revenuecat!.projectId));
        return {
          label: tr(`${p.name} · revenu 28 j`, `${p.name} · revenue 28 d`),
          value: m.ok ? (m.data.revenue?.value ?? 0) : null,
          currency: config().currency,
          sign: 1,
          hint: m.ok ? `MRR ${base(m.data.mrr?.value ?? 0)}` : tr("RevenueCat à brancher", "Connect RevenueCat"),
          project: p.id,
        };
      }),
    ),
  facts: async (p) => {
    if (!p.revenuecat) return [];
    const id = p.revenuecat.projectId;
    const [m, c] = await Promise.all([source(() => rc.overview(id)), source(() => rc.customers(id))]);
    const out: string[] = [];
    const mm = val(m);
    if (mm)
      out.push(
        tr(
          `MRR ${base(mm.mrr?.value ?? 0)}, revenu 28 j ${base(mm.revenue?.value ?? 0)}, ${mm.active_subscriptions?.value ?? 0} abonnement(s) actif(s)`,
          `MRR ${base(mm.mrr?.value ?? 0)}, revenue 28 d ${base(mm.revenue?.value ?? 0)}, ${mm.active_subscriptions?.value ?? 0} active subscription(s)`,
        ),
      );
    const cc = val(c);
    if (cc)
      out.push(
        tr(
          `${cc.length} comptes vus par RevenueCat, ${cc.filter((x) => x.last_seen_at > Date.now() - 7 * 864e5).length} actifs sur 7 j`,
          `${cc.length} accounts seen by RevenueCat, ${cc.filter((x) => x.last_seen_at > Date.now() - 7 * 864e5).length} active over 7 d`,
        ),
      );
    return out;
  },
  keys: ["REVENUECAT_API_KEY"],
};

const githubReleases: Extension = {
  id: "releases",
  kpis: async () =>
    Promise.all(
      withReleases().map(async (p): Promise<Kpi> => {
        const r = await source(() => releases(p.repo!));
        return {
          label: tr(`Téléch. ${p.name}`, `${p.name} downloads`),
          project: p.id,
          value: r.ok ? downloads(r.data) : null,
          hint: r.ok ? tr(`${r.data[0]?.tag_name ?? ""} en ligne`, `${r.data[0]?.tag_name ?? ""} live`) : undefined,
        };
      }),
    ),
  card: async (p) => {
    if (!p.releases || !p.repo) return null;
    const r = val(await source(() => releases(p.repo!)));
    return [
      { label: "version", value: (await projectVersion(p.id)) ?? r?.[0]?.tag_name ?? "—" },
      { label: tr("téléchargements", "downloads"), value: r ? nf(downloads(r)) : "—" },
    ];
  },
  events: async () => {
    const out: Event[] = [];
    for (const p of withReleases()) {
      const r = val(await source(() => releases(p.repo!)));
      for (const x of r?.slice(0, 5) ?? [])
        out.push({ project: p.id, at: new Date(x.published_at).getTime(), kind: "release", text: tr(`${x.name || x.tag_name} publiée`, `${x.name || x.tag_name} released`), href: x.html_url });
    }
    return out;
  },
  facts: async (p) => {
    if (!p.releases || !p.repo) return [];
    const r = val(await source(() => releases(p.repo!)));
    if (!r?.length) return [];
    return [
      tr(
        `Dernière release ${r[0].tag_name} (${date(r[0].published_at, { day: "numeric", month: "short", year: "numeric" })}), ${nf(downloads(r))} téléchargements au total`,
        `Latest release ${r[0].tag_name} (${date(r[0].published_at, { day: "numeric", month: "short", year: "numeric" })}), ${nf(downloads(r))} downloads in total`,
      ),
    ];
  },
};

const appStoreExt: Extension = {
  id: "appstore",
  events: async () => {
    const out: Event[] = [];
    for (const p of withAppStore()) {
      const rv = val(await source(() => appReviews(p.appStore!.id, p.appStore!.countries)));
      for (const r of rv?.slice(0, 5) ?? []) out.push({ project: p.id, at: new Date(r.at).getTime(), kind: "review", text: `${"★".repeat(r.rating)} « ${r.title} » (${r.country})` });
    }
    return out;
  },
  facts: async (p) => {
    if (!p.appStore) return [];
    const [st, rv] = await Promise.all([source(() => appStore(p.appStore!.id, p.appStore!.countries?.[0])), source(() => appReviews(p.appStore!.id, p.appStore!.countries))]);
    const out: string[] = [];
    const s = val(st);
    if (s)
      out.push(
        tr(
          `App Store : v${s.version} en ligne depuis le ${date(s.updated, { day: "numeric", month: "short", year: "numeric" })}, note ${s.rating?.toFixed(1) ?? "—"} (${s.ratings} avis)`,
          `App Store: v${s.version} live since ${date(s.updated, { day: "numeric", month: "short", year: "numeric" })}, rated ${s.rating?.toFixed(1) ?? "—"} (${s.ratings} ratings)`,
        ),
      );
    const r = val(rv);
    if (r?.length) out.push(tr("Derniers avis écrits : ", "Latest written reviews: ") + r.slice(0, 3).map((x) => `${x.rating}★ « ${x.title} » (${x.country}, ${ago(x.at)})`).join(" ; "));
    return out;
  },
};

const sponsorsExt: Extension = {
  id: "sponsors",
  money: async () => {
    if (!config().owner.sponsors) return [];
    const sp = await source(sponsors);
    return [
      {
        label: tr("GitHub Sponsors · par mois", "GitHub Sponsors · per month"),
        value: sp.ok ? sp.data.monthlyUSD : null,
        currency: "USD",
        sign: 1,
        hint: sp.ok ? (sp.data.listed ? tr(`${sp.data.count} sponsor(s)`, `${sp.data.count} sponsor(s)`) : tr("Profil Sponsors pas encore activé", "Sponsors profile not enabled yet")) : tr("GitHub à brancher", "Connect GitHub"),
      },
    ];
  },
};

export const BUILTIN: Extension[] = [revenuecat, githubReleases, appStoreExt, sponsorsExt];

/** The card numbers of a project: the first extension that answers (yours before built-in ones), else `fallback`. */
export async function cardStats(p: Project, fallback: () => Promise<CardStat[]>, extensions: Extension[]) {
  const ordered = [...extensions.filter((e) => !BUILTIN.includes(e)), ...extensions.filter((e) => BUILTIN.includes(e))];
  for (const e of ordered) {
    if (!e.card) continue;
    const stats = await e.card(p).catch(() => null);
    if (stats?.length) return stats;
  }
  return fallback();
}

