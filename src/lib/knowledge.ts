import { invoke } from "@tauri-apps/api/core";
import { writable, type Readable } from "svelte/store";

// Frontend contract of the knowledge backend (`src-tauri/src/knowledge*.rs`): vault registration, the Markdown vault reader, Graphify
// artifacts and the optional Cognee UI. Every command is main-window only and takes camelCase arguments. Nothing here starts a model,
// a CLI or a service; probes are passive and a capability is never a negative claim when a probe failed (`unreadable`/`unknown`).

// ---------- vaults ----------

export type VaultMode = "obsidian" | "markdown";
export type KnowledgeScope = { kind: "global" } | { kind: "project"; repositoryRoot: string };
/** What a human asks for: the backend derives a project's repository root from `folder`; one that is sent must agree with it. */
export type ScopeRequest = { kind: "global" } | { kind: "project"; repositoryRoot?: string };
export interface VaultBinding { vaultId: string; root: string; mode: VaultMode; scope: KnowledgeScope }

export interface ObsidianCapability {
  state: "absent" | "unreadable" | "installed";
  version: string | null;
  executable: string | null;
  /** Vault folders Obsidian already knows (read-only). Picker options only: never adopted without an explicit human choice. */
  registeredVaults: string[];
  message: string | null;
  /** The human declined Obsidian for the shown project: do not ask again. */
  declinedForProject: boolean;
}
export type GraphifyState = "absent" | "present" | "malformed" | "foreign" | "unverified" | "unavailable";
export interface GraphifyCapability {
  present: boolean;
  state: GraphifyState;
  /** Root the output claims (`.graphify_root`), when readable. */
  sourceRoot: string | null;
  /** Null on a passive probe: an artifact is admitted only when Graphify is clicked. */
  artifactId: string | null;
  artifactHash: string | null;
  message: string | null;
}
export type CogneeUiState = "ready" | "missingFrontend" | "missingDependencies" | "unreadable" | "unsupported";
/** Whether the installed Cognee web interface can be started by Open. Passive file metadata only; Open revalidates. */
export interface CogneeUiCapability { state: CogneeUiState; message: string | null }
export interface CogneeCapability { state: "installed" | "absent" | "unreadable"; installed: boolean | null; version: string | null; installationFolder: string | null; message: string | null; ui: CogneeUiCapability }
export type CogneeOpenState = "opened" | "missingComponent" | "backendUnavailable" | "portBusy" | "unsupported" | "startingFailed" | "readinessTimeout" | "openFailed";
/** The answer to one Open click; every state but `opened` explains why. `url` is set when a UI is ready (also for `openFailed`). */
export interface CogneeOpenResult { state: CogneeOpenState; url: string | null; message: string | null }
export interface KnowledgeCapabilities {
  /** The folder as asked (trimmed); a late answer is fenced against the project now shown. */
  folder: string | null;
  /** Canonical backend repository identity of `folder` (null for global probes and folders that are not projects). */
  projectKey: string | null;
  obsidian: ObsidianCapability;
  graphify: GraphifyCapability;
  cognee: CogneeCapability;
  /** The bindings that apply to `folder` (its repository vault) and the single global vault. */
  projectVault: VaultBinding | null;
  globalVault: VaultBinding | null;
  /** Offered destinations: `<repositoryRoot>/Knowledge` and `<home>/OMP Knowledge/Global`. */
  suggestedProjectRoot: string | null;
  suggestedGlobalRoot: string | null;
}
export interface VaultPreparation {
  token: string;
  scope: KnowledgeScope;
  mode: VaultMode;
  root: string;
  exists: boolean;
  /** Existing notes under the root that stay untouched. */
  existingMarkdownCount: number;
  /** Managed paths already occupied, overlapping bindings and other reasons the destination cannot be used as is. */
  conflicts: string[];
  notes: string[];
}
export interface OpenNoteResult {
  /** True when the operating system accepted the Obsidian URI. */
  opened: boolean;
  uri: string;
  /** The vault folder is not registered with Obsidian yet: show `vaultPath` and offer the vault manager. */
  needsVaultRegistration: boolean;
  vaultPath: string;
  message: string | null;
}

