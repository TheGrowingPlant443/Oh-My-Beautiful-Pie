//! A line-fed Markdown scanner: headings, block ids and link occurrences, with fenced code, inline code spans and
//! YAML front matter excluded. The same state machine tells a document page which fence it starts inside, so the
//! index and the paged reader never disagree about what is code.
use super::dto::{FenceState, VaultLinkKind};
use super::fsio::percent_decode;

const FRONTMATTER_LOOKAHEAD: usize = 1000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Fence {
    pub marker: char,
    pub length: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Heading {
    pub level: u8,
    pub text: String,
    pub slug: String,
    pub line: u64,
}

/// One link occurrence as written, before resolution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RawLink {
    pub kind: VaultLinkKind,
    pub line: u64,
    /// The target as written, without the `#fragment` (percent-escapes are kept).
    pub target: String,
    pub fragment: Option<String>,
    pub label: Option<String>,
    /// The target names a URL scheme (`https:`, `mailto:`, `obsidian:`) or a network path.
    pub external: bool,
    /// `[x](#heading)` / `[[#heading]]`: a link inside the same note.
    pub anchor_only: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Event {
    Heading(Heading),
    Link(RawLink),
    Block(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum Front {
    #[default]
    Start,
    In,
    Done,
}

#[derive(Default)]
pub(crate) struct Scanner {
    line_no: u64,
    front: Front,
    fence: Option<Fence>,
    pending: Vec<(u64, String)>,
}

pub(crate) fn slug(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if c == ' ' || c == '-' {
            out.push('-');
        } else if c == '_' {
            out.push('_');
        }
    }
    out
}

pub(crate) fn looks_external(target: &str) -> bool {
    if target.starts_with("//") {
        return true;
    }
    let Some(colon) = target.find(':') else { return false };
    let scheme = &target[..colon];
    // A single letter is a Windows drive, not a URL scheme.
    scheme.len() >= 2 && scheme.starts_with(|c: char| c.is_ascii_alphabetic()) && scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

fn opening_fence(trimmed: &str) -> Option<Fence> {
    let marker = trimmed.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let length = trimmed.chars().take_while(|c| *c == marker).count();
    if length < 3 {
        return None;
    }
    let rest = &trimmed[length..];
    if marker == '`' && rest.contains('`') {
        return None;
    }
    Some(Fence { marker, length })
}

fn closes_fence(trimmed: &str, fence: &Fence) -> bool {
    let run = trimmed.chars().take_while(|c| *c == fence.marker).count();
    run >= fence.length && trimmed[run..].trim().is_empty()
}

fn block_id(line: &str) -> Option<String> {
    let text = line.trim_end();
    let at = text.rfind('^')?;
    let id = &text[at + 1..];
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return None;
    }
    if at == 0 || text[..at].ends_with(char::is_whitespace) {
        Some(id.to_string())
    } else {
        None
    }
}

pub(crate) fn split_fragment(target: &str) -> (String, Option<String>) {
    match target.find('#') {
        Some(at) => {
            let fragment = target[at + 1..].trim();
            let fragment = percent_decode(fragment).unwrap_or_else(|| fragment.to_string());
            (target[..at].trim().to_string(), (!fragment.is_empty()).then_some(fragment))
        }
        None => (target.trim().to_string(), None),
    }
}

fn link(kind: VaultLinkKind, line: u64, target_text: &str, label: Option<&str>) -> Option<RawLink> {
    let (target, fragment) = split_fragment(target_text);
    if target.is_empty() && fragment.is_none() {
        return None;
    }
    let external = looks_external(&target);
    Some(RawLink {
        kind,
        line,
        anchor_only: target.is_empty(),
        external,
        target,
        fragment,
        label: label.map(str::trim).filter(|l| !l.is_empty()).map(str::to_string),
    })
}

impl Scanner {
    pub(crate) fn fence(&self) -> Option<FenceState> {
        self.fence.as_ref().map(|f| FenceState { marker: f.marker.to_string(), length: f.length as u64 })
    }

    pub(crate) fn in_frontmatter(&self) -> bool {
        self.front == Front::In
    }

    pub(crate) fn lines_seen(&self) -> u64 {
        self.line_no
    }

    /// Feeds one line (no terminator).
    pub(crate) fn feed(&mut self, line: &str, out: &mut Vec<Event>) {
        self.line_no += 1;
        let number = self.line_no;
        match self.front {
            Front::Start => {
                if number == 1 && line.trim_end() == "---" {
                    self.front = Front::In;
                    return;
                }
                self.front = Front::Done;
            }
            Front::In => {
                let trimmed = line.trim_end();
                if trimmed == "---" || trimmed == "..." {
                    self.front = Front::Done;
                    self.pending.clear();
                    return;
                }
                if self.pending.len() < FRONTMATTER_LOOKAHEAD {
                    self.pending.push((number, line.to_string()));
                    return;
                }
                // Never closed within the lookahead: it was a thematic break, not front matter.
                self.front = Front::Done;
                for (n, text) in std::mem::take(&mut self.pending) {
                    self.process(n, &text, out);
                }
            }
            Front::Done => {}
        }
        self.process(number, line, out);
    }

    /// End of file: an unclosed `---` block was a thematic break, so its lines are ordinary Markdown.
    pub(crate) fn finish(&mut self, out: &mut Vec<Event>) {
        if self.front == Front::In {
            self.front = Front::Done;
            for (n, text) in std::mem::take(&mut self.pending) {
                self.process(n, &text, out);
            }
        }
    }

    fn process(&mut self, number: u64, line: &str, out: &mut Vec<Event>) {
        let trimmed = line.trim_start_matches([' ', '\t']);
        if let Some(fence) = &self.fence {
            if closes_fence(trimmed, fence) {
                self.fence = None;
            }
            return;
        }
        if let Some(fence) = opening_fence(trimmed) {
            self.fence = Some(fence);
            return;
        }
        let indent = line.len() - trimmed.len();
        if let Some(heading) = heading(trimmed, indent, number) {
            out.push(Event::Heading(heading));
        }
        if let Some(id) = block_id(line) {
            out.push(Event::Block(id));
        }
        if !reference_definition(trimmed, indent, number, out) {
            scan_inline(line, number, out);
        }
    }
}

fn heading(trimmed: &str, indent: usize, number: u64) -> Option<Heading> {
    if indent > 3 || !trimmed.starts_with('#') {
        return None;
    }
    let hashes = trimmed.bytes().take_while(|b| *b == b'#').count();
    let rest = &trimmed[hashes..];
    if !(1..=6).contains(&hashes) || !(rest.is_empty() || rest.starts_with([' ', '\t'])) {
        return None;
    }
    let mut text = rest.trim();
    let stripped = text.trim_end_matches('#');
    if stripped.len() < text.len() && (stripped.is_empty() || stripped.ends_with([' ', '\t'])) {
        text = stripped.trim_end();
    }
    Some(Heading { level: hashes as u8, text: text.to_string(), slug: slug(text), line: number })
}

/// A `[label]: destination` reference definition; `true` when the line is one (it holds no inline links).
fn reference_definition(trimmed: &str, indent: usize, number: u64, out: &mut Vec<Event>) -> bool {
    if indent > 3 || !trimmed.starts_with('[') || trimmed.starts_with("[[") {
        return false;
    }
    let Some(close) = trimmed.find("]:") else { return false };
    let label = &trimmed[1..close];
    if label.is_empty() || label.starts_with('^') || label.contains('[') {
        return false;
    }
    let rest = trimmed[close + 2..].trim_start();
    let destination = rest.split_whitespace().next().unwrap_or("");
    let destination = destination.strip_prefix('<').and_then(|d| d.strip_suffix('>')).unwrap_or(destination);
    if let Some(found) = link(VaultLinkKind::Markdown, number, destination, Some(label)) {
        out.push(Event::Link(found));
    }
    true
}

/// The index after the inline code span opening at `i`; an unmatched run is skipped alone.
fn skip_code_span(bytes: &[u8], i: usize) -> usize {
    let run = bytes[i..].iter().take_while(|b| **b == b'`').count();
    let mut j = i + run;
    while j < bytes.len() {
        if bytes[j] != b'`' {
            j += 1;
            continue;
        }
        let close = bytes[j..].iter().take_while(|b| **b == b'`').count();
        if close == run {
            return j + close;
        }
        j += close;
    }
    i + run
}

fn scan_inline(line: &str, number: u64, out: &mut Vec<Event>) {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'`' => i = skip_code_span(bytes, i),
            b'!' if bytes.get(i + 1) == Some(&b'[') => i = bracket(line, i + 1, true, number, out).max(i + 1),
            b'[' => i = bracket(line, i, false, number, out).max(i + 1),
            _ => i += 1,
        }
    }
}

