import type { Metadata } from "next";
import { notFound } from "next/navigation";
import { config } from "@/lib/config";
import { ASSISTANTS, isAssistantId } from "@/lib/assistants";
import { AssistantSurface } from "@/components/assistants/assistant-surface";

type Props = { params: Promise<{ id: string }> };

export async function generateMetadata({ params }: Props): Promise<Metadata> {
  const { id } = await params;
  return isAssistantId(id) ? { title: ASSISTANTS[id].name } : {};
}

/** Claude or ChatGPT, docked beside the sidebar (zenith.app) or one click away (browser). */
export default async function AssistantPage({ params }: Props) {
  const { id } = await params;
  if (!isAssistantId(id) || !config().assistants.includes(id)) notFound();
  return <AssistantSurface id={id} />;
}