export type VaultEntryKind = "folder" | "markdown" | "other";
export interface VaultEntry {
  /** Vault-relative, forward slashes. */
  path: string;
  name: string;
  kind: VaultEntryKind;
  bytes: number | null;
  /** SHA-256 of the whole file; null for folders. */
  hash: string | null;
}
/** One directory level, folders first, then names in a stable case-insensitive order. */
export interface VaultEntryPage { vaultId: string; revision: number; prefix: string; total: number; offset: number; items: VaultEntry[]; nextOffset: number | null }
/** At most 64 KiB, cut on a UTF-8 boundary. `hash`/`byteLength` describe the whole document, so pages of different revisions cannot be mixed. */
export interface VaultDocumentPage {
  vaultId: string; revision: number; path: string; hash: string; byteLength: number;
  byteOffset: number; length: number; nextOffset: number | null; text: string;
  startsMidLine: boolean; fence: { marker: string; length: number } | null; frontmatter: boolean; lineNumber: number;
}
export type VaultLinkKind = "wiki" | "markdown" | "embed";
export type VaultLinkStatus = "resolved" | "missing" | "ambiguous" | "nonDocument" | "external" | "anchor";
/** One link occurrence found outside code spans and fences. `source` is the note that contains it; `target` is a note path only when resolved. */
export interface VaultLinkRecord {
  id: string;
  source: string;
  target: string | null;
  kind: VaultLinkKind;
  status: VaultLinkStatus;
  /** The text as written (`Folder/Note`, `./a.md`), and what is shown for it. */
  raw: string;
  label: string | null;
  /** Heading text or `^block` reference after `#`. */
  fragment: string | null;
  line: number;
  /** All matching notes when ambiguous. */
  candidates: string[];
}
export interface VaultNoteRecord { path: string; title: string; bytes: number; hash: string; outgoing: number; incoming: number }
export interface VaultSearchHit { path: string; title: string; line: number | null; snippet: string }
export type VaultLinkQuery =
  | { kind: "notes"; offset: number; limit: number }
  | { kind: "links"; offset: number; limit: number }
  | { kind: "search"; text: string; offset: number; limit: number }
  | { kind: "backlinks"; path: string; offset: number; limit: number }
  | { kind: "neighbors"; path: string; offset: number; limit: number }
  /** Resolves up to 200 targets exactly as the index does: relative to the note, then vault path, then unique vault basename. */
  | { kind: "resolve"; fromPath: string; targets: string[] };
export interface VaultResolution { raw: string; status: VaultLinkStatus; path: string | null; candidates: string[] }
export interface VaultQueryPage<T> { vaultId: string; revision: number; total: number; offset: number; items: T[]; nextOffset: number | null }
export interface VaultRefresh { vaultId: string; revision: number; notes: number; links: number; unreadable: number }

// ---------- Graphify ----------

export type GraphifySection = "metadata" | "nodes" | "links" | "hyperedges" | "communities";
export type GraphifyConfidence = "EXTRACTED" | "INFERRED" | "AMBIGUOUS";
/** Numeric and text ids are different identities even when they print alike. */
export interface GraphifyRef { id: string; idKind: "text" | "number" }
/** A validation finding the backend recorded while admitting the artifact (nothing is silently dropped). */
export interface GraphifyIssue { severity: string; section: string; ordinal: number | null; message: string }
export interface GraphifyArtifact {
  artifactId: string;
  /** SHA-256 of the artifact bytes: proves byte identity, not freshness against the current source. */
  artifactHash: string;
  folder: string;
  sourceRoot: string | null;
  provenance: "verified" | "unverified";
  directed: boolean;
  multigraph: boolean;
  counts: { nodes: number; links: number; hyperedges: number; communities: number };
  /** Build/metadata fields as found in the file. */
  metadata: Record<string, unknown>;
  bytes: number;
  /** Present when metadata was too large to inline: states where the complete app-private copy and the byte-exact graph are kept. */
  metadataReceipt?: { path: string; rawGraphPath: string; bytes: number; sha256: string; message: string } | null;
  /** Validation findings the backend recorded (duplicate/dangling records, ...). */
  issues: GraphifyIssue[];
  issueCount: number;
  outputKind: string;
  provenanceCounts: Record<string, number>;
}
export interface GraphifyNode {
  ref: GraphifyRef;
  label: string;
  fileType: string | null;
  sourceFile: string | null;
  sourceLocation: string | null;
  community: number | null;
  origin: string | null;
  attributes: Record<string, unknown>;
}
export interface GraphifyLink {
  ordinal: number;
  /** `graphify:<artifactHash>:link:<ordinal>` unless the file carries an id of its own (`nativeId`). */
  id: string;
  nativeId: string | null;
  source: GraphifyRef;
  target: GraphifyRef;
  /** Multigraph key as found, when present. */
  key: string | null;
  relation: string;
  directed: boolean;
  confidence: GraphifyConfidence | null;
  confidenceScore: number | null;
  sourceFile: string | null;
  sourceLocation: string | null;
  origin: string | null;
  attributes: Record<string, unknown>;
  /** A link whose endpoint is not a node: kept as a record, never drawn. */
  dangling: boolean;
  /** Hydrated endpoints, present on `neighbors` query rows. */
  sourceNode?: GraphifyNode | null;
  targetNode?: GraphifyNode | null;
}
export interface GraphifyHyperedge {
  id: string;
  label: string;
  relation: string | null;
  nodeIds: GraphifyRef[];
  confidence: GraphifyConfidence | null;
  sourceFile: string | null;
  attributes: Record<string, unknown>;
}
export interface GraphifyCommunity { id: number; label: string | null; size: number }
export interface GraphifyPage<T> { artifactId: string; artifactHash: string; section: GraphifySection; total: number; offset: number; items: T[]; nextOffset: number | null }
export type GraphifyQuery =
  | { kind: "search"; text: string; offset: number; limit: number }
  | { kind: "neighbors"; id: GraphifyRef; direction: "in" | "out" | "both"; offset: number; limit: number }
  | { kind: "node"; id: GraphifyRef }
  | { kind: "communityMembers"; community: number; offset: number; limit: number }
  | { kind: "hyperedgesOf"; id: GraphifyRef; offset: number; limit: number };
