/**
 * @n-dimension-database-ndb/client — thin typed client for the nDB wire protocol v1.
 *
 * Zero runtime dependencies. Targets the `/v1` HTTP API documented in
 * docs/PROTOCOL.md. Runs anywhere `fetch` + `AbortController` exist:
 * Node ≥18, browsers, Deno, and edge runtimes.
 *
 * Mirrors the Rust client (`ndb-client-rust`). Retry semantics match it:
 * GET requests retry fully (transport errors + 502/503/504); writes retry
 * only when the connection failed before a response arrived, so a commit
 * is never double-applied.
 */

export interface ClientOptions {
  /** Bearer token sent as `Authorization: Bearer <token>`. */
  token?: string;
  /** Per-request read timeout (GET) in ms. Default 60000. */
  readTimeoutMs?: number;
  /** Per-request write timeout (POST) in ms. Default 30000. */
  writeTimeoutMs?: number;
  /** Max retry attempts (0 = no retries). Default 0. */
  retries?: number;
  /** Base backoff between retries in ms (exponential). Default 100. */
  baseBackoffMs?: number;
  /** Injectable fetch (for tests / non-global environments). */
  fetch?: typeof fetch;
}

/** A protocol-level error: a non-2xx response carrying `{error:{code,message}}`. */
export class NdbError extends Error {
  readonly status: number;
  readonly code: string;
  constructor(status: number, code: string, message: string) {
    super(`nDB ${status} ${code}: ${message}`);
    this.name = "NdbError";
    this.status = status;
    this.code = code;
  }
}

export interface HealthResponse {
  status: string;
}
export interface CommitResponse {
  tx_id: number;
}
export interface FlushResponse {
  memtable_records: number;
  memtable_bytes: number;
  sstable_count: number;
}
export interface CompactResponse {
  records_in: number;
  records_out: number;
  sstables_in: number;
  new_sstable_seq: number | null;
}
/** A query result: column names + row tuples (cells are protocol JSON values). */
export interface QueryResponse {
  columns: string[];
  rows: unknown[][];
}
/** Result of `read(uuid)`: a live record, a tombstone, or not-found. */
export interface ReadResponse {
  outcome: "live" | "tombstoned" | "not_found";
  record?: Record<string, unknown>;
}
/** One record as returned by the protocol (shape depends on `kind`). */
export type JsonRecord = Record<string, unknown>;

/** A record to commit. `kind` selects the shape; see docs/PROTOCOL.md. */
export type CommitRecord = Record<string, unknown> & { kind: string };
export interface CommitRequest {
  records: CommitRecord[];
}

const RETRYABLE_STATUS = new Set([502, 503, 504]);

export class NdbClient {
  private readonly base: string;
  private readonly opts: Required<Omit<ClientOptions, "token" | "fetch">> & {
    token: string;
    fetch: typeof fetch;
  };

  /** @param baseUrl e.g. `http://127.0.0.1:8742` (no trailing slash needed). */
  constructor(baseUrl: string, options: ClientOptions = {}) {
    this.base = baseUrl.replace(/\/+$/, "") + "/v1";
    const f = options.fetch ?? globalThis.fetch;
    if (typeof f !== "function") {
      throw new Error(
        "no global fetch; pass options.fetch (Node <18 / non-browser)",
      );
    }
    this.opts = {
      token: options.token ?? "",
      readTimeoutMs: options.readTimeoutMs ?? 60_000,
      writeTimeoutMs: options.writeTimeoutMs ?? 30_000,
      retries: options.retries ?? 0,
      baseBackoffMs: options.baseBackoffMs ?? 100,
      fetch: f,
    };
  }

  // ---- reads -------------------------------------------------------------

  health(): Promise<HealthResponse> {
    return this.get<HealthResponse>("/health");
  }

  read(uuid: string): Promise<ReadResponse> {
    return this.get<ReadResponse>(`/read/${encodeURIComponent(uuid)}`);
  }

  /**
   * Iterate all visible records at a snapshot. The server streams JSONL
   * (one record per line); this parses it into an array, mirroring the
   * Rust client. `snapshot` optionally pins an `as_of` tx id / timestamp.
   */
  iter(opts: { snapshot?: string | number } = {}): Promise<JsonRecord[]> {
    const q = new URLSearchParams();
    if (opts.snapshot != null) q.set("snapshot", String(opts.snapshot));
    const qs = q.toString();
    return this.request<JsonRecord[]>(
      "GET",
      `/iter${qs ? `?${qs}` : ""}`,
      undefined,
      undefined,
      true,
      parseJsonl,
    );
  }

  /** Run query SOURCE TEXT; the server parses + resolves names. */
  queryText(text: string): Promise<QueryResponse> {
    return this.post<QueryResponse>("/query/text", text, "text/plain");
  }

