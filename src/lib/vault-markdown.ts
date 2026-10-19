import MarkdownIt from "markdown-it";
import type { Env, MarkdownIt as MarkdownItInstance, StateCore, StateInline, Token } from "markdown-it";
import type { VaultResolution } from "./knowledge";

// Rendering of vault notes. Hostile Markdown is the normal case here, so: raw HTML is off, every anchor is `href="#"` (the real target
// travels in `data-*` and is acted on only by the host's click handler), images and Obsidian embeds of non-notes never load anything,
// and code/Mermaid/Dataview blocks are shown as text and never run. The output is additionally sanitized by the host (DOMPurify).

export const TARGET_BATCH = 200;
export const MAX_EMBED_DEPTH = 4;
/** An embedded note is read in whole pages up to this many bytes, then the embed says it continues. */
export const EMBED_BYTE_BUDGET = 256 * 1024;

/** State shared by all segments of one document, so slugs stay unique and link references defined earlier keep working. */
export interface RenderEnv {
  fromPath: string;
  slugs: Map<string, number>;
  resolutions: Map<string, VaultResolution>;
  embeds: Map<string, EmbedResult>;
  /** markdown-it reference definitions shared by every rendered segment of this document. */
  references?: Env["references"];
}
export interface EmbedResult { html: string; notice: string | null }

export const NOT_A_DOCUMENT = "Not a Markdown vault document";

const escapeHtml = (text: string): string => text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");

export function slugify(text: string): string {
  return text.normalize("NFKC").toLowerCase().trim().replace(/[^\p{L}\p{N}\s-]/gu, "").replace(/\s+/g, "-");
}
/** Last heading level of an Obsidian nested reference (`Note#Section#Sub`). */
export const fragmentSlug = (fragment: string): string => slugify(fragment.split("#").filter(Boolean).pop() ?? fragment);

function safeDecode(text: string): string {
  try { return decodeURIComponent(text); } catch { return text; }
}

export type HrefClass =
  | { kind: "anchor"; fragment: string }
  | { kind: "external"; url: string; https: boolean }
  | { kind: "scheme" }
  | { kind: "relative"; path: string; fragment: string | null };

/** Classifies a Markdown link destination. Only relative paths are candidates for vault notes. */
export function classifyHref(href: string): HrefClass {
  const value = href.trim();
  if (value.startsWith("#")) return { kind: "anchor", fragment: safeDecode(value.slice(1)) };
  const scheme = /^([a-zA-Z][a-zA-Z0-9+.-]*):/.exec(value);
  if (scheme) {
    const name = scheme[1].toLowerCase();
    if (name === "https" || name === "http") return { kind: "external", url: value, https: name === "https" };
    return { kind: "scheme" };
  }
  if (value.startsWith("//")) return { kind: "scheme" };
  const hash = value.indexOf("#");
  const path = safeDecode(hash < 0 ? value : value.slice(0, hash)).replace(/\\/g, "/");
  const fragment = hash < 0 ? null : safeDecode(value.slice(hash + 1));
  return path === "" ? { kind: "anchor", fragment: fragment ?? "" } : { kind: "relative", path, fragment };
}

// ---------- markdown-it plugin ----------

interface WikiMeta { embed: boolean; target: string; fragment: string | null; label: string | null; raw: string }