fn skip_blanks(bytes: &[u8], mut k: usize) -> usize {
    while k < bytes.len() && (bytes[k] == b' ' || bytes[k] == b'\t') {
        k += 1;
    }
    k
}

/// `[[target|label]]` / `![[target]]` opening at `start`; returns the index to continue scanning from.
fn wiki_link(line: &str, start: usize, embed: bool, number: u64, out: &mut Vec<Event>) -> usize {
    let rest = &line[start + 2..];
    let Some(close) = rest.find("]]") else { return start + 1 };
    let inner = &rest[..close];
    if !inner.contains("[[") {
        let (target, label) = match inner.find('|') {
            Some(bar) => (inner[..bar].trim_end_matches('\\'), Some(&inner[bar + 1..])),
            None => (inner, None),
        };
        let kind = if embed { VaultLinkKind::Embed } else { VaultLinkKind::Wiki };
        if let Some(found) = link(kind, number, target, label) {
            out.push(Event::Link(found));
        }
    }
    start + 2 + close + 2
}

/// The index of the `]` closing the label that opens at `start`, or past the end when there is none.
fn label_end(bytes: &[u8], start: usize) -> usize {
    let mut depth = 1usize;
    let mut j = start + 1;
    while j < bytes.len() {
        match bytes[j] {
            b'\\' => j += 1,
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
        j += 1;
    }
    j
}

/// The link destination starting at `k` (`<angled>` or bare with balanced parentheses) and the index after it.
fn link_destination(line: &str, k: usize) -> Option<(&str, usize)> {
    let bytes = line.as_bytes();
    if bytes.get(k) == Some(&b'<') {
        let close = line[k + 1..].find('>')?;
        return Some((&line[k + 1..k + 1 + close], k + 1 + close + 1));
    }
    let begin = k;
    let mut k = k;
    let mut parens = 0usize;
    while k < bytes.len() {
        match bytes[k] {
            b'\\' => k += 1,
            b'(' => parens += 1,
            b')' if parens == 0 => break,
            b')' => parens -= 1,
            b' ' | b'\t' => break,
            _ => {}
        }
        k += 1;
    }
    let end = k.min(bytes.len());
    Some((&line[begin..end], end))
}

/// Skips an optional quoted or parenthesised title (and trailing blanks) at `k`; `None` when it never closes.
fn skip_title(line: &str, k: usize) -> Option<usize> {
    let bytes = line.as_bytes();
    let Some(&quote) = bytes.get(k).filter(|q| matches!(**q, b'"' | b'\'' | b'(')) else { return Some(k) };
    let closing = if quote == b'(' { b')' } else { quote };
    let end = line[k + 1..].find(closing as char)?;
    Some(skip_blanks(bytes, k + 1 + end + 1))
}

/// Parses a link opening at `start` (a `[`). Returns the index to continue scanning from.
fn bracket(line: &str, start: usize, embed: bool, number: u64, out: &mut Vec<Event>) -> usize {
    let bytes = line.as_bytes();
    if bytes.get(start + 1) == Some(&b'[') {
        return wiki_link(line, start, embed, number, out);
    }
    // Inline link `[label](destination "title")`.
    let j = label_end(bytes, start);
    if j >= bytes.len() || bytes.get(j + 1) != Some(&b'(') {
        return start + 1;
    }
    let label = &line[start + 1..j];
    let Some((destination, k)) = link_destination(line, skip_blanks(bytes, j + 2)) else { return start + 1 };
    let Some(k) = skip_title(line, skip_blanks(bytes, k)) else { return start + 1 };
    if bytes.get(k) != Some(&b')') {
        return start + 1;
    }
    let kind = if embed { VaultLinkKind::Embed } else { VaultLinkKind::Markdown };
    if let Some(found) = link(kind, number, destination, Some(label)) {
        out.push(Event::Link(found));
    }
    k + 1
}

/// Scans a whole text (used by tests and small in-memory callers).
#[cfg(test)]
pub(crate) fn scan_text(text: &str) -> Vec<Event> {
    let mut scanner = Scanner::default();
    let mut out = Vec::new();
    for line in text.lines() {
        scanner.feed(line, &mut out);
    }
    scanner.finish(&mut out);
    out
}
