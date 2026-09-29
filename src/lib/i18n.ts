/**
 * French or English, chosen by `locale` in zenith.config.json.
 *
 * Strings stay inline, side by side: `tr("Vue d'ensemble", "Overview")`. The server sets
 * the locale when it reads the config; the browser gets it from <L10nProvider> in the
 * root layout, before any component renders. One person per zenith, so one locale.
 */

export type L10n = { locale: string; timeZone: string; currency: string };

const g = globalThis as { __zenithL10n?: L10n };

export const l10n = (): L10n => g.__zenithL10n ?? { locale: "en-US", timeZone: "UTC", currency: "USD" };

export function setL10n(l: L10n) {
  g.__zenithL10n = l;
}

export const isFr = () => l10n().locale.toLowerCase().startsWith("fr");

/** The French or the English string. */
export const tr = (fr: string, en: string) => (isFr() ? fr : en);

/** Plural helper: `plural(n, ["commit", "commits"], ["commit", "commits"])`. French treats 0 and 1 as singular. */
export function plural(n: number, fr: [string, string], en: [string, string]) {
  return isFr() ? (Math.abs(n) < 2 ? fr[0] : fr[1]) : n === 1 ? en[0] : en[1];
}
