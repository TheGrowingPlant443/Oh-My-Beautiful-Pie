import type { GraphifyCommunity, GraphifyHyperedge, GraphifyLink, GraphifyNode, VaultLinkRecord, VaultNoteRecord } from "./knowledge";
import { GraphifyModelBuilder, VaultModelBuilder, type GraphifyStats, type VaultStats } from "./knowledge-graph";
import type { GraphModel } from "./system-graph";

// Link-index worker protocol. The page hands the worker the backend's already validated, paged records; the worker accumulates them
// per job and builds the graph model off the UI thread. A job is cancellable at any time and a result carrying an old job id or
// another protocol version is ignored, so a late build can never replace a newer vault/artifact.

export const INDEX_PROTOCOL_VERSION = 1;

export type IndexKind = "vault" | "graphify";
export type IndexChunk =
  | { section: "notes"; items: VaultNoteRecord[] }
  | { section: "links"; items: VaultLinkRecord[] }
  | { section: "graphifyNodes"; items: GraphifyNode[] }
  | { section: "graphifyLinks"; items: GraphifyLink[] }
  | { section: "graphifyCommunities"; items: GraphifyCommunity[] }
  | { section: "graphifyHyperedges"; items: GraphifyHyperedge[] };
export type IndexResult = { kind: "vault"; model: GraphModel; stats: VaultStats } | { kind: "graphify"; model: GraphModel; stats: GraphifyStats };

export type IndexOutgoing =
  | { type: "begin"; version: number; jobId: number; kind: IndexKind }
  | ({ type: "chunk"; version: number; jobId: number } & IndexChunk)
  | { type: "finish"; version: number; jobId: number }
  | { type: "cancel"; version: number; jobId: number };
export type IndexIncoming =
  | { type: "done"; version: number; jobId: number; result: IndexResult }
  | { type: "cancelled"; version: number; jobId: number }
  | { type: "error"; version: number; jobId: number; message: string };

type Builder = { kind: "vault"; builder: VaultModelBuilder } | { kind: "graphify"; builder: GraphifyModelBuilder };

/** Worker-side state machine (also used directly by tests). */
export class IndexHost {
  private readonly jobs = new Map<number, Builder>();
  private readonly post: (message: IndexIncoming) => void;
  constructor(post: (message: IndexIncoming) => void) { this.post = post; }

  handle(message: IndexOutgoing): void {
    if (message.version !== INDEX_PROTOCOL_VERSION) {
      this.post({ type: "error", version: INDEX_PROTOCOL_VERSION, jobId: message.jobId, message: `Link index protocol ${message.version} is not supported (worker speaks ${INDEX_PROTOCOL_VERSION})` });
      return;
    }
    const { jobId } = message;
    try {
      switch (message.type) {
        case "begin":
          this.jobs.set(jobId, message.kind === "vault" ? { kind: "vault", builder: new VaultModelBuilder() } : { kind: "graphify", builder: new GraphifyModelBuilder() });
          return;
        case "cancel":
          this.jobs.delete(jobId);
          this.post({ type: "cancelled", version: INDEX_PROTOCOL_VERSION, jobId });
          return;
        case "chunk": {
          const job = this.jobs.get(jobId);
          if (!job) return;
          if (job.kind === "vault" && message.section === "notes") job.builder.addNotes(message.items);
          else if (job.kind === "vault" && message.section === "links") job.builder.addLinks(message.items);
          else if (job.kind === "graphify" && message.section === "graphifyNodes") job.builder.addNodes(message.items);
          else if (job.kind === "graphify" && message.section === "graphifyLinks") job.builder.addLinks(message.items);
          else if (job.kind === "graphify" && message.section === "graphifyCommunities") job.builder.addCommunities(message.items);
          else if (job.kind === "graphify" && message.section === "graphifyHyperedges") job.builder.addHyperedges(message.items);
          else throw new Error(`Section ${message.section} does not belong to a ${job.kind} job`);
          return;
        }
        case "finish": {
          const job = this.jobs.get(jobId);
          if (!job) return;
          this.jobs.delete(jobId);
          if (job.kind === "vault") this.post({ type: "done", version: INDEX_PROTOCOL_VERSION, jobId, result: { kind: "vault", model: job.builder.build(), stats: job.builder.stats } });
          else this.post({ type: "done", version: INDEX_PROTOCOL_VERSION, jobId, result: { kind: "graphify", ...job.builder.build() } });
          return;
        }
      }
    } catch (error) {
      this.jobs.delete(jobId);
      this.post({ type: "error", version: INDEX_PROTOCOL_VERSION, jobId, message: error instanceof Error ? error.message : String(error) });
    }
  }
}

