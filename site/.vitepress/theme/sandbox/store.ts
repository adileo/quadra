// The sandbox's "git in the browser": every workspace (a project the user opened) keeps its
// files, its commits (snapshots with a message) and its data session, in IndexedDB.
import type { Files } from "./engine";

export type Commit = { id: string; message: string; at: string; files: Files };
export type Session = { fixture: string; statements: string[] };
export type Workspace = { name: string; title: string; template: string; files: Files; commits: Commit[]; session: Session; updated: string };

const DB = "quadra-sandbox";
const STORE = "workspaces";

function open(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const r = indexedDB.open(DB, 1);
    r.onupgradeneeded = () => r.result.createObjectStore(STORE, { keyPath: "name" });
    r.onsuccess = () => resolve(r.result);
    r.onerror = () => reject(r.error);
  });
}

async function tx<T>(mode: IDBTransactionMode, run: (s: IDBObjectStore) => IDBRequest<T>): Promise<T> {
  const db = await open();
  return new Promise((resolve, reject) => {
    const r = run(db.transaction(STORE, mode).objectStore(STORE));
    r.onsuccess = () => resolve(r.result);
    r.onerror = () => reject(r.error);
  });
}

/** Storage can be missing (private windows, blocked site data): the sandbox still works, unsaved. */
export const storage = {
  async get(name: string): Promise<Workspace | undefined> {
    try {
      return await tx("readonly", (s) => s.get(name) as IDBRequest<Workspace | undefined>);
    } catch {
      return undefined;
    }
  },
  async put(w: Workspace): Promise<boolean> {
    try {
      await tx("readwrite", (s) => s.put(JSON.parse(JSON.stringify(w))));
      return true;
    } catch {
      return false;
    }
  },
  async list(): Promise<Workspace[]> {
    try {
      return await tx("readonly", (s) => s.getAll() as IDBRequest<Workspace[]>);
    } catch {
      return [];
    }
  },
  async remove(name: string): Promise<void> {
    try {
      await tx("readwrite", (s) => s.delete(name));
    } catch {
      /* nothing to remove */
    }
  },
};

export const newId = () => Math.random().toString(16).slice(2, 9);

/** Files changed since a snapshot: added, modified, deleted. */
export function changes(base: Files, now: Files): Record<string, "A" | "M" | "D"> {
  const out: Record<string, "A" | "M" | "D"> = {};
  for (const p of Object.keys(now)) if (!(p in base)) out[p] = "A";
  else if (base[p] !== now[p]) out[p] = "M";
  for (const p of Object.keys(base)) if (!(p in now)) out[p] = "D";
  return out;
}
