import { mintPairingToken } from "@/lib/code/cli";
import { codeStatus } from "@/lib/code/manager";
import { resolveCodeTarget } from "@/lib/code/target";

/**
 * "Open in its own window": pairs the browser that follows this link, then lands
 * on zenith code (focused on ?project= when given). Only for navigations started
 * by the user or by zenith itself, never by another site.
 */
export async function GET(request: Request) {
  const site = request.headers.get("sec-fetch-site");
  if (site && site !== "same-origin" && site !== "none") return new Response("Forbidden", { status: 403 });

  const url = new URL(request.url);
  const status = codeStatus();
  if (!status.running) return Response.redirect(new URL("/code", url), 302);

  const target = resolveCodeTarget(url.searchParams.get("project"));
  const next = new URL("/pair", status.origin);
  if (target) next.searchParams.set("zenithProject", target.dir);
  try {
    next.hash = `token=${await mintPairingToken()}`;
  } catch {
    // Already-paired browsers do not need a token; others land on the pairing screen.
  }
  return new Response(null, { status: 302, headers: { Location: next.toString(), "Cache-Control": "no-store", "Referrer-Policy": "no-referrer" } });
}
