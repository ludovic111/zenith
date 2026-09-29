import "server-only";
import { config } from "../config";
import { isFr, l10n, tr } from "../i18n";
import { today } from "../format";
import { cached, getJson, MissingConfig } from "../source";
import { place } from "./weather";

export type Air = {
  aqi: number;
  pm25: number;
  pm10: number;
  ozone: number;
  uv: number;
  uvMax: number;
  pollen: { label: string; value: number }[];
};

const POLLEN = (): [string, string][] => [
  ["alder_pollen", tr("Aulne", "Alder")],
  ["birch_pollen", tr("Bouleau", "Birch")],
  ["grass_pollen", tr("Graminées", "Grass")],
  ["mugwort_pollen", tr("Armoise", "Mugwort")],
  ["olive_pollen", tr("Olivier", "Olive")],
  ["ragweed_pollen", tr("Ambroisie", "Ragweed")],
];

/**
 * Air quality, UV and pollen at `location` (Open-Meteo, no key). Pollen comes from the
 * European CAMS model: outside Europe the values are simply missing.
 */
export const air = async () => {
  const loc = place();
  const tz = l10n().timeZone;
  return cached(`air:${loc.latitude},${loc.longitude}`, 1800, async (): Promise<Air> => {
    const pollen = POLLEN();
    const d = await getJson<{ current: Record<string, number | null>; hourly: { time: string[]; uv_index: (number | null)[] } }>(
      `https://air-quality-api.open-meteo.com/v1/air-quality?latitude=${loc.latitude}&longitude=${loc.longitude}&current=european_aqi,pm2_5,pm10,ozone,uv_index,${pollen.map(([k]) => k).join(",")}&hourly=uv_index&forecast_days=1&timezone=${encodeURIComponent(tz)}`,
    );
    const c = d.current;
    return {
      aqi: c.european_aqi ?? 0,
      pm25: c.pm2_5 ?? 0,
      pm10: c.pm10 ?? 0,
      ozone: c.ozone ?? 0,
      uv: c.uv_index ?? 0,
      uvMax: Math.max(0, ...d.hourly.uv_index.filter((v): v is number => v != null)),
      pollen: pollen.map(([k, label]) => ({ label, value: c[k] ?? 0 })).filter((p) => p.value >= 1),
    };
  });
};

/** European air quality index → label and color. */
export function aqiLabel(aqi: number) {
  if (aqi <= 20) return { label: tr("Très bon", "Very good"), color: "#34d399" };
  if (aqi <= 40) return { label: tr("Bon", "Good"), color: "#a3e635" };
  if (aqi <= 60) return { label: tr("Moyen", "Moderate"), color: "#FFD166" };
  if (aqi <= 80) return { label: tr("Médiocre", "Poor"), color: "#fb923c" };
  if (aqi <= 100) return { label: tr("Mauvais", "Very poor"), color: "#fb5a6b" };
  return { label: tr("Très mauvais", "Extremely poor"), color: "#c026d3" };
}

/** Pollen in grains/m³ → level (simplified MeteoSwiss thresholds). */
export const pollenLevel = (v: number) => (v >= 70 ? tr("fort", "high") : v >= 20 ? tr("moyen", "moderate") : tr("faible", "low"));

export type Water = { station: string; water: string; temp: number | null; flow: number | null; level: boolean; at: string };

/**
 * Temperature and flow of rivers (or level of lakes) from `water.stations` in the config.
 * Swiss stations only: FOEN hydrology data, served by existenz.ch without a key.
 */
export const water = async () => {
  const stations = config().water?.stations ?? [];
  if (!stations.length) throw new MissingConfig(["water"]);
  return cached(`water:${stations.map((s) => s.id).join(",")}`, 1800, async (): Promise<Water[]> => {
    const d = await getJson<{ payload: { timestamp: number; loc: string; par: string; val: number }[] }>(
      `https://api.existenz.ch/apiv1/hydro/latest?locations=${stations.map((s) => encodeURIComponent(s.id)).join(",")}&parameters=temperature,flow,height&app=zenith`,
    );
    return stations
      .map((s) => {
        const rows = d.payload.filter((p) => p.loc === s.id);
        const get = (par: string) => rows.find((r) => r.par === par)?.val ?? null;
        return { station: s.name, water: s.water, temp: get("temperature"), flow: s.level ? get("height") : get("flow"), level: s.level, at: new Date((rows[0]?.timestamp ?? 0) * 1000).toISOString() };
      })
      .filter((w) => w.temp != null || w.flow != null);
  });
};

export type Holiday = { date: string; name: string };

/** French names of common holidays, keyed by Nager.Date's English name. */
const FR: Record<string, string> = {
  "New Year's Day": "Nouvel An",
  "New Year's Eve": "Saint-Sylvestre",
  "Good Friday": "Vendredi saint",
  "Easter Sunday": "Pâques",
  "Easter Monday": "Lundi de Pâques",
  "Ascension Day": "Ascension",
  "Whit Sunday": "Pentecôte",
  "Whit Monday": "Lundi de Pentecôte",
  "Assumption Day": "Assomption",
  "All Saints' Day": "Toussaint",
  "Armistice Day": "Armistice",
  "Swiss National Day": "Fête nationale",
  "Christmas Eve": "Veille de Noël",
  "Christmas Day": "Noël",
  "St. Stephen's Day": "Saint-Étienne",
};

/**
 * Public holidays of `location.country` (and `location.region`, e.g. "US-CA"), this year
 * and next (Nager.Date, no key).
 */
export const holidays = async () => {
  const country = config().location?.country?.toUpperCase();
  if (!country) throw new MissingConfig(["location.country"]);
  const region = config().location?.region?.toUpperCase();
  const fr = isFr();
  return cached(`holidays:${country}:${region ?? ""}:${fr ? "fr" : "en"}`, 86400, async (): Promise<Holiday[]> => {
    const year = new Date().getFullYear();
    const lists = await Promise.all(
      [year, year + 1].map((y) =>
        getJson<{ date: string; localName: string; name: string; global: boolean; counties: string[] | null }[]>(`https://date.nager.at/api/v3/PublicHolidays/${y}/${country}`),
      ),
    );
    // In French: our translation, else the local name (French already in French-speaking regions).
    // In English: Nager's English name.
    return lists
      .flat()
      .filter((h) => h.global || (!!region && h.counties?.includes(region)))
      .map((h) => ({ date: h.date, name: fr ? (FR[h.name] ?? h.localName) : h.name }))
      // Nager lists some holidays once per group of regions.
      .filter((h, i, all) => all.findIndex((x) => x.date === h.date && x.name === h.name) === i);
  });
};

/** Next public holidays. */
export const nextHolidays = async (n = 4) => {
  const t = today();
  return (await holidays()).filter((h) => h.date >= t).slice(0, n);
};
