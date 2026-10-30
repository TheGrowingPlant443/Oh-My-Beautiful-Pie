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

const escapeHtml = (text: string): string => text.replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll(">", "&gt;").replaceAll('"', "&quot;");

export function slugify(text: string): string {
  return text.normalize("NFKC").toLowerCase().trim().replace(/[^\p{L}\p{N}\s-]/gu, "").replace(/\s+/g, "-");
}
/** Last non-empty `#` segment of an Obsidian nested reference, or the whole text when there is none. */
function lastHeading(fragment: string): string {
  const parts = fragment.split("#");
  for (let i = parts.length - 1; i >= 0; i--) if (parts[i]) return parts[i];
  return fragment;
}
/** Last heading level of an Obsidian nested reference (`Note#Section#Sub`). */
export const fragmentSlug = (fragment: string): string => slugify(lastHeading(fragment));

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
  const path = safeDecode(hash < 0 ? value : value.slice(0, hash)).replaceAll("\\", "/");
  const fragment = hash < 0 ? null : safeDecode(value.slice(hash + 1));
  return path === "" ? { kind: "anchor", fragment: fragment ?? "" } : { kind: "relative", path, fragment };
}

// ---------- markdown-it plugin ----------

interface WikiMeta { embed: boolean; target: string; fragment: string | null; label: string | null; raw: string }

function wikilinkRule(state: StateInline, silent: boolean): boolean {
  const src = state.src;
  let pos = state.pos;
  const embed = src.codePointAt(pos) === 0x21;
  if (embed) pos++;
  if (src.codePointAt(pos) !== 0x5b || src.codePointAt(pos + 1) !== 0x5b) return false;
  const start = pos + 2;
  const end = src.indexOf("]]", start);
  if (end < 0 || end + 2 > state.posMax) return false;
  const inner = src.slice(start, end);
  if (!inner.trim() || inner.includes("\n") || inner.includes("[") || inner.includes("]")) return false;
  let bar = -1;
  for (let i = 0; i < inner.length; i++) if (inner[i] === "|" && inner[i - 1] !== "\\") { bar = i; break; }
  const targetPart = (bar < 0 ? inner : inner.slice(0, bar)).replaceAll(String.raw`\|`, "|");
  const label = bar < 0 ? null : inner.slice(bar + 1).replaceAll(String.raw`\|`, "|").trim() || null;
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

/** Obsidian block identifier: a trailing ` ^id` names the paragraph and is not shown. */
function applyBlockId(token: Token, inline: Token | undefined): void {
  const last = inline?.children?.at(-1);
  const found = last?.type === "text" ? /(?:^|\s)\^([A-Za-z0-9-]+)$/.exec(last.content) : null;
  if (!inline || !last || !found) return;
  token.attrSet("data-block", found[1]);
  last.content = last.content.slice(0, last.content.length - found[0].length);
  if (inline.content.endsWith(found[0])) inline.content = inline.content.slice(0, inline.content.length - found[0].length);
}

/** Task list item: a leading `[ ]`/`[x]` in the first paragraph of a list item. */
function applyTaskBox(state: StateCore, open: Token | undefined, inline: Token | undefined): void {
  const children = inline?.children;
  const first = children?.[0];
  if (open?.type !== "list_item_open" || !children || first?.type !== "text") return;
  const task = /^\[([ xX])\]\s+/.exec(first.content);
  if (!task) return;
  first.content = first.content.slice(task[0].length);
  const box = new state.Token("task_box", "", 0);
  box.meta = { checked: task[1] !== " " };
  children.unshift(box);
  open.attrJoin("class", "task-list-item");
}

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
      applyBlockId(token, tokens[i + 1]);
      applyTaskBox(state, tokens[i - 1], tokens[i + 1]);
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
  const fragmentAttribute = fragment ? ` data-fragment="${escapeHtml(fragment)}"` : "";
  return `<a href="#" class="vault-link note" data-kind="note" data-path="${escapeHtml(path)}"${fragmentAttribute}>${labelHtml}</a>`;
}

