"use client";

import { setL10n, type L10n } from "@/lib/i18n";

/** Sets the locale in the browser before the rest of the tree renders. */
export function L10nProvider({ value, children }: { value: L10n; children: React.ReactNode }) {
  setL10n(value);
  return children;
}
