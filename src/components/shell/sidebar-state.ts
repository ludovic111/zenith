"use client";

import { useSyncExternalStore } from "react";
import { readPref, subscribePrefs, writePref } from "@/lib/prefs";

/**
 * The sidebar: shown or hidden (⌘B) on wide screens, kept per browser and applied before
 * paint (layout.tsx); a drawer on narrow ones, open only while you use it.
 */
const KEY = "zenith:sidebar";

export function useSidebarHidden() {
  return useSyncExternalStore(subscribePrefs, () => readPref(KEY) === "hidden", () => false);
}

export function toggleSidebar() {
  if (!window.matchMedia("(min-width: 1024px)").matches) return setDrawer(!drawerOpen);
  const hide = readPref(KEY) !== "hidden";
  if (hide) document.documentElement.dataset.sidebar = "hidden";
  else delete document.documentElement.dataset.sidebar;
  writePref(KEY, hide ? "hidden" : "shown");
}

let drawerOpen = false;
const drawerListeners = new Set<() => void>();

export function setDrawer(open: boolean) {
  drawerOpen = open;
  for (const l of drawerListeners) l();
}

export function useDrawer() {
  return useSyncExternalStore(
    (l) => {
      drawerListeners.add(l);
      return () => drawerListeners.delete(l);
    },
    () => drawerOpen,
    () => false,
  );
}
