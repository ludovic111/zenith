import os from "node:os";
import { cn } from "@/lib/utils";
import { Copy } from "@/components/identity/copy";

/**
 * Settings, the way macOS System Settings and zenith code draw them: grouped cards of rows,
 * a label (and a line of explanation) on the left, the value or the control on the right.
 */

export const tilde = (p: string) => (p.startsWith(os.homedir()) ? `~${p.slice(os.homedir().length)}` : p);

/** A titled card of rows. */
export function Group({ title, description, action, children, className, id }: { title?: React.ReactNode; description?: React.ReactNode; action?: React.ReactNode; children: React.ReactNode; className?: string; id?: string }) {
  return (
    <section id={id} className={cn("mt-8 first:mt-0", className)}>
      {(title || action) && (
        <div className="mb-2 flex items-end justify-between gap-4 px-1">
          <div className="min-w-0">
            {title && <h2 className="text-[13px] font-semibold text-ink">{title}</h2>}
            {description && <p className="mt-0.5 text-xs text-ink-3">{description}</p>}
          </div>
          {action && <div className="flex shrink-0 items-center gap-2 text-xs text-ink-3">{action}</div>}
        </div>
      )}
      <div className="divide-y divide-line overflow-hidden rounded-xl border border-line bg-surface">{children}</div>
    </section>
  );
}

/** One row: label and description on the left, value on the right. `stack` puts the value below (long values, code). */
export function Row({ label, description, children, stack, className }: { label: React.ReactNode; description?: React.ReactNode; children?: React.ReactNode; stack?: boolean; className?: string }) {
  return (
    <div className={cn("px-4 py-3", stack ? "space-y-2" : "flex min-h-12 flex-wrap items-center justify-between gap-x-6 gap-y-2", className)}>
      <div className="min-w-0 max-w-xl">
        <div className="text-[13px] text-ink">{label}</div>
        {description && <div className="mt-0.5 text-xs text-ink-3">{description}</div>}
      </div>
      {children != null && <div className={cn("min-w-0 text-[13px] text-ink-2", !stack && "flex items-center justify-end gap-2 text-right")}>{children}</div>}
    </div>
  );
}

/** A value in monospace, copied in one click. */
export function Mono({ value, children }: { value: string; children?: React.ReactNode }) {
  return (
    <Copy value={value} mono className="text-ink-2">
      {children ?? value}
    </Copy>
  );
}

/** A command or file to copy, in a code well. */
export function CodeBlock({ code, display }: { code: string; display?: string }) {
  return (
    <div className="rounded-md border border-line bg-muted px-3 py-2">
      <Copy value={code} mono wrap className="w-full leading-relaxed">
        {display ?? code}
      </Copy>
    </div>
  );
}

/** Inline code. */
export function Code({ children }: { children: React.ReactNode }) {
  return <code className="rounded bg-muted px-1 py-px font-mono text-[0.92em] text-ink-2">{children}</code>;
}

/** Yes / no, as a word with a dot. */
export function Toggle({ on, labels }: { on: boolean; labels?: [string, string] }) {
  return (
    <span className="inline-flex items-center gap-1.5 text-[13px] text-ink-2">
      <span className={cn("size-1.5 rounded-full", on ? "bg-good" : "bg-ink-3/50")} />
      {on ? labels?.[0] : labels?.[1]}
    </span>
  );
}
