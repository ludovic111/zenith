import { tr } from "@/lib/i18n";
import { isSameOrigin, noStore } from "@/lib/code/http";
import { rawConfig, updateConfig } from "@/lib/config-write";

/** Your projects as written in the file (the welcome adds to them without losing a field). */
export async function GET(request: Request) {
  if (request.headers.get("sec-fetch-site") !== "same-origin") return Response.json({ error: "Forbidden" }, { status: 403 });
  const raw = await rawConfig();
  return Response.json({ projects: Array.isArray(raw.projects) ? raw.projects : [] }, { headers: noStore });
}

/**
 * `{ set: { "<dotted.path>": value | null } }`: changes zenith.config.json from the
 * welcome and the settings. Only zenith's own pages may: it rewrites your whole setup.
 */
export async function POST(request: Request) {
  if (!isSameOrigin(request) || !(request.headers.get("content-type") ?? "").startsWith("application/json"))
    return Response.json({ error: tr("Interdit", "Forbidden") }, { status: 403 });
  const body = (await request.json().catch(() => null)) as { set?: unknown } | null;
  const set = body?.set;
  if (!set || typeof set !== "object" || Array.isArray(set)) return Response.json({ error: tr("Rien à changer.", "Nothing to change.") }, { status: 400 });
  try {
    const r = await updateConfig(set as Record<string, unknown>);
    if (!r.ok) return Response.json({ error: r.issues.map((i) => `${i.path}: ${i.message}`).join(" · "), issues: r.issues }, { status: 422, headers: noStore });
    return Response.json({ ok: true }, { headers: noStore });
  } catch (e) {
    return Response.json({ error: e instanceof Error ? e.message : String(e) }, { status: 500, headers: noStore });
  }
}