function wikiLabel(meta: WikiMeta): string {
  if (meta.label !== null) return meta.label;
  if (!meta.target) return meta.fragment ?? "";
  if (!meta.fragment) return meta.target;
  return `${meta.target} › ${meta.fragment.split("#").at(-1)}`;
}

function embedHtml(env: RenderEnv, path: string, meta: WikiMeta, shown: string): string {
  const embed = env.embeds.get(`${path}#${meta.fragment ?? ""}`);
  const head = `<div class="vault-embed-head">${noteAnchor(path, meta.fragment, escapeHtml(shown))}</div>`;
  const body = embed ? `<div class="vault-embed-body">${embed.html}</div>` : "";
  const notice = embed?.notice ? `<p class="vault-embed-notice">${escapeHtml(embed.notice)}</p>` : "";
  return `<div class="vault-embed" data-embed-path="${escapeHtml(path)}">${head}${body}${notice}</div>`;
}

/** Opening tag for a Markdown link: every anchor points at "#"; the target lives in data-* for the host's click handler. */
function openLink(target: HrefClass, env: RenderEnv): { html: string; tag: "a" | "span" } {
  switch (target.kind) {
    case "anchor":
      return { tag: "a", html: `<a href="#" class="vault-link anchor" data-kind="anchor" data-fragment="${escapeHtml(target.fragment)}">` };
    case "external":
      if (target.https) return { tag: "a", html: `<a href="#" class="vault-link external" data-kind="external" data-url="${escapeHtml(target.url)}" title="${escapeHtml(target.url)}">` };
      return { tag: "span", html: `<span class="vault-link inert" title="Only https links can be opened">` };
    case "scheme":
      return { tag: "span", html: `<span class="vault-link inert" title="This kind of link is never opened">` };
    default: {
      const resolution = resolutionOf(env, target.path);
      if (resolution?.status === "resolved" && resolution.path) {
        const fragment = target.fragment ? ` data-fragment="${escapeHtml(target.fragment)}"` : "";
        return { tag: "a", html: `<a href="#" class="vault-link note" data-kind="note" data-path="${escapeHtml(resolution.path)}"${fragment}>` };
      }
      const { cls, title } = describe(resolution);
      return { tag: "span", html: `<span class="vault-link ${cls}" title="${escapeHtml(title)}">` };
    }
  }
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
    const shown = wikiLabel(meta);
    if (!meta.target) return `<a href="#" class="vault-link anchor" data-kind="anchor" data-fragment="${escapeHtml(meta.fragment ?? "")}">${escapeHtml(shown)}</a>`;
    const resolution = resolutionOf(env, meta.target);
    if (resolution?.status === "resolved" && resolution.path) {
      return meta.embed ? embedHtml(env, resolution.path, meta, shown) : noteAnchor(resolution.path, meta.fragment, escapeHtml(shown));
    }
    const { cls, title } = describe(resolution);
    return inertSpan(cls, title, shown);
  };

  rules.link_open = (tokens, idx, _options, rawEnv) => {
    const { html, tag } = openLink(classifyHref(String(tokens[idx].attrGet("href") ?? "")), rawEnv as unknown as RenderEnv);
    tokens[idx].meta = { ...tokens[idx].meta, tag };
    return html;
  };
  rules.link_close = (tokens, idx) => {
    // Pair with the nearest unclosed link_open.
    let depth = 0;
    for (let i = idx - 1; i >= 0; i--) {
      const type = tokens[i].type;
      if (type === "link_close") depth++;
      else if (type === "link_open") {
        if (depth === 0) return tokens[i].meta?.tag === "span" ? "</span>" : "</a>";
        depth--;
      }
    }
    return "</a>";
  };

  rules.image = (tokens, idx) => {
    const alt = inlineText(tokens[idx]);
    const label = alt ? `: ${escapeHtml(alt)}` : "";
    return `<span class="vault-link inert vault-image" title="Images are not loaded">[image${label}]</span>`;
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

function collectToken(token: Token, targets: Set<string>, embeds: CollectedRefs["embeds"]): void {
  if (token.type === "wikilink") {
    const meta = token.meta as unknown as WikiMeta;
    if (!meta.target) return;
    targets.add(meta.target);
    if (meta.embed) embeds.push({ raw: meta.target, fragment: meta.fragment });
  } else if (token.type === "link_open") {
    const target = classifyHref(String(token.attrGet("href") ?? ""));
    if (target.kind === "relative") targets.add(target.path);
  }
}

/** Link targets a document needs resolved (deduplicated) and the embeds it contains. */
export function collectRefs(tokens: readonly Token[]): CollectedRefs {
  const targets = new Set<string>();
  const embeds: { raw: string; fragment: string | null }[] = [];
  const visit = (list: readonly Token[]): void => {
    for (const token of list) {
      collectToken(token, targets, embeds);
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
      env.resolutions.set(raw, answer?.raw === raw ? answer : { raw, status: "missing", path: null, candidates: [] });
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

/** Parses an ATX heading line; the closing `#` run and surrounding blanks are not part of the text. */
function parseHeading(line: string): { level: number; text: string } | null {
  let start = 0;
  while (start < 3 && line[start] === " ") start++;
  let level = 0;
  while (line[start + level] === "#") level++;
  const after = line[start + level];
  if (level < 1 || level > 6 || (after !== " " && after !== "\t")) return null;
  let from = start + level;
  while (line[from] === " " || line[from] === "\t") from++;
  let end = line.length;
  const trimEnd = (chars: string): void => { while (end > from && chars.includes(line[end - 1])) end--; };
  trimEnd(" \t");
  trimEnd("#");
  trimEnd(" \t");
  return { level, text: line.slice(from, end) };
}

/** Fence state after `line`: opened by a fence line, closed by a fence line of the same kind, otherwise unchanged. */
function nextFence(line: string, fence: string | null): string | null {
  const opened = FENCE.exec(line);
  if (!opened) return fence;
  if (fence === null) return opened[1][0];
  return fence === opened[1][0] ? null : fence;
}

/** The paragraph or list block that ends in ` ^id`, without the identifier. */
function extractBlock(lines: string[], id: string): string | null {
  const escaped = id.replaceAll(/[.*+?^${}()|[\]\\]/g, String.raw`\$&`);
  const ending = new RegExp(String.raw`(?:^|\s)\^${escaped}\s*$`);
  let fence: string | null = null;
  let start = 0;
  for (let i = 0; i <= lines.length; i++) {
    const line = lines[i] ?? "";
    fence = nextFence(line, fence);
    if (fence !== null || line.trim() !== "") continue;
    const last = lines[i - 1];
    if (i > start && ending.test(last)) return [...lines.slice(start, i - 1), last.replace(ending, "")].join("\n");
    start = i + 1;
  }
  return null;
}

function extractHeading(lines: string[], name: string): string | null {
  const wanted = slugify(name);
  let fence: string | null = null;
  let level = 0;
  let from = -1;
  for (let i = 0; i < lines.length; i++) {
    const inside = fence !== null;
    fence = nextFence(lines[i], fence);
    if (inside || fence !== null) continue;
    const heading = parseHeading(lines[i]);
    if (!heading) continue;
    if (from < 0) {
      if (slugify(heading.text) === wanted) { from = i; level = heading.level; }
    } else if (heading.level <= level) return lines.slice(from, i).join("\n");
  }
  return from < 0 ? null : lines.slice(from).join("\n");
}

/** Section of a note named by `#Heading` (up to the next heading of the same or a higher level) or `#^block`. Null when not found. */
export function extractFragment(text: string, fragment: string): string | null {
  const lines = text.replace(/\r\n?/g, "\n").split("\n");
  const name = lastHeading(fragment);
  return name.startsWith("^") ? extractBlock(lines, name.slice(1)) : extractHeading(lines, name);
}
