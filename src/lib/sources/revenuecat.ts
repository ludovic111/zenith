import "server-only";
import { l10n } from "../i18n";
import { cached, getJson, need } from "../source";

const BASE = "https://api.revenuecat.com/v2";

/** `id` is the project id from the dashboard URL (1a2b3c4d) or the API one (proj1a2b3c4d). */
function config(id: string) {
  const [key] = need("REVENUECAT_API_KEY");
  return { project: id.startsWith("proj") ? id : `proj${id}`, headers: { Authorization: `Bearer ${key}` }, currency: l10n().currency };
}

export type Metric = { id: string; name: string; description: string; unit: string; period: string; value: number };

export const overview = (id: string) =>
  cached(`rc:overview:${id}`, 300, async () => {
    const { project, headers, currency } = config(id);
    const res = await getJson<{ metrics: Metric[] }>(`${BASE}/projects/${project}/metrics/overview?currency=${currency}`, { headers });
    return Object.fromEntries(res.metrics.map((m) => [m.id, m])) as Record<string, Metric>;
  });

type ChartValue = { cohort: number; measure: number; value: number; incomplete?: boolean };

/** Weekly series of a RevenueCat chart (measure 0: revenue, MRR…). */
export const chart = (id: string, name: "revenue" | "mrr" | "actives" | "customers_new", weeks = 16) =>
  cached(`rc:chart:${id}:${name}`, 900, async () => {
    const { project, headers, currency } = config(id);
    const start = new Date(Date.now() - weeks * 7 * 864e5).toISOString().slice(0, 10);
    const res = await getJson<{ values: ChartValue[] }>(
      `${BASE}/projects/${project}/charts/${name}?currency=${currency}&resolution=1&start_date=${start}`,
      { headers },
    );
    return res.values
      .filter((v) => v.measure === 0)
      .map((v) => ({ t: v.cohort * 1000, value: v.value, incomplete: !!v.incomplete }));
  });

export type Customer = {
  id: string;
  first_seen_at: number;
  last_seen_at: number;
  last_seen_country: string | null;
  last_seen_platform: string | null;
  last_seen_app_version: string | null;
};

/** Every RevenueCat customer, 100 per page. */
export const customers = (id: string) =>
  cached(`rc:customers:${id}`, 600, async () => {
    const { project, headers } = config(id);
    const out: Customer[] = [];
    let after: string | null = null;
    for (let page = 0; page < 20; page++) {
      const res: { items: Customer[]; next_page: string | null } = await getJson(
        `${BASE}/projects/${project}/customers?limit=100${after ? `&starting_after=${encodeURIComponent(after)}` : ""}`,
        { headers },
      );
      out.push(...res.items);
      if (!res.next_page || !res.items.length) break;
      after = res.items.at(-1)!.id;
    }
    return out;
  });
