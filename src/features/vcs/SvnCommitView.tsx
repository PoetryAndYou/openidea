/**
 * SVN commit panel — a single-pane variant of the Git commit interface.
 * SVN has no index: the checkbox selection is what gets committed, via
 * `svn commit <paths> -m`. Grouped like the Git panel (Changes / Unversioned
 * files) with the product's CheckMark boxes, file-type icons and
 * basename-first rows; double-click previews the working diff against BASE.
 */
import { useEffect, useMemo, useState } from "react";
import clsx from "clsx";
import { currentRepoRoot, scheduleRefresh, useVcs, vcsKind } from "./store";
import { effectiveKind, KIND_COLOR, type EffectiveKind } from "./model";
import { call, commands } from "@/platform/ipc";
import { basename, dirname } from "@/platform/project";
import { Icon, T, fileIconName } from "@/ui";
import { tx } from "@/platform/i18n";
import { deleteFiles } from "./changeOps";
import { openDiff, withChain, type DiffData } from "./diffInput";
import { showSvnRevert } from "./SvnRevertDialog";
import type { FileStatus } from "@/bindings";
import styles from "./SvnCommitView.module.css";

/** Working diff against BASE: added/unversioned files have no BASE side. */
export function svnDiffData(path: string, kind: EffectiveKind | null): DiffData {
  const none: DiffData["left"] = { type: "text", text: "", label: "(does not exist)" };
  const left: DiffData["left"] =
    kind === "untracked" || kind === "added" ? none : { type: "rev", rev: "BASE", label: "BASE" };
  const right: DiffData["right"] =
    kind === "deleted" ? { type: "text", text: "", label: "(deleted)" } : { type: "worktree" };
  return { path, left, right };
}

/** Directory shown after a file name: its parent relative to the working copy. */
function relativeDir(repo: string | null, path: string): string {
  const rel = repo && path.startsWith(`${repo}/`) ? path.slice(repo.length + 1) : null;
  const target = rel ?? path;
  const dir = dirname(target);
  return dir === "." || dir === "/" || dir === "" || dir === target ? "" : dir.replace(/^\/+/, "");
}

/** The inclusion box, identical to the Git panel's. */
function CheckMark({
  checked,
  indeterminate,
  label,
  onToggle,
}: {
  checked: boolean;
  indeterminate: boolean;
  label: string;
  onToggle: () => void;
}) {
  return (
    <span
      role="checkbox"
      aria-checked={indeterminate ? "mixed" : checked}
      aria-label={label}
      className={clsx(styles.check, (checked || indeterminate) && styles.checkOn)}
      onMouseDown={(e) => {
        if (e.button !== 0) return;
        e.stopPropagation();
        e.preventDefault();
        onToggle();
      }}
      onDoubleClick={(e) => e.stopPropagation()}
    >
      {indeterminate ? <Icon name="minus" size={10} /> : checked ? <Icon name="check" size={10} /> : null}
    </span>
  );
}

interface Row {
  /** The status entry the row renders, for the revert dialog. */
  src: FileStatus;
  path: string;
  relPath: string;
  kind: EffectiveKind;
  isAdd: boolean;
  checked: boolean;
}

