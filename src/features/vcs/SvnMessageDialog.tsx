/**
 * Set Revision Properties (Subversion): edits a revision's commit message
 * (`svn:log`, a revision property). Saving requires the repository to allow
 * revision-property changes (`pre-revprop-change` hook); a history rewrite by
 * another user reaches every later checkout, so the dialog says so.
 */
import { useState } from "react";
import { Dialog, T } from "@/ui";
import { tx } from "@/platform/i18n";
import { call, commands } from "@/platform/ipc";
import { openDialog } from "./dialogs";
import styles from "./SvnMessageDialog.module.css";

export function showSvnMessage(revision: string, message: string, repo: string | null, onSaved: () => void) {
  openDialog((close) => (
    <SvnMessageDialog
      revision={revision}
      message={message}
      repo={repo}
      onClose={close}
      onSaved={() => {
        close();
        onSaved();
      }}
    />
  ));
}

function SvnMessageDialog({
  revision,
  message,
  repo,
  onClose,
  onSaved,
}: {
  revision: string;
  message: string;
  repo: string | null;
  onClose: () => void;
  onSaved: () => void;
}) {
  const [text, setText] = useState(message);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      await call(commands.vcsSvnSetMessage(repo, revision, text));
      onSaved();
    } catch (e) {
      const raw = e instanceof Error ? e.message : String(e);
      // The usual refusal: the repository has no pre-revprop-change hook.
      setError(
        raw.includes("E165006")
          ? tx("The repository does not allow editing revision properties (no pre-revprop-change hook).")
          : raw,
      );
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      open
      title={tx("Set Revision Properties")}
      onClose={onClose}
      width={620}
      buttons={[
        { label: tx("Save"), role: "default", disabled: busy, onClick: () => void save() },
        { label: tx("Cancel"), role: "cancel" },
      ]}
    >
      <div className={styles.body}>
        <div className={styles.hint}>
          <T>{revision}</T> · <T>Commit message</T>
        </div>
        <textarea
          className={styles.message}
          value={text}
          autoFocus
          placeholder={tx("Commit message")}
          onChange={(e) => setText(e.target.value)}
        />
        <div className={styles.note}>
          <T>
            Only the message of this revision changes. The repository must allow revision-property edits.
          </T>
        </div>
        {error && <div className={styles.error}>{error}</div>}
      </div>
    </Dialog>
  );
}
