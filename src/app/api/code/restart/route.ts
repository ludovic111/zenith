import { codeStatus, restartCodeServer } from "@/lib/code/manager";
import { forbidden, isSameOrigin, noStore } from "@/lib/code/http";

/** Restarts the zenith code server; answers once the new process is spawned. */
export async function POST(request: Request) {
  if (!isSameOrigin(request)) return forbidden();
  await restartCodeServer();
  return Response.json(codeStatus(), { headers: noStore });
}
