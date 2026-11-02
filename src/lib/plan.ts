export interface PlanHeadline { line: number; level: number; title: string }

const HEADING = /^(#{1,6})(\s[^\n]*)$/;
const LIST_MARKER = /^\s*([-*+]|\d+[.)])\s+/;
const MAX_TITLE = 100;

/** Heading text: leading blanks and an optional closing `#` run (with surrounding blanks) are dropped; at least one character stays. */
function headingTitle(rest: string): string {
  const body = rest.trimStart();
  if (body === "") return rest.slice(-1);
  let end = body.length;
  while (end > 0 && /\s/.test(body[end - 1])) end--;
  while (end > 0 && body[end - 1] === "#") end--;
  while (end > 0 && /\s/.test(body[end - 1])) end--;
  return body.slice(0, Math.max(end, 1));
}

/** Headlines of a plan; `line` is the 0-based index into `text.split(/\r?\n/)`. */
export function planHeadlines(text: string): PlanHeadline[] {
  if (!text.trim()) return [];
  const lines = text.split(/\r?\n/);
  const headings: PlanHeadline[] = [];
  let inFence = false;
  lines.forEach((raw, line) => {
    if (raw.trimStart().startsWith("```")) { inFence = !inFence; return; }
    if (inFence) return;
    const match = HEADING.exec(raw);
    if (match) headings.push({ line, level: match[1].length, title: headingTitle(match[2]) });
  });
  if (headings.length) return headings;
  // No markdown headings: the first line of every paragraph stands in for one.
  const paragraphs: PlanHeadline[] = [];
  let previousBlank = true;
  lines.forEach((raw, line) => {
    const blank = raw.trim() === "";
    if (!blank && previousBlank) {
      const title = raw.replace(LIST_MARKER, "").trim();
      paragraphs.push({ line, level: 1, title: title.length > MAX_TITLE ? `${title.slice(0, MAX_TITLE)}…` : title });
    }
    previousBlank = blank;
  });
  return paragraphs;
}
