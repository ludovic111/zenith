import { codeStatus } from "@/lib/code/manager";
import { noStore } from "@/lib/code/http";

/** zenith code status: running, port, pid, version, last error, whether it is built. */
export function GET() {
  return Response.json(codeStatus(), { headers: noStore });
}
