// The Quadra engine compiled to WebAssembly (crates/quadra-wasm), loaded once.
export type Files = Record<string, string>;
export type Diagnostic = { path: string; from: number; to: number; line: number; severity: "error" | "warning"; message: string };
export type CheckResult = { diagnostics: Diagnostic[]; fixtures: { name: string; module: string }[]; tests: number; ok: boolean; error?: string };
export type TestResult = { name: string; location: string; passed: boolean; message: string | null };
export type Cell = null | boolean | string | { ref: number; ent: string; d: string } | { error: string };
export type EntityData = { name: string; module: string; fields: string[]; count: number; rows: { id: number; display: string; cells: Cell[] }[] };
export type SessionResult = {
  entities?: EntityData[];
  error_message?: string | null;
  failed_at?: number | null;
  compile_error?: boolean;
  diagnostics?: Diagnostic[];
  error?: string;
};

type Wasm = {
  default: (url?: string) => Promise<unknown>;
  check: (files: string) => string;
  test: (files: string) => string;
  ide: (files: string, path: string, offset: number) => string;
  session: (files: string, fixture: string, statements: string) => string;
  app: (files: string, fixture: string, statements: string, request: string) => string;
};

let loading: Promise<Wasm> | null = null;

export function engine(): Promise<Wasm> {
  loading ??= (async () => {
    // built by `npm run wasm` (scripts/build-wasm.sh); Vite bundles the .wasm next to it
    // @ts-ignore: generated JavaScript
    const mod = (await import("./wasm/quadra_wasm.js")) as Wasm;
    await mod.default();
    return mod;
  })();
  return loading;
}

const json = <T>(s: string): T => JSON.parse(s) as T;

export const quadra = {
  check: async (files: Files) => json<CheckResult>((await engine()).check(JSON.stringify(files))),
  test: async (files: Files) => json<{ results?: TestResult[]; diagnostics?: Diagnostic[]; error?: string }>((await engine()).test(JSON.stringify(files))),
  ide: async (files: Files, path: string, offset: number) =>
    json<{ hover: { from: number; to: number; markdown: string } | null; definition: { path: string; from: number; to: number } | null }>((await engine()).ide(JSON.stringify(files), path, offset)),
  /** the server's HTTP API, on the session: the web app runs on it */
  app: async (files: Files, fixture: string, statements: string[], request: { method: string; path: string; body?: unknown }) =>
    json<{ status: number; body: Record<string, unknown>; append?: string }>((await engine()).app(JSON.stringify(files), fixture, JSON.stringify(statements), JSON.stringify(request))),
  session: async (files: Files, fixture: string, statements: string[]) => json<SessionResult>((await engine()).session(JSON.stringify(files), fixture, JSON.stringify(statements))),
};
