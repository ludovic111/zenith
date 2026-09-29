import { cn } from "@/lib/utils";

export function Panel({
  title,
  kicker,
  action,
  accent,
  className,
  bodyClassName,
  children,
}: {
  title?: React.ReactNode;
  kicker?: React.ReactNode;
  action?: React.ReactNode;
  accent?: string;
  className?: string;
  bodyClassName?: string;
  children: React.ReactNode;
}) {
  return (
    <section
      className={cn(
        "relative overflow-hidden rounded-3xl border border-line bg-white/[0.035] backdrop-blur-md",
        "shadow-[inset_0_1px_0_0_rgb(255_255_255/0.06)]",
        className,
      )}
    >
      {accent && (
        <div
          aria-hidden
          className="pointer-events-none absolute inset-x-0 top-0 h-px"
          style={{ background: `linear-gradient(90deg, transparent, ${accent}, transparent)` }}
        />
      )}
      {(title || action) && (
        <header className="flex items-start justify-between gap-4 px-5 pt-5">
          <div>
            {kicker && <div className="mb-1 text-[11px] font-medium uppercase tracking-[0.2em] text-ink-3">{kicker}</div>}
            {title && <h2 className="font-display text-sm font-medium tracking-wide text-ink">{title}</h2>}
          </div>
          {action}
        </header>
      )}
      <div className={cn("p-5", bodyClassName)}>{children}</div>
    </section>
  );
}

export function Empty({ children }: { children: React.ReactNode }) {
  return <div className="grid min-h-24 place-items-center rounded-2xl border border-dashed border-line px-4 py-6 text-center text-sm text-ink-3">{children}</div>;
}

export function Skeleton({ className }: { className?: string }) {
  return <div className={cn("animate-pulse rounded-3xl border border-line bg-white/[0.03]", className)} />;
}

export function Chip({ children, color, className }: { children: React.ReactNode; color?: string; className?: string }) {
  return (
    <span
      className={cn("inline-flex items-center gap-1.5 rounded-full border border-line bg-white/[0.04] px-2.5 py-0.5 text-xs text-ink-2", className)}
      style={color ? { borderColor: `${color}55`, background: `${color}14` } : undefined}
    >
      {children}
    </span>
  );
}

export function ExtLink({ href, children, className }: { href: string; children: React.ReactNode; className?: string }) {
  return (
    <a href={href} target="_blank" rel="noopener noreferrer" className={cn("underline-offset-4 hover:underline", className)}>
      {children}
    </a>
  );
}
