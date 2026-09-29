/** Claude and ChatGPT inside zenith (scripts/mac/ZenithApp.swift docks their desktop apps). */

export const ASSISTANT_IDS = ["claude", "chatgpt"] as const;
export type AssistantId = (typeof ASSISTANT_IDS)[number];

export type Assistant = { id: AssistantId; name: string; color: string; web: string; app: string };

export const ASSISTANTS: Record<AssistantId, Assistant> = {
  claude: { id: "claude", name: "Claude", color: "#D97757", web: "https://claude.ai/new", app: "Claude" },
  chatgpt: { id: "chatgpt", name: "ChatGPT", color: "#10A37F", web: "https://chatgpt.com/", app: "ChatGPT" },
};

export const isAssistantId = (id: string): id is AssistantId => (ASSISTANT_IDS as readonly string[]).includes(id);

export const assistantHref = (id: AssistantId) => `/assistants/${id}`;
