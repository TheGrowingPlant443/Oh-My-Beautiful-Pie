import { LAYOUT_PROTOCOL_VERSION, seededPosition } from "./overview-layout-core";

export { LAYOUT_PROTOCOL_VERSION, seededPosition };

/** The part of a Worker the client uses (a test double implements the same). */
export interface LayoutWorkerLike {
  postMessage(message: unknown, transfer?: Transferable[]): void;
  terminate(): void;
  onmessage: ((event: { data: unknown }) => void) | null;
  onerror: ((event: { message?: string }) => void) | null;
}

export interface LayoutPositions { ids: string[]; x: Float32Array; y: Float32Array }
/** Reported by the Sigma adapter to the page while the worker lays the graph out. */
export interface LayoutStatus { state: "running" | "done" | "error"; message: string | null; milliseconds: number }
export interface LayoutRequest {
  ids: readonly string[];
  /** Optional start positions per node, in `ids` order; missing ones are seeded from the id. */
  seeds?: ReadonlyMap<string, { x: number; y: number }>;
  edges: readonly (readonly [string, string])[];
  iterations: number;
  onProgress?: (positions: LayoutPositions) => void;
}

type Outgoing = { type: "layout"; version: number; jobId: number; seeds: Float32Array; edges: Uint32Array; iterations: number } | { type: "cancel"; version: number; jobId: number };

interface Active {
  jobId: number;
  ids: string[];
  settle: (value: LayoutPositions | null) => void;
  fail: (error: Error) => void;
  onProgress?: (positions: LayoutPositions) => void;
}

/**
 * Runs ForceAtlas2 in a Web Worker. One job at a time: a new `run` cancels the previous one (it resolves null), `cancel` stops the
 * current one, and results carrying an old job id or another protocol version are ignored. A failed worker is replaced on the next run.
 */
export class LayoutClient {
  private worker: LayoutWorkerLike | null = null;
  private active: Active | null = null;
  private sequence = 0;
  private readonly factory: () => LayoutWorkerLike;
  constructor(factory: () => LayoutWorkerLike) { this.factory = factory; }

  private ensure(): LayoutWorkerLike {
    if (this.worker) return this.worker;
    const worker = this.factory();
    worker.onmessage = event => this.receive(event.data);
    worker.onerror = event => {
      const failing = this.active;
      this.active = null;
      this.worker = null;
      worker.terminate();
      failing?.fail(new Error(event.message || "The layout worker stopped unexpectedly"));
    };
    this.worker = worker;
    return worker;
  }

  private receive(raw: unknown): void {
    const message = raw as { type?: string; version?: number; jobId?: number; positions?: Float32Array; message?: string } | null;
    const current = this.active;
    if (!message || !current || message.jobId !== current.jobId) return;
    if (message.version !== LAYOUT_PROTOCOL_VERSION) {
      this.active = null;
      // A worker of another version is never reused.
      this.worker?.terminate();
      this.worker = null;
      current.fail(new Error(`Layout worker speaks protocol ${message.version}; this page needs ${LAYOUT_PROTOCOL_VERSION}`));
      return;
    }
    const positions = (): LayoutPositions | null => {
      const raw = message.positions;
      if (raw?.length !== current.ids.length * 2) return null;
      const x = new Float32Array(current.ids.length), y = new Float32Array(current.ids.length);
      for (let i = 0; i < current.ids.length; i++) { x[i] = raw[2 * i]; y[i] = raw[2 * i + 1]; }
      return { ids: current.ids, x, y };
    };
    if (message.type === "progress") {
      const found = positions();
      if (found) current.onProgress?.(found);
    } else if (message.type === "done") {
      this.active = null;
      const found = positions();
      if (found) current.settle(found); else current.fail(new Error("The layout worker returned positions for a different graph"));
    } else if (message.type === "cancelled") {
      this.active = null;
      current.settle(null);
    } else if (message.type === "error") {
      this.active = null;
      current.fail(new Error(message.message || "Layout failed"));
    }
  }

  /** Resolves with the final positions, or null when this job was superseded or cancelled. */
  run(request: LayoutRequest): Promise<LayoutPositions | null> {
    this.cancel();
    const ids = [...request.ids];
    const position = new Map(ids.map((id, i) => [id, i]));
    const seeds = new Float32Array(ids.length * 2);
    ids.forEach((id, i) => {
      const seed = request.seeds?.get(id) ?? seededPosition(id, i);
      seeds[2 * i] = seed.x;
      seeds[2 * i + 1] = seed.y;
    });
    const edges: number[] = [];
    for (const [from, to] of request.edges) {
      const a = position.get(from), b = position.get(to);
      if (a !== undefined && b !== undefined && a !== b) edges.push(a, b);
    }
    const jobId = ++this.sequence;
    return new Promise<LayoutPositions | null>((settle, reject) => {
      let worker: LayoutWorkerLike;
      try { worker = this.ensure(); } catch (error) { reject(error instanceof Error ? error : new Error(String(error))); return; }
      this.active = { jobId, ids, settle, fail: reject, onProgress: request.onProgress };
      const message: Outgoing = { type: "layout", version: LAYOUT_PROTOCOL_VERSION, jobId, seeds, edges: Uint32Array.from(edges), iterations: request.iterations };
      worker.postMessage(message, [message.seeds.buffer, message.edges.buffer]);
    });
  }

  /** Stops the current job; its promise resolves null at once. */
  cancel(): void {
    const current = this.active;
    if (!current) return;
    this.active = null;
    this.worker?.postMessage({ type: "cancel", version: LAYOUT_PROTOCOL_VERSION, jobId: current.jobId } satisfies Outgoing);
    current.settle(null);
  }

  /** Cancels and releases the worker. */
  dispose(): void {
    this.cancel();
    this.worker?.terminate();
    this.worker = null;
  }
}

/** Positions keyed by node id. */
export function positionMap(positions: LayoutPositions): Map<string, { x: number; y: number }> {
  return new Map(positions.ids.map((id, i) => [id, { x: positions.x[i], y: positions.y[i] }]));
}
