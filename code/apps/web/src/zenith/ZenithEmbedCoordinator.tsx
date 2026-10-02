import {
  scopedThreadKey,
  scopeProjectRef,
  scopeThreadRef,
} from "@t3tools/client-runtime/environment";
import type { EnvironmentThreadShell } from "@t3tools/client-runtime/state/models";
import { effectiveSnoozed } from "@t3tools/client-runtime/state/thread-settled";
import { sortSettledThreads } from "@t3tools/client-runtime/state/thread-sort";
import type { EnvironmentId, ProjectId, ThreadId } from "@t3tools/contracts";
import { useLocation, useParams, useRouter } from "@tanstack/react-router";
import { useEffect, useEffectEvent, useRef, useState } from "react";

import { openCommandPalette } from "../commandPaletteBus";
import {
  resolveThreadStatusPill,
  sortPinnedThreadsForSidebar,
  sortThreadsForSidebar,
} from "../components/Sidebar.logic";
import { useNewThreadHandler } from "../hooks/useHandleNewThread";
import {
  readThreadShells,
  useAllEnvironmentProjectSnapshotsReady,
  useProjects,
  useServerConfigs,
  useThreadShells,
} from "../state/entities";
import { buildThreadRouteParams, resolveThreadRouteRef } from "../threadRoutes";
import { useUiStateStore } from "../uiStateStore";
import {
  clearPendingProjectFocus,
  isEmbedded,
  isTitleBarPress,
  isTrustedParentMessage,
  parseNavigateRequest,
  postToZenith,
  readPendingProjectFocus,
  sameWorkspacePath,
  setOwnSidebar,
  setPendingProjectFocus,
  ZENITH_MESSAGE,
  zenithParentOrigins,
  type ZenithNavigateRequest,
  type ZenithSidebarSnapshot,
  type ZenithSidebarThread,
  type ZenithThreadSection,
  type ZenithThreadStatus,
} from "./embed";

// Projects registered by zenith at startup may land a moment after the snapshot.
const PROJECT_WAIT_MS = 10_000;
// Coalesce bursts of shell events (a running turn updates its thread often).
const SNAPSHOT_DEBOUNCE_MS = 250;

/**
 * Mounted in the authenticated shell. Opens the project zenith asked for (via
 * `?zenithProject=` or a `zenith-code:open-project` message): its most recent
 * thread, or a new draft when it has none. Also follows zenith's navigation
 * requests and publishes the thread list zenith's sidebar shows.
 */
