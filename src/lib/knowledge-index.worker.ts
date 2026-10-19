/// <reference lib="webworker" />
import { IndexHost, type IndexOutgoing } from "./knowledge-index";

// Cancellable link-index worker: accumulates already validated backend records and builds the graph model. It never reads files or the network.
const scope = self as unknown as DedicatedWorkerGlobalScope;
const host = new IndexHost(message => scope.postMessage(message));
scope.onmessage = (event: MessageEvent<IndexOutgoing>) => host.handle(event.data);
