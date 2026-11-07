/**
 * Revert Local Changes (Subversion): lists the files that will be rolled back,
 * each row clickable to compare it first (the diff opens above the dialog),
 * with a checkbox to leave files alone.
 */
import { useState } from "react";
import { Dialog, Button, Icon, fileIconName, T } from "@/ui";
import type { FileStatus } from "@/bindings";
import { basename } from "@/platform/project";
import { tx } from "@/platform/i18n";
import { openDialog } from "./dialogs";
import { openDiffDialog } from "./diffDialog";
import { effectiveKind, KIND_COLOR } from "./model";
import { svnDiffData } from "./SvnCommitView";
import styles from "./SvnRevertDialog.module.css";

export function showSvnRevert(files: readonly FileStatus[], onConfirm: (paths: string[]) => void) {
  openDialog((close) => (
    <SvnRevertDialog
      files={files}
      onClose={close}
      onConfirm={(paths) => {
        close();
        onConfirm(paths);
      }}
    />
  ));
}

function SvnRevertDialog({
  files,
  onClose,
  onConfirm,
}: {
  files: readonly FileStatus[];
  onClose: () => void;
  onConfirm: (paths: string[]) => void;
}) {
  const [chosen, setChosen] = useState<Record<string, boolean>>(() =>
    Object.fromEntries(files.map((f) => [f.path, true])),
  );
  const selected = files.filter((f) => chosen[f.path]);
  return (
    <Dialog
      open
      title={tx("Revert Local Changes")}
      onClose={onClose}
      width={560}
      buttons={[
        {
          label: tx("Revert"),
          role: "default",
          disabled: selected.length === 0,
          onClick: () => onConfirm(selected.map((f) => f.path)),
        },
        { label: tx("Cancel"), role: "cancel" },
      ]}
    >
      <div className={styles.body}>
        <div className={styles.hint}>
          <T>Select the files to roll back. Click a file to compare it first.</T>
        </div>
        <div className={styles.list}>
          {files.map((f) => {
            const kind = effectiveKind(f) ?? "modified";
            return (
              <div
                key={f.path}
                className={styles.row}
                onClick={() => openDiffDialog(svnDiffData(f.path, effectiveKind(f)))}
                title={f.path}
              >
                <input
                  type="checkbox"
                  checked={chosen[f.path] ?? false}
                  onClick={(e) => e.stopPropagation()}
                  onChange={(e) => setChosen((c) => ({ ...c, [f.path]: e.target.checked }))}
                />
                <Icon name={fileIconName(basename(f.path), f.path)} size={16} />
                <span className={styles.name} style={{ color: KIND_COLOR[kind] }}>
                  {basename(f.path)}
                </span>
                <span className={styles.path}>{f.relPath}</span>
                <Button
                  size="small"
                  variant="ghost"
                  onClick={(e) => {
                    e.stopPropagation();
                    openDiffDialog(svnDiffData(f.path, effectiveKind(f)));
                  }}
                >
                  <T>Show Diff</T>
                </Button>
              </div>
            );
          })}
        </div>
      </div>
    </Dialog>
  );
}
