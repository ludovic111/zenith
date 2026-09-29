import type { Metadata } from "next";
import { codeStatus } from "@/lib/code/manager";
import { resolveCodeTarget } from "@/lib/code/target";
import { CodeRoute } from "@/components/code/code-route";

export const dynamic = "force-dynamic";
export const metadata: Metadata = { title: "Code" };

/**
 * zenith code. The app itself is <CodeHost> in the root layout (it outlives page changes);
 * this route only makes it visible. `/code/<path>` mirrors the app's own path, and
 * `/code?project=<id>` focuses a project.
 */
export default async function CodePage({ searchParams }: { searchParams: Promise<Record<string, string | string[] | undefined>> }) {
  const { project } = await searchParams;
  const target = resolveCodeTarget(typeof project === "string" ? project : null);
  return <CodeRoute status={codeStatus()} target={target} />;
}
