import { isSameOrigin, noStore } from "@/lib/code/http";
import { applyUpdate, checkUpdate, updateState } from "@/lib/updater";

/** Where the update stands. */
export async function GET(request: Request) {
  if (request.headers.get("sec-fetch-site") !== "same-origin") return Response.json({ error: "Forbidden" }, { status: 403 });
  return Response.json(await updateState(), { headers: noStore });
}

/** `{ action: "check" | "apply" }`, from Settings. Applying returns once the build is ready (or failed). */
export async function POST(request: Request) {
  if (!isSameOrigin(request)) return Response.json({ error: "Forbidden" }, { status: 403 });
  const body = (await request.json().catch(() => null)) as { action?: unknown } | null;
  if (body?.action === "check") return Response.json(await checkUpdate(), { headers: noStore });
  if (body?.action === "apply") return Response.json(await applyUpdate(), { headers: noStore });
  return Response.json({ error: "Invalid request" }, { status: 400 });
}
