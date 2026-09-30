"use client";

import { useRouter } from "next/navigation";
import { useEffect } from "react";
import { openAsk } from "@/components/agent/client";
import { dragFromEvent } from "./mac";
import { toggleSidebar } from "./sidebar-state";

/**
 * Window-wide shortcuts (⌘B sidebar, ⌘, settings) and, in zenith.app, the native side:
 * dragging the window by its title bars, and the app's menus (they arrive as
 * `zenith:menu` events, see scripts/mac/ZenithApp.swift).
 */
export function ShellEvents() {
  const router = useRouter();
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!(e.metaKey || e.ctrlKey) || e.altKey || e.shiftKey) return;
      const t = e.target as HTMLElement | null;
      const typing = t?.isContentEditable || t?.tagName === "INPUT" || t?.tagName === "TEXTAREA";
      if (e.key === "b" && !typing) {
        e.preventDefault();
        toggleSidebar();
      } else if (e.key === ",") {
        e.preventDefault();
        router.push("/reglages");
      }
    };
    const onMenu = (e: Event) => {
      const action = (e as CustomEvent<string>).detail;
      if (action === "settings") router.push("/reglages");
      else if (action === "sidebar") toggleSidebar();
      else if (action === "search") window.dispatchEvent(new Event("zenith:command"));
      else if (action === "ask") openAsk();
      else if (action === "home") router.push("/");
      else if (action === "refresh") router.refresh();
    };
    window.addEventListener("keydown", onKey);
    window.addEventListener("mousedown", dragFromEvent);
    window.addEventListener("zenith:menu", onMenu);
    return () => {
      window.removeEventListener("keydown", onKey);
      window.removeEventListener("mousedown", dragFromEvent);
      window.removeEventListener("zenith:menu", onMenu);
    };
  }, [router]);
  return null;
}
