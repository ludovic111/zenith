import type { SVGProps } from "react";

/**
 * zenith: the upstream "T3" wordmark is replaced by zenith's mark, the sky's
 * circle with the sun at its highest point, in the current color like the dashboard's.
 * The export name stays for easy upstream merges.
 */
export function T3Wordmark(props: SVGProps<SVGSVGElement>) {
  return (
    <svg {...props} viewBox="0 0 40 40" xmlns="http://www.w3.org/2000/svg">
      <circle cx="20" cy="22" r="13" fill="none" stroke="currentColor" strokeWidth="3.5" />
      <circle cx="20" cy="9" r="6" fill="currentColor" />
    </svg>
  );
}
