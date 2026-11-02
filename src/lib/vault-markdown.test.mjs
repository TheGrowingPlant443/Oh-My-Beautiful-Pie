import test from "node:test";
import assert from "node:assert/strict";
import { importTs } from "./test-loader.mjs";

const lib = await importTs("vault-markdown");
const { createMarkdown, renderSegment, newEnv, classifyHref, collectRefs, splitStable, splitFrontmatter, frontmatterPending, extractFragment, slugify, fragmentSlug } = lib;

const md = createMarkdown();
const resolved = path => ({ status: "resolved", path, candidates: [] });
/** Index double: resolves by exact name from a table and records what was asked. */
function services(table, extra = {}) {
  const asked = [];
  return {
    asked,
    resolve: async (_from, targets) => targets.map(raw => { asked.push(raw); return { raw, ...(table[raw] ?? { status: "missing", path: null, candidates: [] }) }; }),
    loadEmbed: extra.loadEmbed ?? (async path => ({ html: `<p>embedded ${path}</p>`, notice: null })),
    cancelled: () => false,
  };
}
const render = (source, table = {}, extra = {}) => renderSegment(md, source, newEnv("Folder/Current.md"), services(table, extra));

test("raw HTML, scripts and event handlers are escaped, never emitted as markup", async () => {
  const html = await render('<script>alert(1)</script>\n\n<img src=x onerror=alert(1)>\n\n<a href="javascript:alert(1)">x</a> and [click](javascript:alert(1))');
  assert.ok(!/<script/i.test(html));
  assert.ok(!/<img/i.test(html));
  assert.ok(!/onerror=[^&]*>/.test(html.replace(/&lt;[^]*?&gt;/g, "")));
  assert.ok(!html.includes('href="javascript'));
});

test("every anchor points at # and keeps its destination in data attributes; remote images are inert and request nothing", async () => {
  const html = await render("[site](https://example.com/a) ![pic](https://example.com/p.png) [plain](http://example.com) [odd](obsidian://open?vault=x) [here](#intro)");
  assert.match(html, /data-kind="external" data-url="https:\/\/example.com\/a"/);
  assert.ok(!/<img/.test(html));
  assert.match(html, /vault-image/);
  assert.match(html, /Only https links can be opened/);
  assert.match(html, /This kind of link is never opened/);
  for (const match of html.matchAll(/<a [^>]*href="([^"]*)"/g)) assert.equal(match[1], "#");
});