function wikilinkRule(state: StateInline, silent: boolean): boolean {
  const src = state.src;
  let pos = state.pos;
  const embed = src.charCodeAt(pos) === 0x21;
  if (embed) pos++;
  if (src.charCodeAt(pos) !== 0x5b || src.charCodeAt(pos + 1) !== 0x5b) return false;
  const start = pos + 2;
  const end = src.indexOf("]]", start);
  if (end < 0 || end + 2 > state.posMax) return false;
  const inner = src.slice(start, end);
  if (!inner.trim() || inner.includes("\n") || inner.includes("[") || inner.includes("]")) return false;
  let bar = -1;
  for (let i = 0; i < inner.length; i++) if (inner[i] === "|" && inner[i - 1] !== "\\") { bar = i; break; }
  const targetPart = (bar < 0 ? inner : inner.slice(0, bar)).replace(/\\\|/g, "|");
  const label = bar < 0 ? null : inner.slice(bar + 1).replace(/\\\|/g, "|").trim() || null;
  const hash = targetPart.indexOf("#");
  const target = (hash < 0 ? targetPart : targetPart.slice(0, hash)).trim();
  const fragment = hash < 0 ? null : targetPart.slice(hash + 1).trim() || null;
  if (!target && !fragment) return false;
  if (!silent) {
    const token = state.push("wikilink", "", 0);
    token.content = inner;
    token.meta = { embed, target, fragment, label, raw: inner } satisfies WikiMeta;
  }
  state.pos = end + 2;
  return true;
}

const inlineText = (token: Token | undefined): string => {
  if (!token) return "";
  if (!token.children) return token.content;
  let out = "";
  for (const child of token.children) if (child.type === "text" || child.type === "code_inline") out += child.content;
  return out;
};

function postRule(state: StateCore): boolean {
  const env = state.env as unknown as RenderEnv;
  const tokens = state.tokens;
  for (let i = 0; i < tokens.length; i++) {
    const token = tokens[i];
    if (token.type === "heading_open") {
      const text = inlineText(tokens[i + 1]);
      const base = slugify(text) || "section";
      const seen = env.slugs.get(base) ?? 0;
      env.slugs.set(base, seen + 1);
      token.attrSet("data-anchor", seen === 0 ? base : `${base}-${seen}`);
    } else if (token.type === "paragraph_open") {
      // Obsidian block identifier: a trailing ` ^id` names the paragraph and is not shown.
      const inline = tokens[i + 1];
      const last = inline?.children?.[inline.children.length - 1];
      const found = last && last.type === "text" ? /(?:^|\s)\^([A-Za-z0-9-]+)$/.exec(last.content) : null;
      if (found) {
        token.attrSet("data-block", found[1]);
        last!.content = last!.content.slice(0, last!.content.length - found[0].length);
        if (inline.content.endsWith(found[0])) inline.content = inline.content.slice(0, inline.content.length - found[0].length);
      }
      // Task list item: a leading `[ ]`/`[x]` in the first paragraph of a list item.
      const open = tokens[i - 1];
      const first = inline?.children?.[0];
      if (open?.type === "list_item_open" && first?.type === "text") {
        const task = /^\[([ xX])\]\s+/.exec(first.content);
        if (task) {
          first.content = first.content.slice(task[0].length);
          const box = new state.Token("task_box", "", 0);
          box.meta = { checked: task[1] !== " " };
          inline.children!.unshift(box);
          open.attrJoin("class", "task-list-item");
        }
      }
    }
  }
  return true;
}

function resolutionOf(env: RenderEnv, raw: string): VaultResolution | undefined { return env.resolutions.get(raw); }

function inertSpan(kind: string, title: string, label: string): string {
  return `<span class="vault-link ${kind}" title="${escapeHtml(title)}">${escapeHtml(label)}</span>`;
}

function describe(resolution: VaultResolution | undefined): { cls: string; title: string } {
  if (!resolution) return { cls: "pending", title: "Link not checked" };
  switch (resolution.status) {
    case "missing": return { cls: "missing", title: "No note with that name in this vault" };
    case "ambiguous": return { cls: "ambiguous", title: `Several notes match: ${resolution.candidates.join(", ")}` };
    case "nonDocument": return { cls: "inert", title: NOT_A_DOCUMENT };
    default: return { cls: "inert", title: "This link is not opened from here" };
  }
}

