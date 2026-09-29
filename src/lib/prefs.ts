"use client";

/**
 * Small per-browser preferences in localStorage (sidebar state, assistant mode). Read them
 * with useSyncExternalStore(subscribePrefs, () => readPref(key)); a change in this tab is
 * announced with an event, one in another tab by "storage".
 */
const PREFS_EVENT = "zenith:prefs";

export function subscribePrefs(onChange: () => void) {
  window.addEventListener(PREFS_EVENT, onChange);
  window.addEventListener("storage", onChange);
  return () => {
    window.removeEventListener(PREFS_EVENT, onChange);
    window.removeEventListener("storage", onChange);
  };
}

export function readPref(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

export function writePref(key: string, value: string) {
  try {
    localStorage.setItem(key, value);
  } catch {}
  window.dispatchEvent(new Event(PREFS_EVENT));
}
