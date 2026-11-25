/**
 * Accept Yours / Accept Theirs for a Subversion conflict: Subversion's own
 * `svn resolve --accept mine-full | theirs-full` decides — the file is never
 * deleted from here. Rebuilding a side out of `.mine` / `.rN` once deleted a
 * working file whose artifacts could not be read.
 */
import { afterEach, describe, expect, it, vi } from "vitest";
import type { FileStatus } from "@/bindings";

const mocks = vi.hoisted(() => ({
  resolve: vi.fn(),
  conflictVersions: vi.fn(),
  stage: vi.fn(),
  deleteFile: vi.fn(),
  refresh: vi.fn(),
  notify: vi.fn(),
}));

vi.mock("@/platform/ipc", () => ({
  commands: {
    vcsSvnResolve: (paths: string[], accept: string | null) => mocks.resolve(paths, accept),
    vcsConflictVersions: (path: string) => mocks.conflictVersions(path),
    vcsStage: (paths: string[]) => mocks.stage(paths),
    fsDelete: (path: string) => mocks.deleteFile(path),
  },
  call: (promise: Promise<unknown>) => promise,
  isTauri: () => true,
}));

vi.mock("../store", async () => {
  const { create } = await import("zustand");
  return {
    useVcs: create(() => ({})),
    refreshNow: mocks.refresh,
    beginGitWork: () => () => {},
    conflictedFiles: () => [],
    repoOf: () => "/ws/projA",
    vcsKind: () => "svn",
  };
});

vi.mock("@/workbench/contributions", () => ({
  editorService: () => ({ openFile: vi.fn(), closeInput: vi.fn() }),
  notifications: () => ({ notify: mocks.notify }),
}));

import { acceptSide } from "../conflictsUi";

const conflicted = (path: string): FileStatus =>
  ({ path, relPath: path, staged: null, unstaged: "conflicted", renamedFrom: null }) as FileStatus;

afterEach(() => {
  vi.clearAllMocks();
  mocks.resolve.mockResolvedValue(undefined);
  mocks.conflictVersions.mockResolvedValue({ base: null, ours: null, theirs: null });
  mocks.stage.mockResolvedValue(undefined);
});

describe("Accept Yours / Accept Theirs in a Subversion working copy", () => {
  it("lets svn resolve restore the incoming side", async () => {
    await acceptSide(conflicted("/ws/projA/src/f.txt"), "theirs");
    expect(mocks.resolve).toHaveBeenCalledWith(["/ws/projA/src/f.txt"], "theirs-full");
    // The side files are never consulted, so a missing one cannot delete the file.
    expect(mocks.conflictVersions).not.toHaveBeenCalled();
    expect(mocks.deleteFile).not.toHaveBeenCalled();
    expect(mocks.stage).not.toHaveBeenCalled();
  });

  it("lets svn resolve restore the local side", async () => {
    await acceptSide(conflicted("/ws/projA/src/f.txt"), "ours");
    expect(mocks.resolve).toHaveBeenCalledWith(["/ws/projA/src/f.txt"], "mine-full");
    expect(mocks.deleteFile).not.toHaveBeenCalled();
  });
});
