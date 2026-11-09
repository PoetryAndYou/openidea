/**
 * Show History for a file: the log tab it opens must query that file's path —
 * a workspace with several working copies otherwise lists every project's
 * commits. Uses the real log store and action; only the IPC layer is stubbed.
 */
import { afterEach, describe, expect, it, vi } from "vitest";
import { registerVcsActions } from "../actions";
import { getAction } from "@/platform/actions";
import { useVcs } from "../store";
import { useGitLog, openLogTab, reloadLog } from "../logStore";

const queries = vi.hoisted(() => [] as Array<{ repo: string | null; query: Record<string, unknown> }>);

vi.mock("@/platform/ipc", () => ({
  commands: {
    vcsLog: (repo: string | null, query: Record<string, unknown>) => {
      queries.push({ repo, query });
      return Promise.resolve([]);
    },
    vcsBranches: () => Promise.resolve([]),
    vcsConfigGet: () => Promise.resolve(null),
    vcsLogRefs: () => Promise.resolve({}),
    vcsOpenExternalLog: () => Promise.resolve(null),
    vcsLogUsers: () => Promise.resolve([]),
  },
  call: (promise: Promise<unknown>) => promise,
  nextRequestId: () => 1,
  isTauri: () => true,
}));

vi.mock("@/platform/workflowSettings", () => ({
  workflowSettings: () => ({ vcs: { limitHistory: false, historyLimit: 100 } }),
}));

afterEach(() => {
  vi.clearAllMocks();
  queries.length = 0;
});

describe("Show History for a file", () => {
  it("repairs a history tab that lost its path (earlier session, path-less query)", async () => {
    const file = "/ws/funauthapp/src/com/acme/Controller.java";
    useVcs.setState({
      root: "/ws",
      repo: "/ws/funauthapp",
      roots: ["/ws/funauthapp", "/ws/emapol"],
      kinds: { "/ws/funauthapp": "svn", "/ws/emapol": "svn" },
    });
    // A stale tab for this file whose filter carries no path — the state a
    // restored tab can end up in.
    openLogTab("history", file);
    useGitLog.setState({ filters: { ...useGitLog.getState().filters, path: null }, commits: [] });
    queries.length = 0;
    const unregister = registerVcsActions();
    try {
      try {
        await getAction("vcs.showHistory")!.run({ arg: [file], ctx: { vcsAvailable: true } });
      } catch {
        /* no workbench in this test */
      }
      expect(useGitLog.getState().filters.path).toBe(file);
      await reloadLog({});
      expect(queries.at(-1)!.query.path).toBe(file);
    } finally {
      unregister();
    }
  });

  it("queries the file's path, not the whole workspace", async () => {
    const file = "/ws/funauthapp/src/com/acme/Controller.java";
    useVcs.setState({
      root: "/ws",
      repo: "/ws/funauthapp",
      roots: ["/ws/funauthapp", "/ws/emapol"],
      kinds: { "/ws/funauthapp": "svn", "/ws/emapol": "svn" },
    });
    const unregister = registerVcsActions();
    try {
      // The action opens the tab first, then shows the tool window (which needs
      // the workbench, absent here): the UI step is allowed to throw.
      try {
        await getAction("vcs.showHistory")!.run({ arg: [file], ctx: { vcsAvailable: true } });
      } catch {
        /* no workbench in this test */
      }
      // The tab carries the file filter…
      const state = useGitLog.getState();
      expect(state.filters.path).toBe(file);
      // …and the query sent to the backend names the file.
      await reloadLog({});
      const svnQueries = queries.filter((q) => q.query.path !== undefined && q.query.path !== null);
      expect(svnQueries.length).toBeGreaterThan(0);
      expect(svnQueries.at(-1)!.query.path).toBe(file);
    } finally {
      unregister();
    }
  });
});