export function ZenithEmbedCoordinator() {
  const router = useRouter();
  const projects = useProjects();
  const snapshotsReady = useAllEnvironmentProjectSnapshotsReady();
  const handleNewThread = useNewThreadHandler();
  const [pending, setPending] = useState<string | null>(() => readPendingProjectFocus());
  const [retry, setRetry] = useState(0);
  const [origins, setOrigins] = useState<ReadonlyArray<string> | null>(null);
  const firstSeenAt = useRef<number | null>(null);

  const navigate = useEffectEvent((request: ZenithNavigateRequest) => {
    switch (request.to) {
      case "thread":
        void router.navigate({
          to: "/$environmentId/$threadId",
          params: buildThreadRouteParams(
            scopeThreadRef(request.environmentId as EnvironmentId, request.threadId as ThreadId),
          ),
        });
        return;
      case "new-thread":
        void handleNewThread(
          scopeProjectRef(request.environmentId as EnvironmentId, request.projectId as ProjectId),
        );
        return;
      case "path":
        if (router.state.location.pathname !== request.path) {
          void router.navigate({ href: request.path });
        }
        return;
      case "palette":
        openCommandPalette(request.open ? { open: request.open } : undefined);
        return;
    }
  });

  // Live requests from the parent page, without reloading the iframe.
  useEffect(() => {
    if (!isEmbedded()) return;
    let allowed: ReadonlyArray<string> = [];
    let disposed = false;
    const onMessage = (event: MessageEvent) => {
      if (!isTrustedParentMessage(event, allowed)) return;
      const { type, path, ownSidebar, request, insetLeft } = event.data;
      if (type === ZENITH_MESSAGE.openProject && typeof path === "string" && path.length > 0) {
        setPendingProjectFocus(path);
        firstSeenAt.current = null;
        setPending(path);
      } else if (type === ZENITH_MESSAGE.chrome && typeof ownSidebar === "boolean") {
        setOwnSidebar(ownSidebar);
        // Room for the parent window's controls when its sidebar is hidden.
        const inset = typeof insetLeft === "number" && insetLeft > 0 ? Math.min(insetLeft, 160) : 0;
        if (inset) {
          document.documentElement.style.setProperty(
            "--workspace-controls-left",
            `calc(${inset}px + 0.75rem)`,
          );
        } else {
          document.documentElement.style.removeProperty("--workspace-controls-left");
        }
      } else if (type === ZENITH_MESSAGE.navigate) {
        const parsed = parseNavigateRequest(request);
        if (parsed) navigate(parsed);
      }
    };
    // The top bar is the parent window's title bar: a press there, off any control,
    // lets the parent move (or, double-clicked, zoom) its window.
    const onMouseDown = (event: MouseEvent) => {
      if (!isTitleBarPress(event)) return;
      postToZenith({ type: ZENITH_MESSAGE.drag, zoom: event.detail === 2 }, allowed);
    };
    void zenithParentOrigins().then((resolved) => {
      if (disposed) return;
      allowed = resolved;
      window.addEventListener("message", onMessage);
      window.addEventListener("mousedown", onMouseDown);
      setOrigins(resolved);
      postToZenith({ type: ZENITH_MESSAGE.ready }, resolved);
    });
    return () => {
      disposed = true;
      window.removeEventListener("message", onMessage);
      window.removeEventListener("mousedown", onMouseDown);
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

  return origins && origins.length > 0 ? <ZenithSidebarPublisher origins={origins} /> : null;
}

const STATUS_BY_PILL_LABEL: Record<string, ZenithThreadStatus> = {
  "Pending Approval": "approval",
  "Awaiting Input": "input",
  Working: "working",
  Connecting: "connecting",
  "Plan Ready": "plan",
  Monitoring: "monitoring",
  Completed: "completed",
};

function threadStatus(
  thread: EnvironmentThreadShell,
  lastVisitedAt: string | undefined,
): ZenithThreadStatus | null {
  const pill = resolveThreadStatusPill({ thread: { ...thread, lastVisitedAt } });
  if (pill) return STATUS_BY_PILL_LABEL[pill.label] ?? null;
  return thread.session?.status === "error" ? "failed" : null;
}

/** Posts the projects and threads, in the order the app's own sidebar uses. */
function ZenithSidebarPublisher({ origins }: { origins: ReadonlyArray<string> }) {
  const projects = useProjects();
  const threads = useThreadShells();
  const serverConfigs = useServerConfigs();
  const lastVisitedById = useUiStateStore((state) => state.threadLastVisitedAtById);
  const pathname = useLocation({ select: (location) => location.pathname });
  const activeRef = useParams({ strict: false, select: (params) => resolveThreadRouteRef(params) });
  const activeThread = activeRef ? scopedThreadKey(activeRef) : null;
  // Snoozed threads wake on the clock, not on an event.
  const [minute, setMinute] = useState(() => new Date().toISOString());
  const lastPosted = useRef<string | null>(null);

  useEffect(() => {
    const timer = window.setInterval(() => setMinute(new Date().toISOString()), 60_000);
    return () => window.clearInterval(timer);
  }, []);

  useEffect(() => {
    const timer = window.setTimeout(() => {
      const now = minute;
      const buckets: Record<ZenithThreadSection, EnvironmentThreadShell[]> = {
        pinned: [],
        active: [],
        snoozed: [],
        settled: [],
      };
      for (const thread of threads) {
        if (thread.archivedAt !== null) continue;
        const capabilities = serverConfigs.get(thread.environmentId)?.environment.capabilities;
        if (capabilities?.threadSnooze === true && effectiveSnoozed(thread, { now })) {
          buckets.snoozed.push(thread);
        } else if (
          capabilities?.threadSettlement === true &&
          thread.settledOverride === "settled"
        ) {
          buckets.settled.push(thread);
        } else if (thread.pinnedAt != null) {
          buckets.pinned.push(thread);
        } else {
          buckets.active.push(thread);
        }
      }
      const ordered: Array<[ZenithThreadSection, ReadonlyArray<EnvironmentThreadShell>]> = [
        ["pinned", sortPinnedThreadsForSidebar(buckets.pinned)],
        ["active", sortThreadsForSidebar(buckets.active)],
        [
          "snoozed",
          buckets.snoozed.toSorted((a, b) =>
            (a.snoozedUntil ?? "").localeCompare(b.snoozedUntil ?? ""),
          ),
        ],
        ["settled", sortSettledThreads(buckets.settled)],
      ];
      const snapshotThreads: ZenithSidebarThread[] = ordered.flatMap(([section, list]) =>
        list.map((thread) => ({
          environmentId: thread.environmentId,
          id: thread.id,
          projectId: thread.projectId,
          title: thread.title,
          branch: thread.branch,
          section,
          status: threadStatus(
            thread,
            lastVisitedById[scopedThreadKey(scopeThreadRef(thread.environmentId, thread.id))],
          ),
          activityAt: thread.latestUserMessageAt ?? thread.updatedAt,
        })),
      );
      const snapshot: ZenithSidebarSnapshot = {
        projects: projects.map((project) => ({
          environmentId: project.environmentId,
          id: project.id,
          title: project.title,
          workspaceRoot: project.workspaceRoot,
        })),
        threads: snapshotThreads,
        activeThread,
        pathname,
      };
      const serialized = JSON.stringify(snapshot);
      if (serialized === lastPosted.current) return;
      lastPosted.current = serialized;
      postToZenith({ type: ZENITH_MESSAGE.sidebar, snapshot }, origins);
    }, SNAPSHOT_DEBOUNCE_MS);
    return () => window.clearTimeout(timer);
  }, [projects, threads, serverConfigs, lastVisitedById, pathname, activeThread, minute, origins]);

  return null;
}