function noteAnchor(path: string, fragment: string | null, labelHtml: string): string {
  return `<a href="#" class="vault-link note" data-kind="note" data-path="${escapeHtml(path)}"${fragment ? ` data-fragment="${escapeHtml(fragment)}"` : ""}>${labelHtml}</a>`;
}

export function createMarkdown(): MarkdownItInstance {
  const md = new MarkdownIt({ html: false, linkify: false, typographer: false, breaks: false });
  md.disable(["html_block", "html_inline"], true);
  md.inline.ruler.before("link", "wikilink", wikilinkRule);
  md.core.ruler.push("vault_post", postRule);
  const rules = md.renderer.rules;

  rules.task_box = (tokens, idx) => `<span class="task-box" role="checkbox" aria-disabled="true" aria-checked="${tokens[idx].meta?.checked ? "true" : "false"}">${tokens[idx].meta?.checked ? "☑" : "☐"}</span> `;

  rules.wikilink = (tokens, idx, _options, rawEnv) => {
    const env = rawEnv as unknown as RenderEnv;
    const meta = tokens[idx].meta as unknown as WikiMeta;
    const shown = meta.label ?? (meta.target ? (meta.fragment ? `${meta.target} › ${meta.fragment.split("#").pop()}` : meta.target) : meta.fragment ?? "");
    if (!meta.target) return `<a href="#" class="vault-link anchor" data-kind="anchor" data-fragment="${escapeHtml(meta.fragment ?? "")}">${escapeHtml(shown)}</a>`;
    const resolution = resolutionOf(env, meta.target);
    if (resolution?.status === "resolved" && resolution.path) {
      if (meta.embed) {
        const embed = env.embeds.get(`${resolution.path}#${meta.fragment ?? ""}`);
        const head = `<div class="vault-embed-head">${noteAnchor(resolution.path, meta.fragment, escapeHtml(shown))}</div>`;
        const notice = embed?.notice ? `<p class="vault-embed-notice">${escapeHtml(embed.notice)}</p>` : "";
        return `<div class="vault-embed" data-embed-path="${escapeHtml(resolution.path)}">${head}${embed ? `<div class="vault-embed-body">${embed.html}</div>` : ""}${notice}</div>`;
      }
      return noteAnchor(resolution.path, meta.fragment, escapeHtml(shown));
    }
    const { cls, title } = describe(resolution);
    return inertSpan(cls, title, shown);
  };

  // Links: every anchor points at "#"; the target lives in data-* for the host's click handler.
  rules.link_open = (tokens, idx, _options, rawEnv) => {
    const env = rawEnv as unknown as RenderEnv;
    const href = String(tokens[idx].attrGet("href") ?? "");
    const target = classifyHref(href);
    let html: string;
    let tag: "a" | "span" = "a";
    if (target.kind === "anchor") html = `<a href="#" class="vault-link anchor" data-kind="anchor" data-fragment="${escapeHtml(target.fragment)}">`;
    else if (target.kind === "external" && target.https) html = `<a href="#" class="vault-link external" data-kind="external" data-url="${escapeHtml(target.url)}" title="${escapeHtml(target.url)}">`;
    else if (target.kind === "external") { tag = "span"; html = `<span class="vault-link inert" title="Only https links can be opened">`; }
    else if (target.kind === "scheme") { tag = "span"; html = `<span class="vault-link inert" title="This kind of link is never opened">`; }
    else {
      const resolution = resolutionOf(env, target.path);
      if (resolution?.status === "resolved" && resolution.path) html = `<a href="#" class="vault-link note" data-kind="note" data-path="${escapeHtml(resolution.path)}"${target.fragment ? ` data-fragment="${escapeHtml(target.fragment)}"` : ""}>`;
      else { tag = "span"; const { cls, title } = describe(resolution); html = `<span class="vault-link ${cls}" title="${escapeHtml(title)}">`; }
    }
    tokens[idx].meta = { ...(tokens[idx].meta ?? {}), tag };
    return html;
  };
  rules.link_close = (tokens, idx) => {
    // Pair with the nearest unclosed link_open.
    let depth = 0;
    for (let i = idx - 1; i >= 0; i--) {
      if (tokens[i].type === "link_close") depth++;
      else if (tokens[i].type === "link_open") { if (depth === 0) return tokens[i].meta?.tag === "span" ? "</span>" : "</a>"; depth--; }
    }
    return "</a>";
  };

  rules.image = (tokens, idx) => {
    const alt = inlineText(tokens[idx]);
    return `<span class="vault-link inert vault-image" title="Images are not loaded">[image${alt ? `: ${escapeHtml(alt)}` : ""}]</span>`;
  };

  const plainFence = rules.fence!;
  rules.fence = (tokens, idx, options, env, self) => {
    const lang = (tokens[idx].info.trim().split(/\s+/)[0] ?? "").toLowerCase();
    const body = plainFence(tokens, idx, options, env, self);
    return ["mermaid", "dataview", "dataviewjs", "js", "javascript", "html", "svg", "tasks"].includes(lang)
      ? `<figure class="vault-code">${body}<figcaption>${escapeHtml(lang)} · shown as text, never run</figcaption></figure>`
      : body;
  };
  return md;
}

