import type { LucideIcon } from "lucide-react";
import { cn } from "@/lib/utils";

/**
 * Dense rows for list cards (Linear / Things style). Put them in a Panel with
 * `bodyClassName="p-0 pb-1.5"`: rows run edge to edge and carry their own padding.
 */

export function Rows({ children, className }: { children: React.ReactNode; className?: string }) {
  return <ul className={cn("divide-y divide-line", className)}>{children}</ul>;
}

/** A small heading inside a list card, with a count or an action on the right. */
export function RowGroup({ title, count, action, children, className }: { title: React.ReactNode; count?: number; action?: React.ReactNode; children: React.ReactNode; className?: string }) {
  return (
    <div className={className}>
      <div className="flex h-8 items-center justify-between gap-3 border-t border-line bg-muted/50 px-4 text-xs text-ink-3">
        <span className="min-w-0 truncate font-medium text-ink-2">
          {title}
          {count != null && <span className="ml-1.5 font-normal text-ink-3 tabular">{count}</span>}
        </span>
        {action && <span className="flex shrink-0 items-center gap-2">{action}</span>}
      </div>
      <Rows className="border-t border-line">{children}</Rows>
    </div>
  );
}

const isExternal = (href: string) => !href.startsWith("/");

export function Row({
  icon: Icon,
  tone,
  dot,
  title,
  href,
  meta,
  aside,
  action,
  muted,
  className,
}: {
  icon?: LucideIcon;
  /** Icon color class (status colors only: text-bad, text-warn, text-good). */
  tone?: string;
  /** A small colored dot instead of an icon (a project color). */
  dot?: string;
  title: React.ReactNode;
  href?: string | null;
  meta?: React.ReactNode;
  aside?: React.ReactNode;
  action?: React.ReactNode;
  muted?: boolean;
  className?: string;
}) {
  return (
    <li className={cn("flex min-h-10 items-center gap-3 px-4 py-2 transition-colors hover:bg-hover", muted && "opacity-55", className)}>
      {Icon ? (
        <Icon className={cn("size-4 shrink-0", tone ?? "text-ink-3", meta != null && "self-start mt-0.5")} />
      ) : dot ? (
        <span className={cn("mx-[5px] size-2 shrink-0 rounded-full", meta != null && "self-start mt-1.5")} style={{ background: dot }} />
      ) : null}
      <div className="min-w-0 flex-1">
        <div className="truncate text-[13px] text-ink">
          {href ? (
            <a href={href} target={isExternal(href) ? "_blank" : undefined} rel="noopener noreferrer" className="underline-offset-2 hover:underline">
              {title}
            </a>
          ) : (
            title
          )}
        </div>
        {meta != null && <div className="truncate text-xs text-ink-3">{meta}</div>}
      </div>
      {aside != null && <span className="shrink-0 text-xs text-ink-3 tabular max-sm:hidden">{aside}</span>}
      {action}
    </li>
  );
}

/** A title with its count, for Panel titles and section headings. */
export function Counted({ children, count }: { children: React.ReactNode; count?: number | null }) {
  return (
    <>
      {children}
      {count != null && count > 0 && <span className="ml-1.5 font-normal text-ink-3 tabular">{count}</span>}
    </>
  );
}
