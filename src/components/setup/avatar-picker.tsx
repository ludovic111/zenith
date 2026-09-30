"use client";

import { AVATAR_ACCESSORIES, AVATAR_SHAPES, avatarOf, type Avatar } from "@/lib/agent/avatar";
import { tr } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import { AgentAvatar } from "@/components/agent/agent-avatar";

const COLORS = ["#F5A524", "#F97316", "#EF4444", "#EC4899", "#8B5CF6", "#6366F1", "#0EA5E9", "#14B8A6", "#22C55E", "#84CC16", "#64748B"];

const ACCESSORY_NAMES = (): Record<string, string> => ({
  none: tr("Rien", "None"),
  antenna: tr("Antenne", "Antenna"),
  sprout: tr("Pousse", "Sprout"),
  star: tr("Étoile", "Star"),
  bow: tr("Nœud", "Bow"),
  crown: tr("Couronne", "Crown"),
  glasses: tr("Lunettes", "Glasses"),
  headset: tr("Casque", "Headset"),
});

/** Shape, color and accessory of an agent's face, each shown as it would look. */
export function AvatarPicker({ id, value, onChange }: { id: string; value: Partial<Avatar>; onChange: (v: Avatar) => void }) {
  const a = avatarOf(id, value);
  const set = (p: Partial<Avatar>) => onChange({ ...a, ...p });
  return (
    <div className="flex flex-col gap-3">
      <div className="flex flex-wrap gap-1">
        {AVATAR_SHAPES.map((s) => (
          <button key={s} type="button" onClick={() => set({ shape: s })} title={s} className={cn("grid size-9 place-items-center rounded-lg border transition-colors", a.shape === s ? "border-ink-3/60 bg-selected" : "border-transparent hover:bg-hover")}>
            <AgentAvatar avatar={{ ...a, shape: s }} id={`pick-${id}-${s}`} size={26} />
          </button>
        ))}
      </div>
      <div className="flex flex-wrap gap-1.5">
        {COLORS.map((c) => (
          <button key={c} type="button" onClick={() => set({ color: c })} title={c} className={cn("size-6 rounded-full ring-offset-2 ring-offset-surface transition", a.color.toLowerCase() === c.toLowerCase() ? "ring-2 ring-ink-3" : "hover:scale-110")} style={{ background: c }} />
        ))}
      </div>
      <div className="flex flex-wrap gap-1">
        {AVATAR_ACCESSORIES.map((x) => (
          <button key={x} type="button" onClick={() => set({ accessory: x })} className={cn("inline-flex h-8 items-center gap-1.5 rounded-md border px-2 text-xs transition-colors", a.accessory === x ? "border-ink-3/60 bg-selected text-ink" : "border-line text-ink-2 hover:bg-hover")}>
            <AgentAvatar avatar={{ ...a, accessory: x }} id={`acc-${id}-${x}`} size={18} />
            {ACCESSORY_NAMES()[x]}
          </button>
        ))}
      </div>
    </div>
  );
}