// ---------- collection ----------

export interface CollectedRefs { targets: string[]; embeds: { raw: string; fragment: string | null }[] }

/** Link targets a document needs resolved (deduplicated) and the embeds it contains. */
export function collectRefs(tokens: readonly Token[]): CollectedRefs {
  const targets = new Set<string>();
  const embeds: { raw: string; fragment: string | null }[] = [];
  const visit = (list: readonly Token[]): void => {
    for (const token of list) {
      if (token.type === "wikilink") {
        const meta = token.meta as unknown as WikiMeta;
        if (meta.target) { targets.add(meta.target); if (meta.embed) embeds.push({ raw: meta.target, fragment: meta.fragment }); }
      } else if (token.type === "link_open") {
        const target = classifyHref(String(token.attrGet("href") ?? ""));
        if (target.kind === "relative") targets.add(target.path);
      }
      if (token.children) visit(token.children);
    }
  };
  visit(tokens);
  return { targets: [...targets], embeds };
}

export function newEnv(fromPath: string): RenderEnv {
  return { fromPath, slugs: new Map(), resolutions: new Map(), embeds: new Map() };
}

export interface RenderServices {
  /** Resolves up to `TARGET_BATCH` targets through the vault index (the single authority for link resolution). */
  resolve: (fromPath: string, targets: string[]) => Promise<VaultResolution[]>;
  /** Fetches and renders the embedded note or section; the host guards depth and cycles. */
  loadEmbed: (path: string, fragment: string | null) => Promise<EmbedResult>;
  /** Called between awaits; a stale render stops. */
  cancelled: () => boolean;
}

/** Parses, resolves links through the index, expands embeds and renders one segment. Returns null when cancelled. */
export async function renderSegment(md: MarkdownItInstance, source: string, env: RenderEnv, services: RenderServices): Promise<string | null> {
  const tokens = md.parse(source, env as unknown as Env);
  const refs = collectRefs(tokens);
  const need = refs.targets.filter(target => !env.resolutions.has(target));
  for (let offset = 0; offset < need.length; offset += TARGET_BATCH) {
    const batch = need.slice(offset, offset + TARGET_BATCH);
    const answers = await services.resolve(env.fromPath, batch);
    if (services.cancelled()) return null;
    batch.forEach((raw, i) => {
      const answer = answers[i];
      env.resolutions.set(raw, answer && answer.raw === raw ? answer : { raw, status: "missing", path: null, candidates: [] });
    });
  }
  for (const embed of refs.embeds) {
    const resolution = env.resolutions.get(embed.raw);
    if (resolution?.status !== "resolved" || !resolution.path) continue;
    const key = `${resolution.path}#${embed.fragment ?? ""}`;
    if (env.embeds.has(key)) continue;
    const loaded = await services.loadEmbed(resolution.path, embed.fragment);
    if (services.cancelled()) return null;
    env.embeds.set(key, loaded);
  }
  return md.renderer.render(tokens, md.options, env as unknown as Env);
}

