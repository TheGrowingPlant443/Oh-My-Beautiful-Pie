import DOMPurify from "dompurify";

// Second line of defence behind markdown-it (raw HTML is already off): the rendered note may contain only these elements and
// attributes, and an anchor can carry no destination except "#" (the real target is in data-* for the host's click handler).
// "#text" must be listed: DOMPurify treats text nodes as a tag name, so an allow-list without it deletes every note's text.
const TAGS = [
  "#text", "h1", "h2", "h3", "h4", "h5", "h6", "p", "br", "hr", "ul", "ol", "li", "blockquote", "pre", "code", "em", "strong", "del", "s", "a", "span", "div", "figure", "figcaption",
  "table", "thead", "tbody", "tr", "th", "td", "details", "summary", "sup", "sub",
];
// Everything except href is plain text, not a destination: DOMPurify would otherwise apply the "#"-only URI rule to it too.
const PLAIN_ATTRIBUTES = [
  "class", "title", "role", "style", "start", "aria-checked", "aria-disabled",
  "data-kind", "data-path", "data-fragment", "data-url", "data-anchor", "data-block", "data-embed-path",
];

// Table alignment is the only inline style markdown-it emits; anything else (url(), position, ...) is dropped.
DOMPurify.addHook("uponSanitizeAttribute", (_element, data) => {
  if (data.attrName !== "style") return;
  if (/^\s*text-align:\s*(left|right|center)\s*;?\s*$/.test(data.attrValue)) data.forceKeepAttr = true;
  else data.keepAttr = false;
});

export function sanitizeVaultHtml(html: string): string {
  return DOMPurify.sanitize(html, {
    ALLOWED_TAGS: TAGS,
    ALLOWED_ATTR: ["href", ...PLAIN_ATTRIBUTES],
    ADD_URI_SAFE_ATTR: PLAIN_ATTRIBUTES,
    ALLOW_DATA_ATTR: false,
    ALLOW_ARIA_ATTR: false,
    ALLOWED_URI_REGEXP: /^#$/,
    FORBID_TAGS: ["script", "style", "iframe", "object", "embed", "svg", "math", "img", "form", "input", "link", "meta", "base"],
    KEEP_CONTENT: false,
  });
}
