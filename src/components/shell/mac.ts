"use client";

/**
 * zenith.app (scripts/mac/ZenithApp.swift) shows the dashboard in a native window whose
 * title bar is the page's own top edge. The page tells the app when to drag or zoom the
 * window, and the app tells the page about menus and full screen.
 */
type Webkit = { messageHandlers?: { zenithShell?: { postMessage: (m: unknown) => void } } };

export const inMacApp = () => typeof document !== "undefined" && document.documentElement.dataset.shell === "mac";

export function postShell(message: Record<string, unknown>) {
  (window as unknown as { webkit?: Webkit }).webkit?.messageHandlers?.zenithShell?.postMessage(message);
}

/** What a press in a drag region must leave alone. */
const INTERACTIVE = "a,button,input,textarea,select,label,summary,[role=button],[role=link],[role=menuitem],[role=tab],[role=radio],[contenteditable],[data-no-drag]";

/** A mouse down in a `[data-drag]` region, off any control: the window follows it. */
export function dragFromEvent(e: MouseEvent) {
  if (e.button !== 0 || !inMacApp()) return;
  const t = e.target as Element | null;
  if (!t?.closest("[data-drag]") || t.closest(INTERACTIVE)) return;
  postShell({ type: e.detail === 2 ? "zoom" : "drag" });
}