export function SvnCommitView() {
  const status = useVcs((s) => s.status);
  const storeError = useVcs((s) => s.error);
  const repo = currentRepoRoot();
  const svn = vcsKind(repo) === "svn";
  const [checked, setChecked] = useState<Record<string, boolean>>({});
  // Click selects; Ctrl/Cmd-click extends — the rows the Revert button acts on.
  const [selectedFiles, setSelectedFiles] = useState<ReadonlySet<string>>(new Set());
  const [collapsed, setCollapsed] = useState<Record<string, boolean>>({});
  const [message, setMessage] = useState("");
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  // The workspace's working copies, to group changes per project folder.
  const [copies, setCopies] = useState<string[]>([]);
  const rootsVersion = useVcs((s) => s.roots.length);
  useEffect(() => {
    let alive = true;
    void call(commands.vcsSvnRoots())
      .then((roots) => {
        if (alive) setCopies(roots.map((r) => `${r}`).filter((r) => !r.endsWith("/")));
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [rootsVersion]);

  const rows = useMemo<Row[]>(
    () =>
      status
        .filter((f) => effectiveKind(f) !== "ignored")
        .map((f) => {
          const kind = effectiveKind(f) as EffectiveKind;
          return {
            src: f,
            path: f.path,
            relPath: f.relPath,
            kind,
            isAdd: f.staged === "added",
            checked: checked[f.path] ?? (kind !== "untracked" && kind !== "ignored"),
          };
        }),
    [status, checked],
  );

  const tracked = rows.filter((r) => r.kind !== "untracked");
  const untracked = rows.filter((r) => r.kind === "untracked");
  const selected = rows.filter((r) => r.checked).map((r) => r.path);

  const openPreview = (r: Row) => {
    const items = rows.map((row) => svnDiffData(row.path, row.kind));
    void openDiff(withChain(items, Math.max(0, rows.indexOf(r))), { preview: true });
  };

  const run = async (label: string, f: () => Promise<unknown>) => {
    setBusy(label);
    setError(null);
    try {
      await f();
      setChecked({});
      setMessage("");
      scheduleRefresh(50);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(null);
    }
  };

  if (!svn) return null;

  const toggleGroup = (list: Row[], on: boolean) => {
    setChecked((c) => {
      const next = { ...c };
      for (const r of list) next[r.path] = on;
      return next;
    });
  };

  const clickRow = (r: Row, extend: boolean) => {
    setSelectedFiles((current) => {
      if (!extend) return new Set([r.path]);
      const next = new Set(current);
      if (next.has(r.path)) next.delete(r.path);
      else next.add(r.path);
      return next;
    });
  };

  /** Revert through the dialog: the rows it acts on are the multi-selection (this
   * row included), each still individually checkable off, and nothing is rolled
   * back before the dialog's Revert confirms it. */
  const confirmRevert = (r: Row) => {
    const targets =
      selectedFiles.has(r.path) && selectedFiles.size > 1
        ? rows.filter((row) => selectedFiles.has(row.path))
        : [r];
    showSvnRevert(targets.map((row) => row.src), (paths) =>
      run("revert", () => call(commands.vcsSvnRevert(paths))),
    );
  };

  const renderRow = (r: Row, root: string | null) => {
    const dir = relativeDir(root ?? repo, r.path);
    return (
      <div
        key={r.path}
        className={clsx(styles.row, styles.fileRow, selectedFiles.has(r.path) && styles.rowSelected)}
        onClick={(e) => clickRow(r, e.metaKey || e.ctrlKey)}
        onDoubleClick={() => openPreview(r)}
      >
        <CheckMark
          checked={r.checked}
          indeterminate={false}
          label={`Include ${r.relPath} in commit`}
          onToggle={() => setChecked((c) => ({ ...c, [r.path]: !r.checked }))}
        />
        <Icon name={fileIconName(basename(r.path), r.path)} size={16} className={styles.icon} />
        <span className={styles.name} style={{ color: KIND_COLOR[r.kind] }} title={r.path}>
          {basename(r.path)}
        </span>
        <span className={styles.dir} title={dir}>
          {dir}
        </span>
        <span className={styles.links}>
          {!r.isAdd && r.kind === "untracked" && (
            <button
              className={styles.link}
              onClick={(e) => {
                e.stopPropagation();
                void run("add", () => call(commands.vcsSvnAdd([r.path])));
              }}
            >
              {tx("Add")}
            </button>
          )}
          <button
            className={styles.link}
            title={
              selectedFiles.has(r.path) && selectedFiles.size > 1
                ? tx`Revert ${selectedFiles.size} selected files…`
                : undefined
            }
            onClick={(e) => {
              e.stopPropagation();
              // Unversioned: it exists only in the working tree, so rolling it back
              // means removing it; everything else goes through the dialog first.
              if (r.kind === "untracked" && !(selectedFiles.has(r.path) && selectedFiles.size > 1)) {
                void deleteFiles([r.path]);
              } else {
                confirmRevert(r);
              }
            }}
          >
            {tx("Revert")}
          </button>
        </span>
      </div>
    );
  };

  const group = (key: string, title: string, list: Row[]) => {
    if (list.length === 0) return null;
    const all = list.every((r) => r.checked);
    const some = list.some((r) => r.checked);
    const open = !collapsed[key];
    return (
      <div className={styles.group} key={key}>
        <div className={clsx(styles.row, styles.header)}>
          <button
            className={styles.disclose}
            aria-expanded={open}
            aria-label={open ? "Collapse" : "Expand"}
            onClick={() => setCollapsed((c) => ({ ...c, [key]: open }))}
          >
            <Icon name={open ? "chevron-down" : "chevron-right"} size={12} />
          </button>
          <CheckMark
            checked={all}
            indeterminate={!all && some}
            label={`Include all ${title} in commit`}
            onToggle={() => toggleGroup(list, !all)}
          />
          <span className={styles.groupTitle}>
            {tx(title)}
            <span className={styles.count}>
              {list.length} {list.length === 1 ? tx("file") : tx("files")}
            </span>
          </span>
        </div>
        {open && (multi ? sections(list).map(([root, own]) => projectSection(key, root, own)) : list.map((r) => renderRow(r, copies[0] ?? null)))}
      </div>
    );
  };

  // A workspace of several working copies (an Eclipse-style one) nests each
  // project folder inside Changes / Unversioned Files, like a directory level;
  // a single-copy project stays flat.
  const copyOf = (r: Row): string | null =>
    copies
      .filter((c) => r.path === c || r.path.startsWith(`${c}/`))
      .sort((a, b) => b.length - a.length)[0] ?? null;
  const sections = (list: Row[]): Array<[string | null, Row[]]> => {
    const byRoot = new Map<string | null, Row[]>();
    for (const root of [...copies, null]) {
      const own = list.filter((r) => copyOf(r) === root);
      if (own.length) byRoot.set(root, own);
    }
    return [...byRoot.entries()];
  };
  const projectSection = (groupKey: string, root: string | null, own: Row[]) => {
    const key = `${groupKey}\0${root ?? ""}`;
    const title = root ? basename(root) : tx("Other Files");
    const all = own.every((r) => r.checked);
    const some = own.some((r) => r.checked);
    const open = !collapsed[key];
    return (
      <div className={styles.project} key={key}>
        <div className={clsx(styles.row, styles.header, styles.projectHeader)}>
          <button
            className={styles.disclose}
            aria-expanded={open}
            aria-label={open ? "Collapse" : "Expand"}
            onClick={() => setCollapsed((c) => ({ ...c, [key]: open }))}
          >
            <Icon name={open ? "chevron-down" : "chevron-right"} size={12} />
          </button>
          <CheckMark
            checked={all}
            indeterminate={!all && some}
            label={`Include all ${title} changes in commit`}
            onToggle={() => toggleGroup(own, !all)}
          />
          <Icon name="folder" size={14} />
          <span className={styles.groupTitle}>
            {title}
            <span className={styles.count}>
              {own.length} {own.length === 1 ? tx("file") : tx("files")}
            </span>
          </span>
        </div>
        {open && own.map((r) => renderRow(r, root))}
      </div>
    );
  };
  const multi = copies.length > 1;

  return (
    <div className={styles.root}>
      <div className={styles.list}>
        {storeError && <div className={styles.err}>status error: {storeError}</div>}
        {rows.length === 0 && <div className={styles.empty}><T>No changes</T></div>}
        {group("changes", "Changes", tracked)}
        {group("untracked", "Unversioned Files", untracked)}
      </div>
      <textarea
        className={styles.msg}
        placeholder={tx("Commit message")}
        value={message}
        onChange={(e) => setMessage(e.target.value)}
      />
      {error && <div className={styles.err}>{error}</div>}
      <div className={styles.actions}>
        <button
          className={styles.btn}
          disabled={!message.trim() || selected.length === 0 || busy !== null}
          onClick={() => void run("commit", () => call(commands.vcsSvnCommit(repo, selected, message)))}
        >
          {busy === "commit" ? "…" : tx("Commit")}
        </button>
        <button
          className={styles.ghost}
          disabled={busy !== null}
          onClick={() => void run("update", () => call(commands.vcsSvnUpdate(repo)))}
        >
          {busy === "update" ? "…" : tx("Update")}
        </button>
        <span className={styles.summary}>
          {tx`${selected.length} of ${rows.length} selected`}
        </span>
      </div>
    </div>
  );
}
