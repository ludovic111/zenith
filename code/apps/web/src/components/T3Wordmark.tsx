// oxlint-disable shadcn/no-raw-colors -- the brand mark keeps its exact colors in every theme.
import { useId, type SVGProps } from "react";

/**
 * zenith: the upstream "T3" wordmark is replaced by zenith's mark, the sun at
 * its zenith over a horizon arc. The export name stays for easy upstream merges.
 */
export function T3Wordmark(props: SVGProps<SVGSVGElement>) {
  const id = useId();
  const sun = `${id}-sun`;
  const arc = `${id}-arc`;
  return (
    <svg {...props} viewBox="2 0 36 36" xmlns="http://www.w3.org/2000/svg">
      <defs>
        <radialGradient id={sun} cx="50%" cy="40%" r="60%">
          <stop offset="0%" stopColor="#FFF4CF" />
          <stop offset="45%" stopColor="#FFD166" />
          <stop offset="100%" stopColor="#FF8A4C" />
        </radialGradient>
        <linearGradient id={arc} x1="0" x2="1">
          <stop offset="0%" stopColor="#00D2FF" />
          <stop offset="35%" stopColor="#B18CFF" />
          <stop offset="70%" stopColor="#FF6FB5" />
          <stop offset="100%" stopColor="#B6F23A" />
        </linearGradient>
      </defs>
      <path
        d="M4 34 A16 16 0 0 1 36 34"
        fill="none"
        stroke={`url(#${arc})`}
        strokeWidth="2.5"
        strokeLinecap="round"
      />
      <circle cx="20" cy="11" r="6.5" fill={`url(#${sun})`} />
      <circle cx="20" cy="11" r="9.5" fill="none" stroke="#FFD166" strokeOpacity=".35" />
    </svg>
  );
}
