/**
 * zenith.app, the Mac app (zenith's crates/zenith-app, Tauri), shows this web app with its
 * title bar laid over the page's top bar. There the page leaves room for the traffic lights
 * (`html[data-zenith-app]` in index.css), a press in its top bar moves the window and a
 * double-click zooms it (the app's `shell_drag` / `shell_zoom` commands), and the app's menu
 * opens pages with `zenith:navigate` events. The app signs the page in by itself (its init
 * script on /pair).
 */

import { isZenithAppPath } from "./embed";

interface TauriInternals {
  readonly invoke: (command: string) => Promise<unknown>;
}

declare global {
  interface Window {
    __TAURI_INTERNALS__?: TauriInternals;
  }
}

/** What a press in the title bar must leave alone. */
const DRAG_EXCLUDED =
  "a,button,input,textarea,select,label,summary,[role=button],[role=link],[role=menuitem],[role=tab],[contenteditable],[data-slot=button],[data-slot$=trigger],[draggable=true]";

/** A primary press in the top bar, off its controls: a press in the window's title bar. */
export function isTitleBarPress(event: MouseEvent): boolean {
  const topbar =
    parseFloat(
      getComputedStyle(document.documentElement).getPropertyValue("--workspace-topbar-height"),
    ) || 52;
  const target = event.target instanceof Element ? event.target : null;
  return (
    event.button === 0 &&
    event.clientY <= topbar &&
    target !== null &&
    !target.closest(DRAG_EXCLUDED)
  );
}

/** In zenith.app, set the page up for its window; elsewhere, nothing. */
export function installZenithApp(navigate: (path: string) => void): void {
  const ipc = typeof window === "undefined" ? undefined : window.__TAURI_INTERNALS__;
  if (typeof ipc?.invoke !== "function") return;
  document.documentElement.dataset.zenithApp = "";
  window.addEventListener("mousedown", (event) => {
    if (!isTitleBarPress(event)) return;
    void ipc.invoke(event.detail === 2 ? "shell_zoom" : "shell_drag").catch(() => undefined);
  });
  window.addEventListener("zenith:navigate", (event) => {
    const path = (event as CustomEvent<unknown>).detail;
    if (!isZenithAppPath(path)) return;
    event.preventDefault();
    navigate(path);
  });
}