test("wiki links, markdown links, anchors and aliases resolve through the index and show missing/ambiguous/non-document explicitly", async () => {
  const table = {
    Note: resolved("Notes/Note.md"), "other.md": resolved("Other/other.md"), Same: { status: "ambiguous", path: null, candidates: ["A/Same.md", "B/Same.md"] },
    "scripts/run.py": { status: "nonDocument", path: null, candidates: [] },
  };
  const source = "[[Note]] [[Note|alias text]] [[Note#Heading One]] [[Note#^abc]] [[Gone]] [[Same]] [rel](other.md#Part) [script](scripts/run.py) [[#local]]";
  const html = await render(source, table);
  assert.match(html, /<a href="#" class="vault-link note" data-kind="note" data-path="Notes\/Note.md">Note<\/a>/);
  assert.match(html, />alias text<\/a>/);
  assert.match(html, /data-path="Notes\/Note.md" data-fragment="Heading One">Note › Heading One</);
  assert.match(html, /data-fragment="\^abc"/);
  assert.match(html, /<span class="vault-link missing" title="No note with that name in this vault">Gone<\/span>/);
  assert.match(html, /vault-link ambiguous" title="Several notes match: A\/Same.md, B\/Same.md"/);
  assert.match(html, /data-path="Other\/other.md" data-fragment="Part"/);
  assert.match(html, /<span class="vault-link inert" title="Not a Markdown vault document">script<\/span>/);
  assert.match(html, /data-kind="anchor" data-fragment="local"/);
});

test("links inside code spans and fences are neither resolved nor rendered as links", async () => {
  const source = "`[[Hidden]]` here\n\n```\n[[Fenced]]\n```\n\n    [[Indented]]\n\nreal [[Shown]]";
  const spy = services({ Shown: resolved("Shown.md") });
  const html = await renderSegment(md, source, newEnv("a.md"), spy);
  assert.deepEqual(spy.asked, ["Shown"]);
  assert.ok(!/data-path="Hidden/.test(html));
  assert.match(html, /\[\[Fenced\]\]/);
});

test("collectRefs lists link targets once and embeds separately", () => {
  const tokens = md.parse("[[A]] [[A#x]] ![[B]] ![[C#^id]] [m](d/e.md) [x](https://e.com) [[#same]]", newEnv("n.md"));
  const refs = collectRefs(tokens);
  assert.deepEqual(refs.targets.sort(), ["A", "B", "C", "d/e.md"]);
  assert.deepEqual(refs.embeds, [{ raw: "B", fragment: null }, { raw: "C", fragment: "^id" }]);
});

test("transclusions render through the host and keep their notice; unresolved embeds are not expanded", async () => {
  const loaded = [];
  const html = await render("![[Part#Section]]\n\n![[Nowhere]]", { Part: resolved("Part.md") }, { loadEmbed: async (path, fragment) => { loaded.push([path, fragment]); return { html: "<p>section text</p>", notice: "Only the first 256 KiB of Part.md is shown here; open the note to read the rest." }; } });
  assert.deepEqual(loaded, [["Part.md", "Section"]]);
  assert.match(html, /vault-embed-body"><p>section text<\/p>/);
  assert.match(html, /vault-embed-notice">Only the first 256 KiB/);
  assert.match(html, /vault-link missing[^>]*>Nowhere/);
});

test("headings get stable unique anchors, task items and block ids are recognised, executable fences are labelled", async () => {
  const source = "# Intro\n\n## Intro\n\n- [ ] open\n- [x] done\n\nA paragraph ^blk1\n\n```mermaid\ngraph TD\n```\n";
  const html = await render(source);
  assert.match(html, /<h1 data-anchor="intro">/);
  assert.match(html, /<h2 data-anchor="intro-1">/);
  assert.match(html, /task-list-item/);
  assert.match(html, /aria-checked="false">☐/);
  assert.match(html, /aria-checked="true">☑/);
  assert.match(html, /<p data-block="blk1">A paragraph<\/p>/);
  assert.match(html, /mermaid · shown as text, never run/);
  assert.equal(slugify("Héllo  World!"), "héllo-world");
  assert.equal(fragmentSlug("Parent#Child Heading"), "child-heading");
});

test("href classification keeps only relative paths as note candidates and decodes them", () => {
  assert.deepEqual(classifyHref("My%20Note.md#Part"), { kind: "relative", path: "My Note.md", fragment: "Part" });
  assert.deepEqual(classifyHref("#top"), { kind: "anchor", fragment: "top" });
  assert.deepEqual(classifyHref("https://a.b/c"), { kind: "external", url: "https://a.b/c", https: true });
  assert.equal(classifyHref("javascript:alert(1)").kind, "scheme");
  assert.equal(classifyHref("//evil.example/x").kind, "scheme");
  assert.equal(classifyHref("file:///c:/secret").kind, "scheme");
});

test("a long note splits only at top-level block boundaries: an open fence, list or table stays in the held-back tail", () => {
  const page1 = "# Title\n\nPara one.\n\n```js\nconst a = 1;\n";
  const first = splitStable(md, page1, false);
  assert.equal(first.stable, "# Title\n\nPara one.\n\n");
  assert.ok(first.tail.startsWith("```js"));
  // The next page closes the fence: the whole fence is now a block of its own and the following paragraph is the new tail.
  const page2 = `${first.tail}const b = 2;\n\`\`\`\n\nAfter.`;
  const second = splitStable(md, page2, false);
  assert.ok(second.stable.includes("const b = 2;"));
  assert.equal(second.tail, "After.");
  assert.deepEqual(splitStable(md, "only one block", false), { stable: "", tail: "only one block" });
  assert.deepEqual(splitStable(md, "only one block", true), { stable: "only one block", tail: "" });
  // Every byte survives: stable + tail reproduce the input.
  const text = "a\n\n- x\n- y\n\n| h |\n|---|\n| c |";
  const cut = splitStable(md, text, false);
  assert.equal(cut.stable + cut.tail, text);
});

test("front matter is separated only when closed", () => {
  assert.equal(frontmatterPending("---\ntitle: x\n"), true);
  assert.equal(splitFrontmatter("---\ntitle: x\n"), null);
  assert.deepEqual(splitFrontmatter("---\ntitle: x\ntags: [a]\n---\nBody"), { properties: "title: x\ntags: [a]", body: "Body" });
  assert.equal(frontmatterPending("Body\n---\n"), false);
});

test("embed sections: by heading up to the next same-or-higher heading, by block id, ignoring fenced pseudo headings", () => {
  const text = "# Top\nintro\n## Part A\na1\n```\n## not a heading\n```\n### Deep\nd1\n## Part B\nb1\n\nA block ^id1\n\nOther ^id2\n";
  assert.equal(extractFragment(text, "Part A"), "## Part A\na1\n```\n## not a heading\n```\n### Deep\nd1");
  assert.equal(extractFragment(text, "Top#Part B"), "## Part B\nb1\n\nA block ^id1\n\nOther ^id2\n");
  assert.equal(extractFragment(text, "^id1"), "A block");
  assert.equal(extractFragment(text, "Missing"), null);
  assert.equal(extractFragment(text, "^nope"), null);
});

test("embed sections accept closed and tab-separated ATX headings and block ids after a fenced block", () => {
  const text = "  ##\tPart X ##  \nx\n####### not a heading\n## Next\n\n```\ncode\n\nstill code ^fenced\n```\ntail ^tail\n";
  assert.equal(extractFragment(text, "Part X"), "  ##\tPart X ##  \nx\n####### not a heading");
  assert.equal(extractFragment(text, "^tail"), "```\ncode\n\nstill code ^fenced\n```\ntail");
  assert.equal(extractFragment(text, "^fenced"), null);
});

test("a cancelled render stops without output", async () => {
  let calls = 0;
  const html = await renderSegment(md, "[[A]]", newEnv("a.md"), { resolve: async () => { calls++; return [{ raw: "A", status: "resolved", path: "A.md", candidates: [] }]; }, loadEmbed: async () => ({ html: "", notice: null }), cancelled: () => true });
  assert.equal(html, null);
  assert.equal(calls, 1);
});
