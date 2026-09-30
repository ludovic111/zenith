"use client";

import { useEffect, useState } from "react";
import { Check, LoaderCircle, MapPin } from "lucide-react";
import { tr } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import { currencyOf, findPlaces, type Place } from "./api";
import { Button, Field, Select, TextInput } from "./fields";

export type YouDraft = {
  name: string;
  locale: string;
  currency: string;
  timezone?: string;
  location?: { name: string; latitude: number; longitude: number; country?: string; region?: string };
};

export const LOCALES = [
  { id: "fr-CH", label: "Français (Suisse)" },
  { id: "fr-FR", label: "Français (France)" },
  { id: "fr-BE", label: "Français (Belgique)" },
  { id: "fr-CA", label: "Français (Canada)" },
  { id: "en-US", label: "English (US)" },
  { id: "en-GB", label: "English (UK)" },
];

export const CURRENCIES = ["CHF", "EUR", "USD", "GBP", "CAD", "AUD", "JPY", "SEK", "NOK", "DKK", "PLN"];

/** Your city, found by name: it gives zenith your weather, holidays and time zone. */
export function CityField({ value, onChange, language }: { value: YouDraft["location"]; onChange: (place: Place | null) => void; language: string }) {
  const [q, setQ] = useState(value?.name ?? "");
  const [results, setResults] = useState<Place[]>([]);
  const [busy, setBusy] = useState(false);
  const searching = !!q.trim() && q !== value?.name;
  useEffect(() => {
    if (!searching) return;
    const t = window.setTimeout(async () => {
      setBusy(true);
      setResults(await findPlaces(q, language).catch(() => []));
      setBusy(false);
    }, 300);
    return () => window.clearTimeout(t);
  }, [q, searching, language]);
  const shown = searching ? results : [];
  return (
    <div className="relative">
      <MapPin className="pointer-events-none absolute left-2.5 top-2 size-4 text-ink-3" />
      <TextInput value={q} onChange={(e) => setQ(e.target.value)} placeholder={tr("Paris, Bruxelles, Montréal…", "London, New York, Berlin…")} className="pl-8" />
      {busy && <LoaderCircle className="absolute right-2.5 top-2 size-4 animate-spin text-ink-3" />}
      {!busy && value && q === value.name && <Check className="absolute right-2.5 top-2 size-4 text-good" />}
      {shown.length > 0 && (
        <ul className="absolute inset-x-0 top-full z-20 mt-1 overflow-hidden rounded-lg border border-line bg-popover p-1 shadow-lg">
          {shown.map((r) => (
            <li key={`${r.latitude},${r.longitude}`}>
              <button
                type="button"
                onClick={() => {
                  setQ(r.name);
                  setResults([]);
                  onChange(r);
                }}
                className="flex h-8 w-full items-center rounded-md px-2 text-left text-[13px] text-ink-2 hover:bg-hover hover:text-ink"
              >
                <span className="truncate">{r.label}</span>
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

/** Name, language, city and currency. */
export function YouFields({ value, onChange }: { value: YouDraft; onChange: (v: YouDraft) => void }) {
  return (
    <div className="grid gap-4 sm:grid-cols-2">
      <Field label={tr("Ton nom", "Your name")} hint={tr("Le premier mot sert de prénom (« Bonjour, … »).", "The first word is your first name (\"Good morning, …\").")}>
        <TextInput value={value.name} onChange={(e) => onChange({ ...value, name: e.target.value })} placeholder={tr("Camille Martin", "Alex Smith")} autoFocus />
      </Field>
      <Field label={tr("Langue", "Language")}>
        <Select value={value.locale} onChange={(e) => onChange({ ...value, locale: e.target.value, currency: value.location ? value.currency : (currencyOf(e.target.value.split("-")[1]) ?? value.currency) })}>
          {[...LOCALES, ...(LOCALES.some((l) => l.id === value.locale) ? [] : [{ id: value.locale, label: value.locale }])].map((l) => (
            <option key={l.id} value={l.id}>
              {l.label}
            </option>
          ))}
        </Select>
      </Field>
      <Field label={tr("Ta ville", "Your city")} hint={tr("Pour la météo, les jours fériés et ton fuseau.", "For weather, holidays and your time zone.")}>
        <CityField
          value={value.location}
          language={value.locale.slice(0, 2)}
          onChange={(p) =>
            onChange({
              ...value,
              location: p ? { name: p.name, latitude: p.latitude, longitude: p.longitude, ...(p.country ? { country: p.country } : {}) } : undefined,
              timezone: p?.timezone ?? value.timezone,
              currency: currencyOf(p?.country) ?? value.currency,
            })
          }
        />
      </Field>
      <Field label={tr("Devise", "Currency")} hint={tr("Tous les totaux y sont convertis.", "Every total is converted to it.")}>
        <Select value={value.currency} onChange={(e) => onChange({ ...value, currency: e.target.value })}>
          {[...new Set([value.currency, ...CURRENCIES])].map((c) => (
            <option key={c}>{c}</option>
          ))}
        </Select>
      </Field>
    </div>
  );
}

export const youSet = (v: YouDraft): Record<string, unknown> => ({
  "owner.name": v.name.trim(),
  locale: v.locale,
  currency: v.currency,
  timezone: v.timezone ?? null,
  location: v.location ?? null,
});

/** "Unsaved changes — Cancel / Save", stuck to the bottom while something changed. */
export function SaveBar({ dirty, busy, error, saved, onSave, onReset }: { dirty: boolean; busy: boolean; error: string | null; saved: boolean; onSave: () => void; onReset: () => void }) {
  if (!dirty && !error && !saved) return null;
  return (
    <div className="sticky bottom-4 z-30 mt-6 flex items-center gap-3 rounded-xl border border-line bg-popover/95 px-4 py-2.5 shadow-lg backdrop-blur">
      <span className={cn("min-w-0 flex-1 truncate text-[13px]", error ? "text-bad" : "text-ink-2")} title={error ?? undefined}>
        {error ?? (dirty ? tr("Modifications non enregistrées", "Unsaved changes") : tr("Enregistré.", "Saved."))}
      </span>
      {dirty && (
        <>
          <Button onClick={onReset} disabled={busy}>
            {tr("Annuler", "Cancel")}
          </Button>
          <Button variant="primary" onClick={onSave} disabled={busy}>
            {busy && <LoaderCircle className="size-3.5 animate-spin" />}
            {tr("Enregistrer", "Save")}
          </Button>
        </>
      )}
    </div>
  );
}
