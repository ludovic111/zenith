import "server-only";

/** Only zenith's own pages may call these routes (proxy.ts already rejects non-local hosts). */
export function isSameOrigin(request: Request): boolean {
  const origin = request.headers.get("origin");
  if (request.headers.get("sec-fetch-site") !== "same-origin" || !origin) return false;
  try {
    // Compare with the Host header: request.url may say "localhost" in dev.
    return new URL(origin).host === request.headers.get("host");
  } catch {
    return false;
  }
}

export const forbidden = () => Response.json({ error: "Forbidden" }, { status: 403 });

export const noStore = { "Cache-Control": "no-store" };
