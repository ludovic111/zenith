import type { Metadata, Viewport } from "next";
import { Sidebar } from "@/components/shell/sidebar";
import { TitleBar } from "@/components/shell/title-bar";
import { ShellEvents } from "@/components/shell/shell-events";
import { CommandMenu } from "@/components/shell/command-menu";
import { AutoRefresh } from "@/components/shell/auto-refresh";
import { L10nProvider } from "@/components/shell/l10n-provider";
import { config } from "@/lib/config";
import { l10n, tr } from "@/lib/i18n";
import { PROJECTS, projectDir } from "@/lib/projects";
import { EXTENSIONS } from "@/lib/extensions";
import { CodeHost } from "@/components/code/code-host";
import { AskDialog } from "@/components/agent/ask-dialog";
import { SUGGESTIONS, agentUi } from "@/lib/agent/ui";
import "./globals.css";

export function generateMetadata(): Metadata {
  return {
    title: { default: "zenith", template: "%s · zenith" },
    description: tr("Tes projets et ta journée, en un coup d'œil.", "Your projects and your day, at a glance."),
  };
}

export const viewport: Viewport = {
  themeColor: [
    { media: "(prefers-color-scheme: light)", color: "#fcfcfc" },
    { media: "(prefers-color-scheme: dark)", color: "#0a0a0a" },
  ],
};

// Before paint: inside zenith.app (its user agent), and the sidebar hidden with ⌘B.
const BOOT = `try{var d=document.documentElement;if(/ZenithMac\\//.test(navigator.userAgent))d.dataset.shell="mac";if(localStorage.getItem("zenith:sidebar")==="hidden")d.dataset.sidebar="hidden"}catch(e){}`;

export default function RootLayout({ children }: { children: React.ReactNode }) {
  const c = config();
  const loc = l10n();
  const nav = PROJECTS.map(({ id, name, href, color, tagline, dir }) => ({ id, name, href, color, tagline, dir: projectDir({ dir }) }));
  const links = PROJECTS.flatMap((p) => p.links.map((l) => ({ ...l, project: p.name, color: p.color })));
  const titles: Record<string, string[]> = {};
  for (const e of EXTENSIONS) for (const p of e.pages ?? []) titles[`/${p.slug}`] = [p.title];
  for (const p of PROJECTS) titles[p.href] = [tr("Projets", "Projects"), p.name];
  const ui = agentUi();
  const agent = ui.enabled ? { targets: ui.targets, provider: ui.provider } : null;
  return (
    <html lang={loc.locale} className="h-full" suppressHydrationWarning>
      <head>
        <script dangerouslySetInnerHTML={{ __html: BOOT }} />
      </head>
      <body className="h-full overflow-hidden">
        <L10nProvider value={loc}>
          <div className="flex h-dvh">
            <Sidebar projects={nav} code={c.code.enabled} home={process.cwd()} agent={agent ? { home: ui.home, name: ui.name, avatar: ui.avatar, bots: ui.bots } : null} />
            <div className="flex min-w-0 flex-1 flex-col bg-background">
              <main className="relative min-h-0 flex-1 overflow-y-auto">
                <TitleBar titles={titles} />
                <div className="mx-auto w-full max-w-6xl px-4 pb-16 pt-6 sm:px-6 lg:px-8">{children}</div>
              </main>
            </div>
          </div>
          <CodeHost />
          <CommandMenu projects={nav} links={links} code={c.code.enabled} agent={agent} />
          {agent && <AskDialog targets={agent.targets} provider={agent.provider} suggestions={SUGGESTIONS()} />}
          <ShellEvents />
          <AutoRefresh seconds={60} />
        </L10nProvider>
      </body>
    </html>
  );
}
