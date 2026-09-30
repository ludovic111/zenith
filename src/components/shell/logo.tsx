/** zenith's mark: the sky's circle with the sun at its highest point, in the current color. */
export function ZenithMark({ size = 16, className }: { size?: number; className?: string }) {
  return (
    <svg width={size} height={size} viewBox="0 0 40 40" aria-hidden className={className}>
      <circle cx="20" cy="22" r="13" fill="none" stroke="currentColor" strokeWidth="3.5" />
      <circle cx="20" cy="9" r="6" fill="currentColor" />
    </svg>
  );
}