  /** Run a wire-AST query request object. */
  query(req: unknown): Promise<QueryResponse> {
    return this.post<QueryResponse>("/query", JSON.stringify(req));
  }

  lookup(req: unknown): Promise<unknown> {
    return this.post("/lookup", JSON.stringify(req));
  }
  vectorSearch(req: unknown): Promise<unknown> {
    return this.post("/vector_search", JSON.stringify(req));
  }
  propertyLookup(req: unknown): Promise<unknown> {
    return this.post("/property_lookup", JSON.stringify(req));
  }
  propertyRange(req: unknown): Promise<unknown> {
    return this.post("/property_range", JSON.stringify(req));
  }
  traverse(req: unknown): Promise<unknown> {
    return this.post("/traverse", JSON.stringify(req));
  }

  // ---- writes ------------------------------------------------------------

  commit(req: CommitRequest): Promise<CommitResponse> {
    return this.post<CommitResponse>("/commit", JSON.stringify(req));
  }
  flush(): Promise<FlushResponse> {
    return this.post<FlushResponse>("/flush", "");
  }
  compact(): Promise<CompactResponse> {
    return this.post<CompactResponse>("/compact", "");
  }

  // ---- transport ---------------------------------------------------------

  private get<T>(path: string): Promise<T> {
    // GET is idempotent: retry transport errors AND 502/503/504 responses.
    return this.request<T>("GET", path, undefined, undefined, true);
  }

  private post<T>(
    path: string,
    body: string,
    contentType = "application/json",
  ): Promise<T> {
    // Writes: retry connection failures only (no response received), never a
    // received response — so a commit is never applied twice.
    return this.request<T>("POST", path, body, contentType, false);
  }

  private async request<T>(
    method: "GET" | "POST",
    path: string,
    body: string | undefined,
    contentType: string | undefined,
    retryOnStatus: boolean,
    parse: (resp: Response) => Promise<unknown> = parseBody,
  ): Promise<T> {
    const url = this.base + path;
    const timeout =
      method === "GET" ? this.opts.readTimeoutMs : this.opts.writeTimeoutMs;
    const headers: Record<string, string> = {};
    if (this.opts.token) headers["Authorization"] = `Bearer ${this.opts.token}`;
    if (contentType) headers["Content-Type"] = contentType;

    let lastErr: unknown;
    for (let attempt = 0; attempt <= this.opts.retries; attempt++) {
      if (attempt > 0) {
        await sleep(this.opts.baseBackoffMs * 2 ** (attempt - 1));
      }
      const ctrl = new AbortController();
      const timer = setTimeout(() => ctrl.abort(), timeout);
      let resp: Response;
      try {
        resp = await this.opts.fetch(url, {
          method,
          headers,
          body,
          signal: ctrl.signal,
        });
      } catch (e) {
        // No response received → connection-level failure. Safe to retry
        // for both reads and writes (write was never delivered).
        clearTimeout(timer);
        lastErr = e;
        continue;
      }
      clearTimeout(timer);

      if (resp.ok) {
        return (await parse(resp)) as T;
      }
      // A response WAS received. Reads may retry on transient 5xx; writes
      // never retry a received response.
      if (retryOnStatus && RETRYABLE_STATUS.has(resp.status)) {
        lastErr = await errorFromResponse(resp);
        continue;
      }
      throw await errorFromResponse(resp);
    }
    throw lastErr instanceof Error
      ? lastErr
      : new Error(`request failed after retries: ${String(lastErr)}`);
  }
}

/** Parse a JSONL stream (one JSON value per line) into an array. */
async function parseJsonl(resp: Response): Promise<unknown[]> {
  const text = await resp.text();
  const out: unknown[] = [];
  for (const line of text.split("\n")) {
    const trimmed = line.trim();
    if (trimmed) out.push(JSON.parse(trimmed));
  }
  return out;
}

async function parseBody(resp: Response): Promise<unknown> {
  const text = await resp.text();
  if (!text) return {};
  try {
    return JSON.parse(text);
  } catch {
    return text;
  }
}

async function errorFromResponse(resp: Response): Promise<NdbError> {
  let code = "http_error";
  let message = resp.statusText || `HTTP ${resp.status}`;
  try {
    const body = (await resp.json()) as {
      error?: string | { code?: string; message?: string };
      detail?: string;
    };
    // nDB envelope: { "error": "<code>", "detail": "<message>" }.
    if (typeof body?.error === "string") {
      code = body.error;
      if (typeof body.detail === "string") message = body.detail;
    } else if (body?.error && typeof body.error === "object") {
      // Tolerate a nested { error: { code, message } } shape too.
      code = body.error.code ?? code;
      message = body.error.message ?? message;
    }
  } catch {
    /* non-JSON error body — keep defaults */
  }
  return new NdbError(resp.status, code, message);
}

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}
