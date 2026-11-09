/**
 * "Set Revision Properties…": resolves the revision that last changed the file,
 * loads its message, and hands it to the dialog. Exercised through the real
 * action registry with the IPC layer stubbed, so the wiring is covered, not
 * just the Subversion client.
 */
import { afterEach, describe, expect, it, vi } from "vitest";
import { getAction } from "@/platform/actions";
import { registerVcsActions } from "../actions";

const mocks = vi.hoisted(() => ({
  fileRevision: vi.fn(),
  logDetails: vi.fn(),
  setMessage: vi.fn(),
  show: vi.fn(),
}));

vi.mock("@/platform/ipc", () => ({
  commands: {
    vcsSvnFileRevision: (path: string) => mocks.fileRevision(path),
    vcsLogDetails: (repo: string | null, revision: string) => mocks.logDetails(repo, revision),
    vcsSvnSetMessage: (repo: string | null, revision: string, message: string) =>
      mocks.setMessage(repo, revision, message),
  },
  call: (promise: Promise<unknown>) => promise,
  isTauri: () => true,
}));

vi.mock("../SvnMessageDialog", () => ({
  showSvnMessage: (revision: string, message: string, repo: string | null, onSaved: () => void) => {
    mocks.show(revision, message, repo);
    onSaved();
  },
}));

vi.mock("../logStore", () => ({
  openLogTab: vi.fn(),
  reloadLog: () => Promise.resolve(),
  revealCommit: vi.fn(),
  selectGitTab: vi.fn(),
  useGitLog: { getState: () => ({ logTabs: [], shownLog: "log", filters: {} }) },
}));

afterEach(() => vi.clearAllMocks());

describe("Set Revision Properties…", () => {
  it("reads the file's last-changed revision and its message", async () => {
    mocks.fileRevision.mockResolvedValue("r7");
    mocks.logDetails.mockResolvedValue({ message: "old message" });
    const unregister = registerVcsActions();
    try {
      const action = getAction("vcs.svnSetMessage")!;
      await action.run({ arg: ["/ws/projB/src/Main.java"], ctx: { vcsAvailable: true } });
      expect(mocks.fileRevision).toHaveBeenCalledWith("/ws/projB/src/Main.java");
      expect(mocks.logDetails).toHaveBeenCalledWith(null, "r7");
      expect(mocks.show).toHaveBeenCalledWith("r7", "old message", null);
    } finally {
      unregister();
    }
  });
});