export type GraphifySearchItem =
  | { kind: "node"; node: GraphifyNode }
  | { kind: "link"; link: GraphifyLink }
  | { kind: "hyperedge"; hyperedge: GraphifyHyperedge };
export type GraphifyQueryItem = GraphifyNode | GraphifyLink | GraphifyHyperedge | GraphifySearchItem;
export interface GraphifyQueryPage { artifactId: string; artifactHash: string; kind: GraphifyQuery["kind"]; total: number; offset: number; items: GraphifyQueryItem[]; nextOffset: number | null }

// ---------- commands ----------

export const knowledgeApi = {
  getCapabilities: (folder: string | null) => invoke<KnowledgeCapabilities>("get_knowledge_capabilities", { folder }),
  configureObsidian: (executable: string | null) => invoke<void>("configure_obsidian_executable", { executable }),
  listVaults: () => invoke<VaultBinding[]>("list_knowledge_vaults"),
  prepareVault: (scope: ScopeRequest, folder: string | null, destination: string, mode: VaultMode) => invoke<VaultPreparation>("prepare_knowledge_vault", { scope, folder, destination, mode }),
  confirmVault: (token: string) => invoke<VaultBinding>("confirm_knowledge_vault", { token }),
  openNote: (vaultId: string, path: string | null) => invoke<OpenNoteResult>("open_knowledge_note", { vaultId, path }),
  /** Opens `obsidian://choose-vault` so the user registers a folder with Obsidian. Never edits Obsidian's own settings. */
  openObsidianVaultManager: () => invoke<void>("open_obsidian_vault_manager"),
  /** Opens a credential-free https link with the system handler (no shell). */
  openLink: (url: string) => invoke<void>("open_knowledge_link", { url }),
  openObsidianDownload: () => invoke<void>("open_obsidian_download"),
  /** Remembers that the human does not want Obsidian for this project, then returns the refreshed capabilities. */
  recordObsidianDecline: async (folder: string): Promise<KnowledgeCapabilities> => {
    await invoke<void>("record_obsidian_decline", { folder });
    return invoke<KnowledgeCapabilities>("get_knowledge_capabilities", { folder });
  },
  listEntries: (vaultId: string, prefix: string, offset: number, limit: number) => invoke<VaultEntryPage>("list_vault_entries", { vaultId, prefix, offset, limit }),
  readDocument: (vaultId: string, path: string, offset: number) => invoke<VaultDocumentPage>("read_vault_document", { vaultId, path, offset }),
  queryLinks: <T>(vaultId: string, query: VaultLinkQuery) => invoke<VaultQueryPage<T>>("query_vault_links", { vaultId, query }),
  resolveLinks: (vaultId: string, fromPath: string, targets: string[]) => invoke<VaultQueryPage<VaultResolution>>("query_vault_links", { vaultId, query: { kind: "resolve", fromPath, targets } satisfies VaultLinkQuery }),
  refreshIndex: (vaultId: string) => invoke<VaultRefresh>("refresh_vault_index", { vaultId }),
  openGraphifyArtifact: (folder: string) => invoke<GraphifyArtifact>("open_graphify_artifact", { folder }),
  checkGraphifyArtifact: (artifactId: string) => invoke<{ artifactId: string; current: boolean; message: string | null }>("check_graphify_artifact", { artifactId }),
  registerGraphifyOutput: (folder: string, directory: string) => invoke<KnowledgeCapabilities>("register_graphify_output", { folder, directory }),
  readGraphify: <T>(artifactId: string, section: GraphifySection, offset: number, limit: number) => invoke<GraphifyPage<T>>("read_graphify_graph", { artifactId, section, offset, limit }),
  queryGraphify: (artifactId: string, query: GraphifyQuery) => invoke<GraphifyQueryPage>("query_graphify_graph", { artifactId, query }),
  /** Explicit click only: revalidates the installed Cognee UI, starts this app's own frontend when needed and opens it. Always an answer. */
  openCognee: () => invoke<CogneeOpenResult>("open_cognee_ui"),
};
export type KnowledgeApi = typeof knowledgeApi;

