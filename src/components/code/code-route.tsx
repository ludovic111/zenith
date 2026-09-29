"use client";

import { useEffect } from "react";
import type { CodeStatus } from "@/lib/code/manager";
import type { CodeTarget } from "@/lib/code/target";
import { codeStore } from "./store";

/** Hands the server's view of /code (status, project to focus) to <CodeHost>. */
export function CodeRoute({ status, target }: { status: CodeStatus; target: CodeTarget | null }) {
  useEffect(() => {
    codeStore.set({ status });
  }, [status]);

  useEffect(() => {
    if (!target) return;
    codeStore.set({ target });
    // The focus request is handed over; the URL follows the app from here.
    const url = new URL(window.location.href);
    url.searchParams.delete("project");
    window.history.replaceState(null, "", url.pathname + url.search);
  }, [target]);

  return null;
}