/** The part of a Worker the client uses (a test double implements the same). */
export interface IndexWorkerLike {
  postMessage(message: unknown): void;
  terminate(): void;
  onmessage: ((event: { data: unknown }) => void) | null;
  onerror: ((event: { message?: string }) => void) | null;
}

export interface IndexJob {
  push(chunk: IndexChunk): void;
  /** Resolves with the model, or null when the job was cancelled or superseded. */
  finish(): Promise<IndexResult | null>;
  cancel(): void;
}

interface Active { jobId: number; settle: (value: IndexResult | null) => void; fail: (error: Error) => void }

/** One job at a time: `begin` cancels the previous job. A failed worker is replaced on the next job. */
export class IndexClient {
  private worker: IndexWorkerLike | null = null;
  private active: Active | null = null;
  private sequence = 0;
  /** A job that was begun but not yet finished: the worker holds its records until it is finished or cancelled. */
  private open: number | null = null;
  private readonly factory: () => IndexWorkerLike;
  constructor(factory: () => IndexWorkerLike) { this.factory = factory; }

  private ensure(): IndexWorkerLike {
    if (this.worker) return this.worker;
    const worker = this.factory();
    worker.onmessage = event => this.receive(event.data);
    worker.onerror = event => {
      const failing = this.active;
      this.active = null;
      this.worker = null;
      worker.terminate();
      failing?.fail(new Error(event.message || "The link index worker stopped unexpectedly"));
    };
    this.worker = worker;
    return worker;
  }

  private receive(raw: unknown): void {
    const message = raw as IndexIncoming | null;
    const current = this.active;
    if (!message || !current || message.jobId !== current.jobId) return;
    if (message.version !== INDEX_PROTOCOL_VERSION) {
      this.active = null;
      this.worker?.terminate();
      this.worker = null;
      current.fail(new Error(`Link index worker speaks protocol ${message.version}; this page needs ${INDEX_PROTOCOL_VERSION}`));
      return;
    }
    this.active = null;
    if (message.type === "done") current.settle(message.result);
    else if (message.type === "cancelled") current.settle(null);
    else current.fail(new Error(message.message || "The link index failed"));
  }

  begin(kind: IndexKind): IndexJob {
    this.cancel();
    const jobId = ++this.sequence;
    this.open = jobId;
    const worker = this.ensure();
    worker.postMessage({ type: "begin", version: INDEX_PROTOCOL_VERSION, jobId, kind } satisfies IndexOutgoing);
    return {
      push: chunk => { if (this.sequence === jobId) worker.postMessage({ type: "chunk", version: INDEX_PROTOCOL_VERSION, jobId, ...chunk } as IndexOutgoing); },
      finish: () => new Promise<IndexResult | null>((settle, reject) => {
        if (this.sequence !== jobId) { settle(null); return; }
        this.active = { jobId, settle, fail: reject };
        this.open = null;
        worker.postMessage({ type: "finish", version: INDEX_PROTOCOL_VERSION, jobId } satisfies IndexOutgoing);
      }),
      cancel: () => { if (this.sequence === jobId) this.cancel(); },
    };
  }

  /** Stops the current job; its promise resolves null at once. */
  cancel(): void {
    const current = this.active;
    const unfinished = this.open;
    this.sequence++;
    this.active = null;
    this.open = null;
    if (unfinished !== null) this.worker?.postMessage({ type: "cancel", version: INDEX_PROTOCOL_VERSION, jobId: unfinished } satisfies IndexOutgoing);
    if (current) {
      this.worker?.postMessage({ type: "cancel", version: INDEX_PROTOCOL_VERSION, jobId: current.jobId } satisfies IndexOutgoing);
      current.settle(null);
    }
  }

  dispose(): void {
    this.cancel();
    this.worker?.terminate();
    this.worker = null;
  }
}