// ---------- long documents ----------

/** UTF-8 pages are rendered as they arrive: everything before the last top-level block is final, the last block waits for more text. */
export function splitStable(md: MarkdownItInstance, pending: string, final: boolean): { stable: string; tail: string } {
  if (final) return { stable: pending, tail: "" };
  const tokens = md.parse(pending, newEnv("") as unknown as Env);
  let last = -1;
  for (const token of tokens) if (token.level === 0 && token.map && (token.nesting === 1 || token.nesting === 0)) last = token.map[0];
  if (last <= 0) return { stable: "", tail: pending };
  const lines = pending.split("\n");
  return { stable: `${lines.slice(0, last).join("\n")}\n`, tail: lines.slice(last).join("\n") };
}

export interface Frontmatter { properties: string; body: string }
/** YAML properties block at the very start. Null when absent or not yet closed. */
export function splitFrontmatter(text: string): Frontmatter | null {
  if (!text.startsWith("---\n")) return null;
  const closing = /\n---[ \t]*(?:\n|$)/.exec(text.slice(3));
  if (!closing) return null;
  const end = 3 + closing.index + closing[0].length;
  return { properties: text.slice(4, 3 + closing.index), body: text.slice(end) };
}
export const frontmatterPending = (text: string): boolean => text.startsWith("---\n") && splitFrontmatter(text) === null;

export function renderProperties(properties: string): string {
  return `<details class="vault-properties"><summary>Properties</summary><pre>${escapeHtml(properties)}</pre></details>`;
}

const FENCE = /^ {0,3}(`{3,}|~{3,})/;
const HEADING = /^ {0,3}(#{1,6})[ \t]+(.*?)[ \t]*#*[ \t]*$/;

/** Section of a note named by `#Heading` (up to the next heading of the same or a higher level) or `#^block`. Null when not found. */
export function extractFragment(text: string, fragment: string): string | null {
  const lines = text.replace(/\r\n?/g, "\n").split("\n");
  const name = fragment.split("#").filter(Boolean).pop() ?? fragment;
  if (name.startsWith("^")) {
    const id = name.slice(1);
    const ending = new RegExp(`(?:^|\\s)\\^${id.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}\\s*$`);
    let fence: string | null = null;
    let start = 0;
    for (let i = 0; i <= lines.length; i++) {
      const line = i < lines.length ? lines[i] : "";
      const opened = FENCE.exec(line);
      if (opened && fence === null) fence = opened[1][0];
      else if (opened && fence === opened[1][0]) fence = null;
      if (fence === null && (line.trim() === "" || i === lines.length)) {
        const block = lines.slice(start, i);
        if (block.length && ending.test(block[block.length - 1])) {
          block[block.length - 1] = block[block.length - 1].replace(ending, "");
          return block.join("\n");
        }
        start = i + 1;
      }
    }
    return null;
  }
  const wanted = slugify(name);
  let fence: string | null = null;
  let level = 0;
  let from = -1;
  for (let i = 0; i < lines.length; i++) {
    const opened = FENCE.exec(lines[i]);
    if (opened && fence === null) { fence = opened[1][0]; continue; }
    if (opened && fence === opened[1][0]) { fence = null; continue; }
    if (fence !== null) continue;
    const heading = HEADING.exec(lines[i]);
    if (!heading) continue;
    if (from < 0) {
      if (slugify(heading[2]) === wanted) { from = i; level = heading[1].length; }
    } else if (heading[1].length <= level) return lines.slice(from, i).join("\n");
  }
  return from < 0 ? null : lines.slice(from).join("\n");
}
