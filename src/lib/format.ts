import { isFr, l10n } from "./i18n";

const loc = () => l10n().locale;

export const nf = (n: number, digits = 0) =>
  new Intl.NumberFormat(loc(), { maximumFractionDigits: digits, minimumFractionDigits: digits }).format(n);

export const compact = (n: number) => new Intl.NumberFormat(loc(), { notation: "compact", maximumFractionDigits: 1 }).format(n);

/** An amount in any currency. */
export const money = (n: number, currency: string, digits = 2) =>
  new Intl.NumberFormat(loc(), { style: "currency", currency, maximumFractionDigits: digits, minimumFractionDigits: digits }).format(n);

/** An amount in your currency (config `currency`). */
export const base = (n: number, digits = 2) => money(n, l10n().currency, digits);

export const usd = (n: number, digits = 2) => money(n, "USD", digits);

export const pct = (n: number, digits = 0) => (isFr() ? `${nf(n * 100, digits)} %` : `${nf(n * 100, digits)}%`);

export function ago(input: string | number | Date | null | undefined): string {
  if (input == null) return "—";
  const t = typeof input === "number" ? (input < 1e12 ? input * 1000 : input) : new Date(input).getTime();
  if (!Number.isFinite(t)) return "—";
  const fr = isFr();
  const s = Math.round((Date.now() - t) / 1000);
  if (s < 0) return fr ? "à venir" : "upcoming";
  if (s < 60) return fr ? "à l'instant" : "just now";
  const m = Math.round(s / 60);
  if (m < 60) return fr ? `il y a ${m} min` : `${m} min ago`;
  const h = Math.round(m / 60);
  if (h < 24) return fr ? `il y a ${h} h` : `${h} h ago`;
  const d = Math.round(h / 24);
  if (d < 30) return fr ? `il y a ${d} j` : `${d} d ago`;
  const mo = Math.round(d / 30);
  if (mo < 12) return fr ? `il y a ${mo} mois` : `${mo} mo ago`;
  const y = Math.round(mo / 12);
  return fr ? `il y a ${y} an${mo >= 24 ? "s" : ""}` : `${y} yr ago`;
}

export const date = (input: string | number | Date, opts: Intl.DateTimeFormatOptions = { day: "numeric", month: "short" }) =>
  new Intl.DateTimeFormat(loc(), { timeZone: l10n().timeZone, ...opts }).format(new Date(input));

export const dateTime = (input: string | number | Date) => date(input, { day: "numeric", month: "short", hour: "2-digit", minute: "2-digit" });

/** Today as YYYY-MM-DD in your time zone. */
export const today = () => new Intl.DateTimeFormat("en-CA", { timeZone: l10n().timeZone }).format(new Date());

/** Monday (UTC) of the week of an instant, in ms. */
export function weekStart(t: number) {
  const d = new Date(t);
  d.setUTCHours(0, 0, 0, 0);
  d.setUTCDate(d.getUTCDate() - ((d.getUTCDay() + 6) % 7));
  return d.getTime();
}

/** Counts instants per week over the last `weeks` weeks. */
export function weekly(times: number[], weeks = 12) {
  const now = weekStart(Date.now());
  const buckets = Array.from({ length: weeks }, (_, i) => ({ t: now - (weeks - 1 - i) * 7 * 864e5, v: 0 }));
  for (const t of times) {
    const i = Math.round((weekStart(t) - buckets[0].t) / (7 * 864e5));
    if (i >= 0 && i < weeks) buckets[i].v++;
  }
  return buckets.map((b) => ({ label: date(b.t), value: b.v, t: b.t }));
}
