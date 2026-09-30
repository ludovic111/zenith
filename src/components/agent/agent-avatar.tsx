import { avatarSvg, type Avatar } from "@/lib/agent/avatar";
import { cn } from "@/lib/utils";

/** An agent's face: its shape, color and accessory, with eyes that blink when `blink`. */
export function AgentAvatar({ avatar, id, size = 16, blink = false, className }: { avatar: Avatar; id: string; size?: number; blink?: boolean; className?: string }) {
  return (
    <span
      className={cn("inline-grid shrink-0 place-items-center [&>svg]:block", className)}
      style={{ width: size, height: size }}
      // Our own markup, from validated shapes and colors.
      dangerouslySetInnerHTML={{ __html: avatarSvg(avatar, { size, blink, id: `${id}-${size}` }) }}
    />
  );
}
