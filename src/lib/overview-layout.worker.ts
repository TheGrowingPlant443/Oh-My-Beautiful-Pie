/// <reference lib="webworker" />
import { LAYOUT_PROTOCOL_VERSION, runLayout, type LayoutJob } from "./overview-layout-core";

// Versioned, cancellable layout worker for the Sigma views. It never touches repository data: only numeric seeds and edge indexes.
type Incoming =
  | { type: "layout"; version: number; jobId: number; seeds: Float32Array; edges: Uint32Array; iterations: number }
  | { type: "cancel"; version: number; jobId: number };

const cancelled = new Set<number>();
const scope = self as unknown as DedicatedWorkerGlobalScope;

scope.onmessage = (event: MessageEvent<Incoming>) => {
  const message = event.data;
  if (message.version !== LAYOUT_PROTOCOL_VERSION) {
    scope.postMessage({ type: "error", version: LAYOUT_PROTOCOL_VERSION, jobId: message.jobId, message: `Layout protocol ${message.version} is not supported (worker speaks ${LAYOUT_PROTOCOL_VERSION})` });
    return;
  }
  if (message.type === "cancel") { cancelled.add(message.jobId); return; }
  const job: LayoutJob = { seeds: message.seeds, edges: message.edges, iterations: message.iterations };
  void runLayout(job, {
    cancelled: () => cancelled.has(message.jobId),
    yieldNow: () => new Promise(resolve => setTimeout(resolve, 0)),
    progress: (positions, iteration) => scope.postMessage({ type: "progress", version: LAYOUT_PROTOCOL_VERSION, jobId: message.jobId, iteration, positions }, [positions.buffer]),
  }).then(positions => {
    if (positions === null) { scope.postMessage({ type: "cancelled", version: LAYOUT_PROTOCOL_VERSION, jobId: message.jobId }); return; }
    scope.postMessage({ type: "done", version: LAYOUT_PROTOCOL_VERSION, jobId: message.jobId, positions }, [positions.buffer]);
  }, error => {
    scope.postMessage({ type: "error", version: LAYOUT_PROTOCOL_VERSION, jobId: message.jobId, message: error instanceof Error ? error.message : String(error) });
  }).finally(() => { cancelled.delete(message.jobId); });
};
