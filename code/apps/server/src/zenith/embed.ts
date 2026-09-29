/**
 * zenith embedding support.
 *
 * zenith shows this app in an iframe. The parent origins allowed to frame it
 * (and to hand it one-time pairing tokens over postMessage) come from
 * ZENITH_CODE_PARENT_ORIGINS, a comma-separated list of http(s) origins.
 */
import * as Effect from "effect/Effect";
import { HttpRouter, HttpServerResponse } from "effect/unstable/http";

export const ZENITH_EMBED_CONFIG_PATH = "/zenith/embed.json";

const DEFAULT_PARENT_ORIGINS = "http://127.0.0.1:4747,http://127.0.0.1:4748";

/** Normalized, validated origins (anything that is not a bare http(s) origin is dropped). */
export function zenithParentOrigins(
  raw: string | undefined = process.env.ZENITH_CODE_PARENT_ORIGINS,
): ReadonlyArray<string> {
  const origins = new Set<string>();
  for (const entry of (raw ?? DEFAULT_PARENT_ORIGINS).split(",")) {
    const value = entry.trim();
    if (!value) continue;
    try {
      const url = new URL(value);
      if ((url.protocol === "http:" || url.protocol === "https:") && url.origin === value) {
        origins.add(url.origin);
      }
    } catch {
      // Ignore malformed entries.
    }
  }
  return [...origins];
}

/** CSP for HTML documents: only this server and the allowed zenith pages may frame the app. */
export function zenithFrameAncestorsPolicy(): string {
  return ["frame-ancestors", "'self'", ...zenithParentOrigins()].join(" ");
}

/** Public, unauthenticated: tells the web client which parents it may talk to. */
export const zenithEmbedRouteLayer = HttpRouter.add(
  "GET",
  ZENITH_EMBED_CONFIG_PATH,
  Effect.sync(() =>
    HttpServerResponse.jsonUnsafe(
      { parentOrigins: zenithParentOrigins() },
      { headers: { "cache-control": "no-store" } },
    ),
  ),
);
