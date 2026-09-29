import { mintPairingToken } from "@/lib/code/cli";
import { codeStatus } from "@/lib/code/manager";
import { CODE_BRAND } from "@/lib/code/brand";
import { forbidden, isSameOrigin, noStore } from "@/lib/code/http";

/** One-time pairing token for the embedded zenith code (handed over by postMessage). */
export async function POST(request: Request) {
  if (!isSameOrigin(request)) return forbidden();
  if (!codeStatus().running) return Response.json({ error: `${CODE_BRAND} is not running` }, { status: 503, headers: noStore });
  try {
    return Response.json({ token: await mintPairingToken() }, { headers: noStore });
  } catch (e) {
    return Response.json({ error: e instanceof Error ? e.message : String(e) }, { status: 500, headers: noStore });
  }
}
