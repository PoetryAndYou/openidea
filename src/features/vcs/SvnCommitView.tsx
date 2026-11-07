/**
 * SVN commit panel — a single-pane variant of the Git commit interface.
 * SVN has no index: the checkbox selection is what gets committed, via
 * `svn commit <paths> -m`. Grouped like the Git panel (Changes / Unversioned
 * files) with the product's CheckMark boxes, file-type icons and
 * basename-first rows; double-click previews the working diff against BASE.
 */
import { useMemo, useState } from "react";
import clsx from "clsx";
import { currentRepoRoot, scheduleRefresh, useVcs, vcsKind } from "./store";
import { effectiveKind, KIND_COLOR, type EffectiveKind } from "./model";
import { call, commands } from "@/platform/ipc";
import { basename, dirname } from "@/platform/project";
import { Icon, T, fileIconName } from "@/ui";
import { tx } from "@/platform/i18n";
import { deleteFiles } from "./changeOps";
import { openDiff, withChain, type DiffData } from "./diffInput";
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
  const [selectedFile, setSelectedFile] = useState<string | null>(null);
  const [collapsed, setCollapsed] = useState<Record<string, boolean>>({});
  const [message, setMessage] = useState("");
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const rows = useMemo<Row[]>(
    () =>
      status
        .filter((f) => effectiveKind(f) !== "ignored")
        .map((f) => {
          const kind = effectiveKind(f) as EffectiveKind;
          return {
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

  const renderRow = (r: Row) => {
    const dir = relativeDir(repo, r.path);
    return (
      <div
        key={r.path}
        className={clsx(styles.row, styles.fileRow, selectedFile === r.path && styles.rowSelected)}
        onClick={() => setSelectedFile(r.path)}
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
            onClick={(e) => {
              e.stopPropagation();
              // Unversioned: it exists only in the working tree, so rolling it back
              // means removing it. A scheduled add is un-added; anything else is
              // restored from BASE.
              if (r.kind === "untracked") void deleteFiles([r.path]);
              else void run("revert", () => call(commands.vcsSvnRevert([r.path])));
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
        {open && list.map(renderRow)}
      </div>
    );
  };

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
