"use client";

import { cn } from "@/lib/utils";

/** Form controls in zenith's quiet style, for the welcome and the settings. */

export const inputClass =
  "h-8 w-full rounded-md border border-line bg-background px-2.5 text-[13px] text-ink outline-none transition-colors placeholder:text-ink-3 focus:border-ink-3/60 disabled:opacity-60";

export function Field({ label, hint, children, className }: { label: string; hint?: React.ReactNode; children: React.ReactNode; className?: string }) {
  return (
    <label className={cn("flex min-w-0 flex-col gap-1", className)}>
      <span className="text-xs text-ink-2">{label}</span>
      {children}
      {hint && <span className="text-2xs text-ink-3">{hint}</span>}
    </label>
  );
}

export function TextInput({ className, ...props }: React.InputHTMLAttributes<HTMLInputElement>) {
  return <input {...props} className={cn(inputClass, className)} />;
}

export function TextArea({ className, ...props }: React.TextareaHTMLAttributes<HTMLTextAreaElement>) {
  return <textarea {...props} className={cn(inputClass, "h-auto min-h-16 resize-y py-1.5 leading-5", className)} />;
}

export function Select({ className, children, ...props }: React.SelectHTMLAttributes<HTMLSelectElement>) {
  return (
    <select {...props} className={cn(inputClass, "pr-7", className)}>
      {children}
    </select>
  );
}

/** Two to four choices side by side. */
export function Segmented<T extends string>({ value, options, onChange, className }: { value: T; options: { id: T; label: React.ReactNode }[]; onChange: (v: T) => void; className?: string }) {
  return (
    <div role="radiogroup" className={cn("inline-flex h-8 items-center rounded-md bg-muted p-0.5", className)}>
      {options.map((o) => (
        <button
          key={o.id}
          type="button"
          role="radio"
          aria-checked={value === o.id}
          onClick={() => onChange(o.id)}
          className={cn(
            "inline-flex h-7 items-center gap-1.5 rounded-[5px] px-2.5 text-xs transition-colors",
            value === o.id ? "bg-surface text-ink shadow-xs dark:bg-selected" : "text-ink-3 hover:text-ink-2",
          )}
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}

export function Switch({ on, onChange, label }: { on: boolean; onChange: (v: boolean) => void; label?: string }) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={on}
      aria-label={label}
      onClick={() => onChange(!on)}
      className={cn("relative h-5 w-9 shrink-0 rounded-full transition-colors", on ? "bg-primary" : "bg-ink-3/30")}
    >
      <span className={cn("absolute left-0.5 top-0.5 size-4 rounded-full bg-white shadow-sm transition-transform", on && "translate-x-4")} />
    </button>
  );
}

export function Button({ variant = "quiet", className, ...props }: React.ButtonHTMLAttributes<HTMLButtonElement> & { variant?: "primary" | "quiet" | "danger" }) {
  return (
    <button
      type="button"
      {...props}
      className={cn(
        "inline-flex h-8 items-center justify-center gap-1.5 rounded-md px-3 text-[13px] transition-colors disabled:opacity-50",
        variant === "primary" && "bg-primary text-primary-foreground hover:bg-primary/90",
        variant === "quiet" && "border border-line bg-surface text-ink-2 hover:bg-hover hover:text-ink",
        variant === "danger" && "text-bad hover:bg-bad/10",
        className,
      )}
    />
  );
}
