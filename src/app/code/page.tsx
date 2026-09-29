import type { Metadata } from "next";
import { codeStatus } from "@/lib/code/manager";
import { resolveCodeTarget } from "@/lib/code/target";
import { CodeWorkspace } from "./code-workspace";

export const dynamic = "force-dynamic";
export const metadata: Metadata = { title: "Code" };

/** zenith code, full height beside the sidebar. `/code?project=<id>` focuses a project. */
export default async function CodePage({ searchParams }: { searchParams: Promise<Record<string, string | string[] | undefined>> }) {
  const { project } = await searchParams;
  const target = resolveCodeTarget(typeof project === "string" ? project : null);
  return <CodeWorkspace initialStatus={codeStatus()} target={target} />;
}
