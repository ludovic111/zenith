import type { Extension } from "./extensions";

/**
 * Used when there is no perso/ folder. Your own extensions go in perso/index.ts
 * (ignored by git), exporting `PERSO: Extension[]`: see docs/extensions.md.
 */
export const PERSO: Extension[] = [];
