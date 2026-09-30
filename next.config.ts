import { existsSync } from "node:fs";
import type { NextConfig } from "next";

/**
 * perso/ holds what is yours only (your config, your extensions, your pages). Git ignores
 * it; when it exists, `@perso` points at perso/index.ts, otherwise at an empty list.
 */
const perso = existsSync("perso/index.ts") ? "./perso/index.ts" : "./src/lib/perso-empty.ts";

const nextConfig: NextConfig = {
  // The updater builds the next version beside the running one, then swaps them.
  distDir: process.env.ZENITH_DIST_DIR || ".next",
  devIndicators: false,
  turbopack: { resolveAlias: { "@perso": perso } },
};

export default nextConfig;
