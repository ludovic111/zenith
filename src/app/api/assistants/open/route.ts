import { execFile } from "node:child_process";
import { config } from "@/lib/config";
import { ASSISTANTS, isAssistantId } from "@/lib/assistants";
import { forbidden, isSameOrigin, noStore } from "@/lib/code/http";

/**
 * Opens Claude's or ChatGPT's desktop app on this Mac (`open -a`), for pages seen in a
 * browser, where the app can't be docked. 404 when it isn't installed.
 */
export async function POST(request: Request) {
  if (!isSameOrigin(request)) return forbidden();
  const id = new URL(request.url).searchParams.get("app") ?? "";
  if (!isAssistantId(id) || !config().assistants.includes(id)) return Response.json({ error: "Unknown app" }, { status: 400, headers: noStore });
  const ok = await new Promise<boolean>((resolve) => execFile("open", ["-a", ASSISTANTS[id].app], (err) => resolve(!err)));
  return Response.json({ ok }, { status: ok ? 200 : 404, headers: noStore });
}
