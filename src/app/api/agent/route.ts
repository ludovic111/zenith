import { config } from "@/lib/config";
import { tr } from "@/lib/i18n";
import { noStore } from "@/lib/code/http";
import { codeStatus } from "@/lib/code/manager";
import { ask, targets, type AskSource } from "@/lib/agent/ask";
import { mayAct, underLimit, viaToken } from "@/lib/agent/auth";
import { findNow } from "@/lib/agent/now";

/** Where requests can go, the default provider, and whether zenith code can run them. */
export function GET(request: Request) {
  if (!mayAct(request) && request.headers.get("sec-fetch-site") !== "same-origin") return Response.json({ error: "Forbidden" }, { status: 403 });
  const c = config();
  return Response.json(
    { enabled: c.agent.enabled && c.code.enabled, running: codeStatus().running, provider: c.agent.provider, targets: targets().map(({ id, name }) => ({ id, name })) },
    { headers: noStore },
  );
}

/**
 * Ask zenith: `{ prompt, target?, provider?, nowId? }` starts an agent and returns the
 * thread to open. With only `nowId`, the agent gets that Now item's own request.
 */
export async function POST(request: Request) {
  if (!mayAct(request)) return Response.json({ error: "Forbidden" }, { status: 403 });
  const local = viaToken(request);
  if ((local && !underLimit("token", 12)) || !underLimit("all", 40)) return Response.json({ error: tr("Trop de demandes en une heure.", "Too many requests this hour.") }, { status: 429 });
  let body: { prompt?: unknown; target?: unknown; provider?: unknown; nowId?: unknown; source?: unknown };
  try {
    body = await request.json();
  } catch {
    return Response.json({ error: "Invalid JSON" }, { status: 400 });
  }
  const str = (v: unknown) => (typeof v === "string" && v.trim() ? v.trim() : undefined);
  const nowId = str(body.nowId);
  const item = nowId ? await findNow(nowId) : null;
  const prompt = str(body.prompt) ?? item?.prompt;
  if (!prompt) return Response.json({ error: tr("Dis-moi ce que tu veux.", "Tell me what you want.") }, { status: 400 });
  if (prompt.length > 20_000) return Response.json({ error: tr("Demande trop longue.", "Request too long.") }, { status: 413 });
  const provider = body.provider === "codex" || body.provider === "claude" ? body.provider : undefined;
  const source: AskSource = local ? "mcp" : body.source === "command" || body.source === "now" ? body.source : "bar";
  try {
    const result = await ask({
      prompt,
      target: str(body.target) ?? item?.target,
      provider,
      source: item ? "now" : source,
      nowId: item?.id,
      title: item && !str(body.prompt) ? item.title : undefined,
    });
    return Response.json(result, { headers: noStore });
  } catch (e) {
    return Response.json({ error: e instanceof Error ? e.message : String(e) }, { status: 502, headers: noStore });
  }
}
