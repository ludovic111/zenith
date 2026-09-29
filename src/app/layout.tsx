import type { Metadata } from "next";
import { Geist, Instrument_Serif, JetBrains_Mono, Unbounded } from "next/font/google";
import { Sky } from "@/components/shell/sky";
import { Sidebar } from "@/components/shell/sidebar";
import { CommandMenu } from "@/components/shell/command-menu";
import { AutoRefresh } from "@/components/shell/auto-refresh";
import { L10nProvider } from "@/components/shell/l10n-provider";
import { config } from "@/lib/config";
import { l10n, tr } from "@/lib/i18n";
import { PROJECTS } from "@/lib/projects";
import "./globals.css";

const body = Geist({ variable: "--font-body", subsets: ["latin"] });
const mono = JetBrains_Mono({ variable: "--font-mono-face", subsets: ["latin"] });
const display = Unbounded({ variable: "--font-display-face", subsets: ["latin"], weight: ["400", "500", "700", "900"] });
const serif = Instrument_Serif({ variable: "--font-serif-face", subsets: ["latin"], weight: "400", style: ["normal", "italic"] });

export function generateMetadata(): Metadata {
  return {
    title: { default: "zenith", template: "%s · zenith" },
    description: tr("Tout ce qui brille au-dessus de mes projets.", "Everything that shines above my projects."),
  };
}

export default function RootLayout({ children }: { children: React.ReactNode }) {
  const c = config();
  const loc = l10n();
  const nav = PROJECTS.map(({ id, name, href, color, glow, emoji, tagline }) => ({ id, name, href, color, glow, emoji, tagline }));
  const links = PROJECTS.flatMap((p) => p.links.map((l) => ({ ...l, project: p.name, color: p.color })));
  return (
    <html lang={loc.locale} className={`${body.variable} ${mono.variable} ${display.variable} ${serif.variable} h-full antialiased`}>
      <body className="min-h-full">
        <L10nProvider value={loc}>
          <Sky />
          <div className="relative z-10 flex min-h-screen">
            <Sidebar projects={nav} code={c.code.enabled} />
            <main className="min-w-0 flex-1 px-4 pb-16 pt-4 sm:px-6 lg:px-10 lg:pt-8">{children}</main>
          </div>
          <CommandMenu projects={nav} links={links} code={c.code.enabled} />
          <AutoRefresh seconds={60} />
        </L10nProvider>
      </body>
    </html>
  );
}
