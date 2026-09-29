import { CircleAlert, CircleCheck, CircleDashed, CircleX, LoaderCircle } from "lucide-react";
import { cn } from "@/lib/utils";
import { tr } from "@/lib/i18n";

export type Health = "up" | "down" | "warn" | "busy" | "unknown";

const STYLE: Record<Health, { color: string; icon: typeof CircleCheck; label: () => string }> = {
  up: { color: "var(--good)", icon: CircleCheck, label: () => tr("En ligne", "Online") },
  down: { color: "var(--bad)", icon: CircleX, label: () => tr("Hors ligne", "Offline") },
  warn: { color: "var(--warn)", icon: CircleAlert, label: () => tr("À surveiller", "Needs attention") },
  busy: { color: "#7dd3fc", icon: LoaderCircle, label: () => tr("En cours", "In progress") },
  unknown: { color: "var(--ink-3)", icon: CircleDashed, label: () => tr("Inconnu", "Unknown") },
};

/** State is always carried by an icon and a word, never by color alone. */
export function Status({ health, label, className }: { health: Health; label?: string; className?: string }) {
  const s = STYLE[health];
  const Icon = s.icon;
  return (
    <span className={cn("inline-flex items-center gap-1.5 text-xs font-medium", className)} style={{ color: s.color }}>
      <Icon className={cn("size-3.5", health === "busy" && "animate-spin")} />
      <span className="text-ink-2">{label ?? s.label()}</span>
    </span>
  );
}

export function LiveDot({ health = "up" }: { health?: Health }) {
  return (
    <span className="relative inline-block size-2 rounded-full live-dot" style={{ color: STYLE[health].color, background: STYLE[health].color }} />
  );
}

export function deployHealth(status?: string | null): Health {
  if (!status) return "unknown";
  if (status === "SUCCESS" || status === "SLEEPING") return "up";
  if (["BUILDING", "DEPLOYING", "QUEUED", "INITIALIZING", "WAITING"].includes(status)) return "busy";
  if (["FAILED", "CRASHED"].includes(status)) return "down";
  return "unknown";
}

/** Human label of a Railway deployment status. */
export function deployLabel(status: string): string {
  const labels: Record<string, [string, string]> = {
    SUCCESS: ["Déployé", "Deployed"],
    SLEEPING: ["En veille", "Sleeping"],
    BUILDING: ["Construction", "Building"],
    DEPLOYING: ["Déploiement", "Deploying"],
    QUEUED: ["En file", "Queued"],
    INITIALIZING: ["Démarrage", "Starting"],
    WAITING: ["En attente", "Waiting"],
    FAILED: ["Échec", "Failed"],
    CRASHED: ["Planté", "Crashed"],
    REMOVED: ["Retiré", "Removed"],
    SKIPPED: ["Ignoré", "Skipped"],
  };
  const l = labels[status];
  return l ? tr(l[0], l[1]) : status;
}

export function runHealth(r: { status: string; conclusion: string | null }): Health {
  if (r.status !== "completed") return "busy";
  if (r.conclusion === "success") return "up";
  if (r.conclusion === "failure" || r.conclusion === "timed_out") return "down";
  return "warn";
}
