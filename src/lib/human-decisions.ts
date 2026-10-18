import { writable } from "svelte/store";
import type { HumanDecision, HumanDecisionResponse, HumanDecisionSnapshot, HumanDecisionTarget, NativeDecisionImage, NativeDecisionOption, NativeDecisionQuestion, NativeDecisionResult } from "./api";

// No live IDs or answer drafts are persisted. App owns the connection, not a keyed terminal body.
export const decisionKey = (target: HumanDecisionTarget): string => JSON.stringify([
  target.instanceId, target.requestId, target.generation, target.sessionId, target.ownershipEpoch, target.workspaceEpoch,
]);
export const nativeOption = (option: string | NativeDecisionOption): NativeDecisionOption => typeof option === "string" ? { label: option } : option;
export const canAnswer = (item: HumanDecision): boolean => item.answerable && ["approve", "confirm", "select", "input", "editor", "askDialog"].includes(item.method);
export interface QuestionDraft {
  selectedOptions: string[]; customInput: string; note: string;
  customInputImages: NativeDecisionImage[]; noteImages: NativeDecisionImage[];
}
export interface DecisionDraft {
  text: string; questions: Record<string, QuestionDraft>; submitting: boolean;
  awaitingResolution: boolean; loadingImages: number; error: string | null;
}
export function initialDecisionDraft(item: HumanDecision): DecisionDraft {
  return { text: item.method === "editor" ? item.payload.prefill ?? "" : "", questions: Object.fromEntries(
    (item.payload.questions ?? []).map(question => [question.id, { selectedOptions: [], customInput: "", note: "", customInputImages: [], noteImages: [] }]),
  ), submitting: false, awaitingResolution: false, loadingImages: 0, error: null };
}
export function toggleQuestionOption(question: NativeDecisionQuestion, draft: QuestionDraft, index: number): QuestionDraft {
  const option = question.options[index];
  if (option === undefined || question.disabledIndices?.includes(index)) return draft;
  const label = nativeOption(option).label;
  const selected = draft.selectedOptions.includes(label);
  return { ...draft, selectedOptions: question.multi
    ? selected ? draft.selectedOptions.filter(value => value !== label) : [...draft.selectedOptions, label]
    : selected ? [] : [label] };
}
export function askResponse(item: HumanDecision, draft: DecisionDraft): HumanDecisionResponse {
  const results: NativeDecisionResult[] = (item.payload.questions ?? []).map(question => {
    const answer = draft.questions[question.id];
    const enabled = question.options.map(nativeOption).filter((_, index) => !question.disabledIndices?.includes(index)).map(option => option.label);
    if (!answer || answer.selectedOptions.some(label => !enabled.includes(label)) || new Set(answer.selectedOptions).size !== answer.selectedOptions.length || (!question.multi && answer.selectedOptions.length > 1)) throw new Error("Invalid native question selection");
    const result: NativeDecisionResult = { id: question.id, selectedOptions: [...answer.selectedOptions] };
    if (answer.customInput !== "") result.customInput = answer.customInput;
    if (answer.note !== "") result.note = answer.note;
    // Images enter the draft only after an explicit human file selection. Never synthesize attachments.
    if (item.payload.acceptImages) {
      if (answer.customInputImages.length) result.customInputImages = [...answer.customInputImages];
      if (answer.noteImages.length) result.noteImages = [...answer.noteImages];
    }
    return result;
  });
  return { kind: "submit", results };
}
export async function readDecisionImage(file: File): Promise<NativeDecisionImage> {
  if (!file.type.startsWith("image/")) throw new Error("Select an image file");
  const bytes = new Uint8Array(await file.arrayBuffer());
  const chunks: string[] = [];
  for (let offset = 0; offset < bytes.length; offset += 32768) {
    chunks.push(String.fromCharCode(...bytes.subarray(offset, offset + 32768)));
  }
  return { type: "image", data: btoa(chunks.join("")), mimeType: file.type };
}
export interface HumanDecisionState {
  revision: number; items: HumanDecision[]; drafts: ReadonlyMap<string, DecisionDraft>; loading: boolean; error: string | null;
}
export interface HumanDecisionTransport {
  listen: (receive: (snapshot: HumanDecisionSnapshot) => void) => Promise<() => void>;
  list: () => Promise<HumanDecisionSnapshot>;
  respond: (target: HumanDecisionTarget, response: HumanDecisionResponse) => Promise<{ accepted: true }>;
}
const errorText = (error: unknown): string => error instanceof Error ? error.message : String(error);
export function createHumanDecisionStore() {
  let state: HumanDecisionState = { revision: -1, items: [], drafts: new Map(), loading: true, error: null };
  const store = writable(state);
  let transport: HumanDecisionTransport | null = null;
  const publish = (patch: Partial<HumanDecisionState>) => { state = { ...state, ...patch }; store.set(state); };
  function apply(snapshot: HumanDecisionSnapshot): void {
    if (snapshot.revision <= state.revision) return;
    const items = [...snapshot.items].sort((a, b) => a.createdAt - b.createdAt || a.target.instanceId.localeCompare(b.target.instanceId) || a.target.requestId.localeCompare(b.target.requestId));
    const drafts = new Map(items.map(item => {
      const key = decisionKey(item.target);
      return [key, state.drafts.get(key) ?? initialDecisionDraft(item)] as const;
    }));
    publish({ revision: snapshot.revision, items, drafts, loading: false, error: null });
  }
  function updateDraft(target: HumanDecisionTarget, update: (draft: DecisionDraft) => DecisionDraft): void {
    const key = decisionKey(target), draft = state.drafts.get(key);
    if (draft) publish({ drafts: new Map(state.drafts).set(key, update(draft)) });
  }
  async function refresh(): Promise<void> {
    const active = transport;
    if (!active) return;
    try {
      const snapshot = await active.list();
      if (transport === active) { apply(snapshot); publish({ loading: false, error: null }); }
    } catch (error) { if (transport === active) publish({ loading: false, error: errorText(error) }); }
  }
  async function connect(next: HumanDecisionTransport): Promise<() => void> {
    transport = next;
    // Subscribe first: a late list can never resurrect a retired request.
    let unlisten: () => void;
    try { unlisten = await next.listen(snapshot => { if (transport === next) apply(snapshot); }); }
    catch (error) {
      if (transport === next) { transport = null; publish({ loading: false, error: errorText(error) }); }
      throw error;
    }
    if (transport !== next) { unlisten(); return () => {}; }
    await refresh();
    return () => { unlisten(); if (transport === next) transport = null; };
  }
  async function submit(target: HumanDecisionTarget, response: HumanDecisionResponse): Promise<void> {
    const active = transport, key = decisionKey(target), draft = state.drafts.get(key);
    const item = state.items.find(item => decisionKey(item.target) === key);
    if (!active || !item || !canAnswer(item) || !draft || draft.submitting || draft.awaitingResolution || draft.loadingImages) return;
    updateDraft(target, draft => ({ ...draft, submitting: true, error: null }));
    try {
      await active.respond(target, response);
      updateDraft(target, draft => ({ ...draft, submitting: false, awaitingResolution: true }));
    } catch (error) {
      updateDraft(target, draft => ({ ...draft, submitting: false, error: `Answer not confirmed: ${errorText(error)}. Refresh to check whether it resolved; answers are never retried automatically.` }));
    }
    // Acceptance is not authority to retire a card. Only a full backend snapshot can do that.
    if (transport === active) await refresh();
  }
  function connectionError(error: unknown): void { publish({ loading: false, error: errorText(error) }); }
  return { subscribe: store.subscribe, connect, refresh, submit, updateDraft, connectionError };
}
export const humanDecisions = createHumanDecisionStore();
