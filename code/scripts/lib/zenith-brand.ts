/**
 * zenith code branding, applied at build time.
 *
 * Upstream T3 Code spells its name in ~150 user-facing strings. Rewriting them
 * in source would make every upstream sync conflict, so the web and server
 * builds run this plugin instead: it swaps the product name in first-party
 * modules (never node_modules) and in index.html. The brand lives here only.
 */

export const ZENITH_BRAND_NAME = "zenith code";

const UPSTREAM_BRAND_NAME = /T3 Code/g;
const FIRST_PARTY_MODULE = /\.(?:[cm]?[jt]sx?)$/;

function rebrand(source: string): string | null {
  return source.includes("T3 Code") ? source.replace(UPSTREAM_BRAND_NAME, ZENITH_BRAND_NAME) : null;
}

/** Vite / Rolldown plugin (both accept this shape). */
export function zenithBrandPlugin() {
  return {
    name: "zenith:brand",
    enforce: "pre" as const,
    transform(code: string, id: string) {
      const file = id.split("?")[0] ?? id;
      if (file.includes("/node_modules/") || !FIRST_PARTY_MODULE.test(file)) return null;
      const next = rebrand(code);
      return next === null ? null : { code: next, map: null };
    },
    transformIndexHtml(html: string) {
      return rebrand(html) ?? html;
    },
  };
}