// ---------- parent-lived capability store ----------

export interface KnowledgeState {
  /** Folder the capabilities were probed for (null: global only). */
  folder: string | null;
  capabilities: KnowledgeCapabilities | null;
  loading: boolean;
  error: string | null;
  /** Counts every probe, so a view can tell a re-check from a stale value. */
  probes: number;
}
export interface KnowledgeStore extends Readable<KnowledgeState> {
  /** Passive probe for `folder`; a late answer for an older probe is dropped. Use for startup, folder entry, window focus and Check again. */
  probe(folder: string | null): Promise<KnowledgeCapabilities | null>;
  /** Replaces the capabilities with a backend answer that already includes them (for example after registering an output). */
  adopt(capabilities: KnowledgeCapabilities): void;
}

function errorText(error: unknown): string {
  if (error instanceof Error) return error.message;
  return typeof error === "string" ? error : JSON.stringify(error);
}

export function createKnowledgeStore(backend: Pick<KnowledgeApi, "getCapabilities">): KnowledgeStore {
  let state: KnowledgeState = { folder: null, capabilities: null, loading: false, error: null, probes: 0 };
  const store = writable(state);
  const commit = (patch: Partial<KnowledgeState>): void => { state = { ...state, ...patch }; store.set(state); };
  let sequence = 0;
  return {
    subscribe: store.subscribe,
    async probe(folder) {
      const mine = ++sequence;
      commit({ folder, loading: true });
      try {
        const capabilities = await backend.getCapabilities(folder);
        if (mine !== sequence) return null;
        commit({ capabilities, loading: false, error: null, probes: state.probes + 1 });
        return capabilities;
      } catch (error) {
        // A failed probe says nothing about availability: the previous capabilities stay and the error is shown as such.
        if (mine === sequence) commit({ loading: false, error: errorText(error), probes: state.probes + 1 });
        return null;
      }
    },
    adopt(capabilities) {
      sequence++;
      commit({ capabilities, folder: capabilities.folder, loading: false, error: null, probes: state.probes + 1 });
    },
  };
}

/** Parent-lived: probes are passive and keyed by the shown folder. */
export const knowledge = createKnowledgeStore(knowledgeApi);

// ---------- pure helpers ----------

/** Vault of the scope that applies to the shown folder: its project vault, otherwise the global one. */
export const projectVaultOf = (state: KnowledgeState): VaultBinding | null => state.capabilities?.projectVault ?? null;
export const obsidianInstalled = (capabilities: KnowledgeCapabilities | null): boolean => capabilities?.obsidian.state === "installed";
/** The folder as the backend echoes it (`folder.trim()`), or null when no project is shown. */
export const projectFolder = (folder: string | null | undefined): string | null => folder?.trim() || null;
/**
 * The capabilities that were accepted for exactly this project. A retained answer for another project (while a probe for this
 * one is in flight or after it failed) is never used: the echoed `folder` must match. A failed re-probe of the same project keeps
 * its previous answer, which is the intended "an error never hides what was found" behavior.
 */
export const capabilitiesFor = (state: Pick<KnowledgeState, "capabilities">, folder: string | null | undefined): KnowledgeCapabilities | null => {
  const wanted = projectFolder(folder);
  return wanted !== null && state.capabilities?.folder === wanted ? state.capabilities : null;
};
/** Graphify is reachable from every shown project; what it opens depends on `graphifyAction`. */
export const graphifyVisible = (folder: string | null | undefined): boolean => projectFolder(folder) !== null;
/**
 * What the Graphify button does: a valid admitted output (verified, or local and labelled unverified) opens the graph view; anything
 * else (absent, foreign, malformed, unavailable, or not probed for this project yet) opens the access dialog with its diagnostic.
 */
export const graphifyAction = (capabilities: KnowledgeCapabilities | null): "view" | "access" =>
  capabilities && capabilities.graphify.present && (capabilities.graphify.state === "present" || capabilities.graphify.state === "unverified") ? "view" : "access";
/** Cognee button: positive installed evidence only (an unreadable installation is a diagnostic elsewhere, not proof). */
export const cogneeVisible = (capabilities: KnowledgeCapabilities | null): boolean => capabilities?.cognee.installed === true;
