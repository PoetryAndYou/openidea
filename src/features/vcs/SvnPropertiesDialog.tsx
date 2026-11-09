/**
 * Edit Properties (Subversion): the versioned properties of one file or folder
 * (`svn proplist -v` / `svn propset` / `svn propdel`). Property changes are
 * working-copy changes: they appear in the commit panel and go up with the next
 * commit.
 */
import { useEffect, useState } from "react";
import { Dialog, Button, Icon, T } from "@/ui";
import { tx } from "@/platform/i18n";
import { call, commands } from "@/platform/ipc";
import type { Property } from "@/bindings";
import { basename } from "@/platform/project";
import { openDialog } from "./dialogs";
import styles from "./SvnPropertiesDialog.module.css";

export function showSvnProperties(path: string, onChanged: () => void) {
  openDialog((close) => <SvnPropertiesDialog path={path} onClose={close} onChanged={onChanged} />);
}

/** The properties IDEA offers to set from a menu. */
const SUGGESTED: Array<{ name: string; hint: string }> = [
  { name: "svn:ignore", hint: "each line: a name or pattern to ignore in this folder" },
  { name: "svn:eol-style", hint: "native | LF | CRLF | CR" },
  { name: "svn:mime-type", hint: "e.g. text/plain, application/octet-stream" },
  { name: "svn:keywords", hint: "e.g. Id Author Date Rev URL" },
  { name: "svn:needs-lock", hint: "* makes the file read-only until locked" },
  { name: "svn:executable", hint: "* makes the file executable" },
];

function SvnPropertiesDialog({
  path,
  onClose,
  onChanged,
}: {
  path: string;
  onClose: () => void;
  onChanged: () => void;
}) {
  const [properties, setProperties] = useState<Property[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [name, setName] = useState("svn:ignore");
  const [value, setValue] = useState("");

  const load = () => {
    setBusy(true);
    setError(null);
    void call(commands.vcsSvnProperties(path))
      .then((found) => setProperties(found))
      .catch((e) => setError(e instanceof Error ? e.message : String(e)))
      .finally(() => setBusy(false));
  };
  useEffect(() => {
    load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [path]);

  const set = async () => {
    if (!name.trim()) return;
    setBusy(true);
    setError(null);
    try {
      await call(commands.vcsSvnSetProperty(path, name.trim(), value));
      load();
      onChanged();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const remove = async (property: string) => {
    setBusy(true);
    setError(null);
    try {
      await call(commands.vcsSvnDeleteProperty(path, property));
      load();
      onChanged();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      open
      title={tx("Edit Properties")}
      onClose={onClose}
      width={640}
      buttons={[{ label: tx("Close"), role: "cancel" }]}
    >
      <div className={styles.body}>
        <div className={styles.hint}>
          <Icon name={basename(path).includes(".") ? "file" : "folder"} size={14} />{" "}
          <T>{path}</T>
        </div>
        {properties === null && !error && <div className={styles.hint}>{tx("Loading…")}</div>}
        {properties?.length === 0 && (
          <div className={styles.hint}>
            <T>This file has no versioned properties</T>
          </div>
        )}
        {properties && properties.length > 0 && (
          <div className={styles.list}>
            {properties.map((property) => (
              <div key={property.name} className={styles.row} onClick={() => {
                setName(property.name);
                setValue(property.value);
              }}>
                <span className={styles.name}>{property.name}</span>
                <span className={styles.value}>{property.value}</span>
                <Button
                  size="small"
                  variant="ghost"
                  onClick={(e) => {
                    e.stopPropagation();
                    void remove(property.name);
                  }}
                >
                  <T>Remove</T>
                </Button>
              </div>
            ))}
          </div>
        )}
        <div className={styles.editor}>
          <div className={styles.field}>
            <span className={styles.label}>{tx("Property")}</span>
            <input
              className={styles.input}
              list="svn-property-names"
              value={name}
              onChange={(e) => setName(e.target.value)}
            />
            <datalist id="svn-property-names">
              {SUGGESTED.map((s) => (
                <option key={s.name} value={s.name} />
              ))}
            </datalist>
          </div>
          <textarea
            className={styles.textarea}
            value={value}
            placeholder={SUGGESTED.find((s) => s.name === name)?.hint ?? tx("Value")}
            onChange={(e) => setValue(e.target.value)}
          />
          <div className={styles.actions}>
            <Button size="small" disabled={busy || !name.trim()} onClick={() => void set()}>
              <T>Set Property</T>
            </Button>
          </div>
        </div>
        {error && <div className={styles.error}>{error}</div>}
        <div className={styles.note}>
          <T>Property changes are local changes: they are committed with the next commit.</T>
        </div>
      </div>
    </Dialog>
  );
}
