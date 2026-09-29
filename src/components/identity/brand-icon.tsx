import {
  siApple, siAppstore, siBluesky, siCloudflare, siGithub, siGmail, siGoogleplay, siInstagram, siMastodon, siOpenrouter,
  siRailway, siRevenuecat, siSolana, siStripe, siSupabase, siTelegram, siThreads, siTiktok, siVercel, siX, siYoutube,
} from "simple-icons";
import { Globe } from "lucide-react";
import type { Brand } from "@/lib/identity";

type Icon = { path: string; hex: string; title: string };

/** LinkedIn is no longer in Simple Icons: its former CC0 glyph. */
const LINKEDIN: Icon = {
  title: "LinkedIn",
  hex: "0A66C2",
  path: "M20.447 20.452h-3.554v-5.569c0-1.328-.027-3.037-1.852-3.037-1.853 0-2.136 1.445-2.136 2.939v5.667H9.351V9h3.414v1.561h.046c.477-.9 1.637-1.85 3.37-1.85 3.601 0 4.267 2.37 4.267 5.455v6.286zM5.337 7.433c-1.144 0-2.063-.926-2.063-2.065 0-1.138.92-2.063 2.063-2.063 1.14 0 2.064.925 2.064 2.063 0 1.139-.925 2.065-2.064 2.065zm1.782 13.019H3.555V9h3.564v11.452zM22.225 0H1.771C.792 0 0 .774 0 1.729v20.542C0 23.227.792 24 1.771 24h20.451C23.2 24 24 23.227 24 22.271V1.729C24 .774 23.2 0 22.222 0h.003z",
};

const ICONS: Record<Exclude<Brand, "web">, Icon> = {
  x: siX, instagram: siInstagram, youtube: siYoutube, tiktok: siTiktok, github: siGithub, telegram: siTelegram,
  linkedin: LINKEDIN, bluesky: siBluesky, mastodon: siMastodon, threads: siThreads,
  appstore: siAppstore, googleplay: siGoogleplay, railway: siRailway, supabase: siSupabase, revenuecat: siRevenuecat,
  openrouter: siOpenrouter, solana: siSolana, gmail: siGmail, apple: siApple,
  vercel: siVercel, cloudflare: siCloudflare, stripe: siStripe,
};

/** Brand logo (Simple Icons). Black logos are lightened to stay readable on the night background. */
export function BrandIcon({ brand, size = 16, className }: { brand: Brand; size?: number; className?: string }) {
  const icon = brand === "web" ? null : ICONS[brand];
  if (!icon) return <Globe width={size} height={size} className={className} aria-label="Web" color="#f4f1ea" />;
  const dark = ["000000", "181717", "121212", "0B0D0E"].includes(icon.hex.toUpperCase());
  return (
    <svg viewBox="0 0 24 24" width={size} height={size} className={className} role="img" aria-label={icon.title} fill={dark ? "#f4f1ea" : `#${icon.hex}`}>
      <path d={icon.path} />
    </svg>
  );
}
