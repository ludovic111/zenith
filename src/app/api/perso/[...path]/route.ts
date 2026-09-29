import { extensionRoute } from "@/lib/extensions";

/** API routes added by extensions (perso/), at /api/perso/<path>. Same-origin only. */
export async function GET(request: Request, { params }: { params: Promise<{ path: string[] }> }) {
  if (request.headers.get("sec-fetch-site") === "cross-site") return Response.json({ error: "Forbidden" }, { status: 403 });
  const route = extensionRoute((await params).path.join("/"));
  if (!route) return Response.json({ error: "Not found" }, { status: 404 });
  return route.GET(request);
}
