"use client";

/** The browser side of the welcome and the settings: save the config, scan a folder. */

export async function saveConfig(set: Record<string, unknown>): Promise<void> {
  const res = await fetch("/api/config", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ set }) });
  const data = (await res.json().catch(() => ({}))) as { error?: string };
  if (!res.ok) throw new Error(data.error ?? `HTTP ${res.status}`);
}

export type FoundProject = { id: string; name: string; dir: string; repo: string | null; site: string | null; tagline: string };

export async function scanFolder(root: string): Promise<{ root: string; projects: FoundProject[] }> {
  const res = await fetch(`/api/setup/scan?root=${encodeURIComponent(root)}`);
  const data = (await res.json().catch(() => ({}))) as { root?: string; projects?: FoundProject[]; error?: string };
  if (!res.ok) throw new Error(data.error ?? `HTTP ${res.status}`);
  return { root: data.root!, projects: data.projects ?? [] };
}

export type Place = { name: string; latitude: number; longitude: number; country?: string; region?: string; timezone?: string; label: string };

/** Cities matching a name (Open-Meteo's free geocoder). */
export async function findPlaces(name: string, language: string): Promise<Place[]> {
  if (name.trim().length < 2) return [];
  const res = await fetch(`https://geocoding-api.open-meteo.com/v1/search?count=6&language=${language}&name=${encodeURIComponent(name.trim())}`);
  const data = (await res.json().catch(() => ({}))) as {
    results?: { name: string; latitude: number; longitude: number; country_code?: string; country?: string; admin1?: string; admin1_code?: string; timezone?: string }[];
  };
  return (data.results ?? []).map((r) => ({
    name: r.name,
    latitude: Math.round(r.latitude * 1e4) / 1e4,
    longitude: Math.round(r.longitude * 1e4) / 1e4,
    country: r.country_code,
    timezone: r.timezone,
    label: [r.name, r.admin1, r.country].filter(Boolean).join(", "),
  }));
}

const EURO = ["FR", "DE", "ES", "IT", "BE", "NL", "PT", "AT", "IE", "FI", "LU", "GR", "SK", "SI", "EE", "LV", "LT", "MT", "CY", "HR", "MC"];
const CURRENCY: Record<string, string> = { CH: "CHF", LI: "CHF", GB: "GBP", US: "USD", CA: "CAD", AU: "AUD", NZ: "NZD", JP: "JPY", SE: "SEK", NO: "NOK", DK: "DKK", PL: "PLN", CZ: "CZK", HU: "HUF", BR: "BRL", MX: "MXN", IN: "INR", SG: "SGD", HK: "HKD" };

/** The currency of a country (ISO code), if zenith knows it. */
export const currencyOf = (country?: string | null) => (country ? (CURRENCY[country.toUpperCase()] ?? (EURO.includes(country.toUpperCase()) ? "EUR" : null)) : null);

/** A config id from a name: lowercase letters, digits and dashes. */
export const slug = (s: string) =>
  s
    .toLowerCase()
    .normalize("NFD")
    .replace(/[̀-ͯ]/g, "")
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 32);

/** `base`, or `base-2`… when taken. */
export function freeId(base: string, taken: Set<string>): string {
  const b = slug(base) || "agent";
  let id = b;
  for (let i = 2; taken.has(id); i++) id = `${b}-${i}`;
  return id;
}
