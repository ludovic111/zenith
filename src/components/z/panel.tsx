import { cn } from "@/lib/utils";

/**
 * A card: a title row (optional kicker above it, an action on the right) and a body.
 * `accent` is accepted for older callers and ignored: cards carry no color of their own.
 */
export function Panel({
  title,
  kicker,
  action,
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
    <section className={cn("relative overflow-hidden rounded-xl border border-line bg-surface", className)}>
      {(title || action || kicker) && (
        <header className="flex min-h-11 items-center justify-between gap-4 px-4 pt-3">
          <div className="min-w-0">
            {kicker && <div className="text-2xs text-ink-3">{kicker}</div>}
            {title && <h2 className="truncate text-[13px] font-semibold text-ink">{title}</h2>}
          </div>
          {action && <div className="flex shrink-0 items-center gap-2 text-xs text-ink-3">{action}</div>}
        </header>
      )}
      <div className={cn("p-4", bodyClassName)}>{children}</div>
    </section>
  );
}

/** A page's own title block, under the title bar: one line of context, actions on the right. */
export function PageHeader({ title, description, action }: { title: React.ReactNode; description?: React.ReactNode; action?: React.ReactNode }) {
  return (
    <header className="mb-6 flex flex-wrap items-end justify-between gap-x-6 gap-y-3">
      <div className="min-w-0">
        <h1 className="text-xl font-semibold tracking-tight text-ink">{title}</h1>
        {description && <p className="mt-1 max-w-2xl text-sm text-ink-3">{description}</p>}
      </div>
      {action && <div className="flex shrink-0 items-center gap-2">{action}</div>}
    </header>
  );
}

/** A heading between groups of cards. */
export function SectionTitle({ children, action }: { children: React.ReactNode; action?: React.ReactNode }) {
  return (
    <div className="mb-3 mt-8 flex items-center justify-between gap-4 first:mt-0">
      <h2 className="text-[13px] font-semibold text-ink">{children}</h2>
      {action && <div className="text-xs text-ink-3">{action}</div>}
    </div>
  );
}

export function Empty({ children }: { children: React.ReactNode }) {
  return <div className="grid min-h-20 place-items-center rounded-lg border border-dashed border-line px-4 py-6 text-center text-sm text-ink-3">{children}</div>;
}

export function Skeleton({ className }: { className?: string }) {
  return <div className={cn("animate-pulse rounded-xl border border-line bg-muted", className)} />;
}

/** A small label. `color` only tints its dot. */
export function Chip({ children, color, className }: { children: React.ReactNode; color?: string; className?: string }) {
  return (
    <span className={cn("inline-flex items-center gap-1.5 rounded-md border border-line bg-muted px-1.5 py-0.5 text-2xs text-ink-2", className)}>
      {color && <span className="size-1.5 shrink-0 rounded-full" style={{ background: color }} />}
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
