/** The sun at its zenith: a star resting on top of a horizon arc. */
export function ZenithMark({ size = 36 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 40 40" aria-hidden>
      <defs>
        <radialGradient id="zsun" cx="50%" cy="40%" r="60%">
          <stop offset="0%" stopColor="#FFF4CF" />
          <stop offset="45%" stopColor="#FFD166" />
          <stop offset="100%" stopColor="#FF8A4C" />
        </radialGradient>
        <linearGradient id="zarc" x1="0" x2="1">
          <stop offset="0%" stopColor="#00D2FF" />
          <stop offset="35%" stopColor="#B18CFF" />
          <stop offset="70%" stopColor="#FF6FB5" />
          <stop offset="100%" stopColor="#B6F23A" />
        </linearGradient>
      </defs>
      <path d="M4 34 A16 16 0 0 1 36 34" fill="none" stroke="url(#zarc)" strokeWidth="2.5" strokeLinecap="round" />
      <line x1="20" y1="18" x2="20" y2="34" stroke="rgb(255 255 255 / .25)" strokeWidth="1" strokeDasharray="2 2" />
      <circle cx="20" cy="11" r="6.5" fill="url(#zsun)" />
      <circle cx="20" cy="11" r="9.5" fill="none" stroke="#FFD166" strokeOpacity=".35" />
    </svg>
  );
}
