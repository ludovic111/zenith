import { noStore } from "@/lib/code/http";
import { mayAct } from "@/lib/agent/auth";
import { mark, now } from "@/lib/agent/now";

/** What is waiting for you, most pressing first (zenith's pages and local agents only). */
export async function GET(request: Request) {
  if (!mayAct(request) && request.headers.get("sec-fetch-site") !== "same-origin") return Response.json({ error: "Forbidden" }, { status: 403 });
  return Response.json(await now(), { headers: noStore });
}

/** `{ id, action: "done" | "snooze" | "restore", hours? }` */
export async function POST(request: Request) {
  if (!mayAct(request)) return Response.json({ error: "Forbidden" }, { status: 403 });
  const body = (await request.json().catch(() => null)) as { id?: unknown; action?: unknown; hours?: unknown } | null;
  const id = typeof body?.id === "string" ? body.id : "";
  const action = body?.action === "done" || body?.action === "snooze" || body?.action === "restore" ? body.action : null;
  if (!/^[a-z]+:[\w-]+$/.test(id) || !action) return Response.json({ error: "Invalid request" }, { status: 400 });
  const hours = typeof body?.hours === "number" && body.hours > 0 && body.hours <= 24 * 30 ? body.hours : undefined;
  await mark(id, action, hours);
  return Response.json({ ok: true }, { headers: noStore });
}
