import { siClaude } from "simple-icons";
import { Sparkles } from "lucide-react";
import type { AssistantId } from "@/lib/assistants";

/** Claude's mark (Simple Icons); ChatGPT has none there, so a spark in its green. */
export function AssistantIcon({ id, size = 16, className }: { id: AssistantId; size?: number; className?: string }) {
  if (id === "chatgpt") return <Sparkles width={size} height={size} className={className} color="#10A37F" aria-label="ChatGPT" />;
  return (
    <svg viewBox="0 0 24 24" width={size} height={size} className={className} role="img" aria-label="Claude" fill={`#${siClaude.hex}`}>
      <path d={siClaude.path} />
    </svg>
  );
}
