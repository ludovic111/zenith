import { noStore } from "@/lib/code/http";
import { mayAct } from "@/lib/agent/auth";
import { recall } from "@/lib/agent/knowledge";

/** `?q=…`: what the team wrote down or said about it. */
export async function GET(request: Request) {
  if (!mayAct(request) && request.headers.get("sec-fetch-site") !== "same-origin") return Response.json({ error: "Forbidden" }, { status: 403 });
  const q = new URL(request.url).searchParams.get("q")?.trim() ?? "";
  if (!q || q.length > 200) return Response.json({ error: "Invalid query" }, { status: 400 });
  return Response.json({ text: await recall(q) }, { headers: noStore });
}
