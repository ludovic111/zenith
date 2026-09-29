import { scopeProjectRef, scopeThreadRef } from "@t3tools/client-runtime/environment";
import { useRouter } from "@tanstack/react-router";
import { useEffect, useRef, useState } from "react";

import { useNewThreadHandler } from "../hooks/useHandleNewThread";
import {
  readThreadShells,
  useAllEnvironmentProjectSnapshotsReady,
  useProjects,
} from "../state/entities";
import { buildThreadRouteParams } from "../threadRoutes";
import {
  clearPendingProjectFocus,
  isEmbedded,
  isTrustedParentMessage,
  postToZenith,
  readPendingProjectFocus,
  sameWorkspacePath,
  setPendingProjectFocus,
  ZENITH_MESSAGE,
  zenithParentOrigins,
} from "./embed";

// Projects registered by zenith at startup may land a moment after the snapshot.
const PROJECT_WAIT_MS = 10_000;

/**
 * Mounted in the authenticated shell. Opens the project zenith asked for (via
 * `?zenithProject=` or a `zenith-code:open-project` message): its most recent
 * thread, or a new draft when it has none.
 */
export function ZenithEmbedCoordinator() {
  const router = useRouter();
  const projects = useProjects();
  const snapshotsReady = useAllEnvironmentProjectSnapshotsReady();
  const handleNewThread = useNewThreadHandler();
  const [pending, setPending] = useState<string | null>(() => readPendingProjectFocus());
  const [retry, setRetry] = useState(0);
  const firstSeenAt = useRef<number | null>(null);

  // Live requests from the parent page, without reloading the iframe.
  useEffect(() => {
    if (!isEmbedded()) return;
    let origins: ReadonlyArray<string> = [];
    let disposed = false;
    const onMessage = (event: MessageEvent) => {
      if (!isTrustedParentMessage(event, origins)) return;
      const { type, path } = event.data;
      if (type === ZENITH_MESSAGE.openProject && typeof path === "string" && path.length > 0) {
        setPendingProjectFocus(path);
        firstSeenAt.current = null;
        setPending(path);
      }
    };
    void zenithParentOrigins().then((allowed) => {
      if (disposed) return;
      origins = allowed;
      window.addEventListener("message", onMessage);
      postToZenith({ type: ZENITH_MESSAGE.ready }, allowed);
    });
    return () => {
      disposed = true;
      window.removeEventListener("message", onMessage);
    };
  }, []);

  useEffect(() => {
    if (!pending || !snapshotsReady) return;
    const project = projects.find((candidate) =>
      sameWorkspacePath(candidate.workspaceRoot, pending),
    );
    if (!project) {
      firstSeenAt.current ??= Date.now();
      const waited = Date.now() - firstSeenAt.current;
      if (waited < PROJECT_WAIT_MS) {
        const timer = window.setTimeout(() => setRetry((count) => count + 1), 1_000);
        return () => window.clearTimeout(timer);
      }
      clearPendingProjectFocus();
      setPending(null);
      return;
    }

    clearPendingProjectFocus();
    setPending(null);
    const latest = readThreadShells()
      .filter(
        (thread) =>
          thread.environmentId === project.environmentId &&
          thread.projectId === project.id &&
          thread.archivedAt === null,
      )
      .toSorted((a, b) => b.updatedAt.localeCompare(a.updatedAt))[0];
    if (latest) {
      void router.navigate({
        to: "/$environmentId/$threadId",
        params: buildThreadRouteParams(scopeThreadRef(latest.environmentId, latest.id)),
      });
    } else {
      void handleNewThread(scopeProjectRef(project.environmentId, project.id));
    }
  }, [pending, retry, snapshotsReady, projects, router, handleNewThread]);

  return null;
}
