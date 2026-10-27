//! The vault link index: one revisioned snapshot of every Markdown note, its headings/blocks and every link
//! occurrence with its resolution. `resolve_target` is the single resolution authority for index records and for
//! the viewer's `resolve` query alike.
//!
//! The index is built by streaming each note (no whole-file load, no note-size cap). Links are discovered outside
//! fenced code, inline code spans and front matter. Nothing here writes to the vault.
use super::dto::*;
use super::fsio::{self, err, is_markdown_name, now_ms, percent_decode, validate_relative, HashedReader};
use super::markdown::{looks_external, slug, split_fragment, Event, Heading, RawLink, Scanner};
use super::vault::VaultRoot;
use super::{KnowledgeService, Result};
use crate::code_quality::snapshot::is_link;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// A single line above this size is reported instead of scanned (never silently truncated).
const MAX_LINE: usize = 8 * 1024 * 1024;
const MAX_DEPTH: usize = 64;
const MAX_LIMIT: usize = 200;
pub(crate) const MAX_PAGE_BYTES: usize = 1024 * 1024;
const MAX_WARNINGS: usize = 50;
/// Display bounds never change the exact heading/title text kept by the index.
const MAX_DISPLAY_BYTES: usize = 8 * 1024;
const MAX_TARGET_BYTES: usize = 1024;
const MAX_SEARCH_BYTES: usize = 1024;
/// A note modified this soon before the previous build may still be changing; it is re-read.
const RACY_NANOS: u128 = 2_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NoteState {
    Indexed,
    /// A line above [`MAX_LINE`] was skipped; links on that line are unknown (reported as a warning).
    LineTooLong,
}

#[derive(Debug)]
pub(crate) struct Note {
    pub path: String,
    pub title: String,
    pub bytes: u64,
    pub hash: String,
    pub mtime: u128,
    pub state: NoteState,
    pub headings: Vec<Heading>,
    pub blocks: HashSet<String>,
    pub links: Vec<RawLink>,
}

#[derive(Debug)]
pub(crate) struct Link {
    pub id: String,
    pub source: usize,
    pub target: Option<usize>,
    pub kind: VaultLinkKind,
    pub status: VaultLinkStatus,
    pub raw: String,
    pub label: Option<String>,
    pub fragment: Option<String>,
    pub line: u64,
    pub candidates: Vec<usize>,
    pub anchor_missing: bool,
}

pub(crate) struct VaultIndex {
    pub revision: u64,
    pub hash: String,
    pub built_ms: u64,
    pub notes: Vec<Arc<Note>>,
    by_path: HashMap<String, usize>,
    by_suffix: HashMap<String, Vec<usize>>,
    by_filename_lower: HashMap<String, Vec<usize>>,
    pub links: Vec<Link>,
    outgoing: Vec<Vec<usize>>,
    incoming: Vec<Vec<usize>>,
    pub unreadable: u64,
    pub warnings: Vec<String>,
}

#[derive(Default)]
pub(crate) struct Slot {
    build: Mutex<()>,
    current: Mutex<Option<Arc<VaultIndex>>>,
    revision: AtomicU64,
}

struct Candidate {
    path: String,
    components: Vec<String>,
    bytes: u64,
    mtime: u128,
}

#[derive(Default)]
struct Walked {
    candidates: Vec<Candidate>,
    skipped_links: u64,
    skipped_unsafe: u64,
    too_deep: u64,
}

struct Built {
    notes: Vec<Arc<Note>>,
    unreadable: u64,
    warnings: Vec<String>,
    hash: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Resolved {
    pub status: VaultLinkStatus,
    pub target: Option<usize>,
    pub candidates: Vec<usize>,
}

fn walk(root: &Path, components: &mut Vec<String>, out: &mut Walked) -> Result<()> {
    let dir = components.iter().fold(root.to_path_buf(), |path, c| path.join(c));
    let _directory_pin = fsio::pin_directory(&dir)?;
    let entries = fs::read_dir(&dir).map_err(|e| err("Cannot list the vault", e))?;
    for entry in entries {
        let entry = entry.map_err(|e| err("Cannot list the vault", e))?;
        walk_entry(root, components, &entry, out)?;
    }
    Ok(())
}

fn walk_entry(root: &Path, components: &mut Vec<String>, entry: &fs::DirEntry, out: &mut Walked) -> Result<()> {
    let Ok(name) = entry.file_name().into_string() else {
        out.skipped_unsafe += 1;
        return Ok(());
    };
    if name.starts_with('.') {
        return Ok(());
    }
    let Ok(meta) = fs::symlink_metadata(entry.path()) else {
        out.skipped_unsafe += 1;
        return Ok(());
    };
    if is_link(&meta) {
        out.skipped_links += 1;
        return Ok(());
    }
    components.push(name.clone());
    let relative = components.join("/");
    if validate_relative(&relative).is_err() {
        out.skipped_unsafe += 1;
    } else if meta.is_dir() {
        if components.len() >= MAX_DEPTH {
            out.too_deep += 1;
        } else {
            walk(root, components, out)?;
        }
    } else if meta.is_file() && is_markdown_name(&name) {
        out.candidates.push(Candidate { path: relative, components: components.clone(), bytes: meta.len(), mtime: fsio::mtime_nanos(&meta) });
    }
    components.pop();
    Ok(())
}

fn scan_note(root: &Path, candidate: &Candidate) -> std::result::Result<Note, String> {
    let (file, _) = fsio::open_components(root, &candidate.components)?;
    let length = file.metadata().map_err(|e| err("Cannot inspect note", e))?.len();
    let mut reader = HashedReader::new(file);
    let mut scanner = Scanner::default();
    let mut events = Vec::new();
    let mut line = Vec::new();
    let mut first = true;
    let mut long_lines = 0u64;
    while let Some(read) = reader.next_line(&mut line, MAX_LINE).map_err(|e| err("Cannot read note", e))? {
        let at_start = first;
        first = false;
        if read.too_long {
            long_lines += 1;
            continue;
        }
        match std::str::from_utf8(fsio::trim_line(&line, at_start)) {
            Ok(text) => scanner.feed(text, &mut events),
            Err(_) => return Err("not valid UTF-8".into()),
        }
    }
    scanner.finish(&mut events);
    let (digest, file) = reader.finish().map_err(|e| err("Cannot read note", e))?;
    if !digest.utf8_ok {
        return Err("not valid UTF-8".into());
    }
    let after = file.metadata().map_err(|e| err("Cannot inspect note", e))?.len();
    if digest.bytes != length || after != length {
        return Err("changed while it was being indexed".into());
    }
    let mut note = Note {
        path: candidate.path.clone(),
        title: String::new(),
        bytes: length,
        hash: digest.hash,
        mtime: candidate.mtime,
        state: if long_lines > 0 { NoteState::LineTooLong } else { NoteState::Indexed },
        headings: Vec::new(),
        blocks: HashSet::new(),
        links: Vec::new(),
    };
    for event in events {
        match event {
            Event::Heading(heading) => note.headings.push(heading),
            Event::Link(link) => note.links.push(link),
            Event::Block(id) => {
                note.blocks.insert(id);
            }
        }
    }
    note.title = note.headings.iter().find(|h| h.level == 1 && !h.text.is_empty()).map(|h| h.text.clone()).unwrap_or_else(|| {
        let name = candidate.path.rsplit('/').next().unwrap_or(&candidate.path);
        name[..name.len() - 3].to_string()
    });
    Ok(note)
}

fn scan_vault(root: &Path, previous: Option<&VaultIndex>) -> Result<Built> {
    let mut walked = Walked::default();
    walk(root, &mut Vec::new(), &mut walked)?;
    walked.candidates.sort_by(|a, b| a.path.cmp(&b.path));
    let mut notes = Vec::with_capacity(walked.candidates.len());
    let mut warnings = Vec::new();
    let warn = |text: String, warnings: &mut Vec<String>| {
        if warnings.len() < MAX_WARNINGS {
            warnings.push(text);
        }
    };
    let mut unreadable = 0u64;
    let mut hasher = Sha256::new();
    for candidate in &walked.candidates {
        let reusable = previous.and_then(|p| p.by_path.get(&candidate.path).map(|i| (p, p.notes[*i].clone()))).filter(|(p, note)| {
            candidate.mtime > 0 && note.bytes == candidate.bytes && note.mtime == candidate.mtime && u128::from(p.built_ms) * 1_000_000 >= candidate.mtime + RACY_NANOS
        });
        let note = match reusable {
            Some((_, note)) => Ok(note),
            None => scan_note(root, candidate).map(Arc::new),
        };
        match note {
            Ok(note) => {
                if note.state == NoteState::LineTooLong {
                    warn(format!("{}: a line is longer than {MAX_LINE} bytes; links on it are not indexed", note.path), &mut warnings);
                }
                hasher.update(format!("{}\0{}\0{}\n", note.path, note.bytes, note.hash).as_bytes());
                notes.push(note);
            }
            Err(reason) => {
                unreadable += 1;
                hasher.update(format!("{}\0!\n", candidate.path).as_bytes());
                warn(format!("{}: {reason}", candidate.path), &mut warnings);
            }
        }
    }
    if walked.skipped_links > 0 {
        warn(format!("{} linked/reparse-point entries were skipped", walked.skipped_links), &mut warnings);
    }
    if walked.skipped_unsafe > 0 {
        warn(format!("{} entries with unsafe or non-Unicode names were skipped", walked.skipped_unsafe), &mut warnings);
    }
    if walked.too_deep > 0 {
        warn(format!("{} folders deeper than {MAX_DEPTH} levels were skipped", walked.too_deep), &mut warnings);
    }
    Ok(Built { notes, unreadable, warnings, hash: fsio::hex(hasher.finalize()) })
}

fn normalize_join(base: &str, relative: &str) -> Option<String> {
    let mut parts: Vec<&str> = if relative.starts_with('/') || base.is_empty() { Vec::new() } else { base.split('/').collect() };
    for segment in relative.trim_start_matches('/').split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn found_note(index: usize) -> Resolved {
    Resolved { status: VaultLinkStatus::Resolved, target: Some(index), candidates: Vec::new() }
}

fn unique_or_ambiguous(list: &[usize]) -> Resolved {
    match list {
        [only] => found_note(*only),
        _ => Resolved { status: VaultLinkStatus::Ambiguous, target: None, candidates: list.to_vec() },
    }
}

fn has_other_extension(name: &str) -> bool {
    let last = name.rsplit('/').next().unwrap_or(name);
    match last.rsplit_once('.') {
        Some((stem, ext)) => !stem.is_empty() && !ext.is_empty() && ext.len() <= 10 && ext.chars().all(|c| c.is_ascii_alphanumeric()) && !ext.eq_ignore_ascii_case("md"),
        None => false,
    }
}

/// A UTF-8 prefix for display only; identities and source text are never shortened in the index.
fn display_prefix(text: &str, maximum: usize) -> &str {
    let mut end = text.len().min(maximum);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
impl VaultIndex {
    fn assemble(revision: u64, built: Built) -> Self {
        let mut by_path = HashMap::new();
        let mut by_suffix: HashMap<String, Vec<usize>> = HashMap::new();
        let mut by_filename_lower: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, note) in built.notes.iter().enumerate() {
            by_path.insert(note.path.clone(), i);
            let mut rest = note.path.as_str();
            loop {
                by_suffix.entry(rest.to_string()).or_default().push(i);
                match rest.split_once('/') {
                    Some((_, tail)) => rest = tail,
                    None => break,
                }
            }
            let name = note.path.rsplit('/').next().unwrap_or(&note.path);
            by_filename_lower.entry(name.to_lowercase()).or_default().push(i);
        }
        let mut index = Self {
            revision,
            hash: built.hash,
            built_ms: now_ms(),
            outgoing: vec![Vec::new(); built.notes.len()],
            incoming: vec![Vec::new(); built.notes.len()],
            notes: built.notes,
            by_path,
            by_suffix,
            by_filename_lower,
            links: Vec::new(),
            unreadable: built.unreadable,
            warnings: built.warnings,
        };
        let mut links = Vec::new();
        for (source, note) in index.notes.iter().enumerate() {
            for (ordinal, raw) in note.links.iter().enumerate() {
                links.push(index.link_record(source, ordinal, raw));
            }
        }
        for (i, link) in links.iter().enumerate() {
            index.outgoing[link.source].push(i);
            if let (VaultLinkStatus::Resolved, Some(target)) = (link.status, link.target) {
                index.incoming[target].push(i);
            }
        }
        index.links = links;
        index
    }

    fn anchor_exists(&self, note: usize, fragment: &str) -> bool {
        let note = &self.notes[note];
        if let Some(block) = fragment.strip_prefix('^') {
            return note.blocks.contains(block);
        }
        let wanted = fragment.rsplit('#').find(|s| !s.trim().is_empty()).unwrap_or(fragment).trim();
        let wanted_slug = slug(wanted);
        note.headings.iter().any(|h| h.text == wanted || h.text.eq_ignore_ascii_case(wanted) || h.slug == wanted_slug)
    }

    fn link_record(&self, source: usize, ordinal: usize, raw: &RawLink) -> Link {
        let note = &self.notes[source];
        let mut link = Link {
            id: format!("{}#{ordinal}", note.path),
            source,
            target: None,
            kind: raw.kind,
            status: VaultLinkStatus::Resolved,
            raw: raw.target.clone(),
            label: raw.label.clone(),
            fragment: raw.fragment.clone(),
            line: raw.line,
            candidates: Vec::new(),
            anchor_missing: false,
        };
        if raw.external {
            link.status = VaultLinkStatus::External;
        } else if raw.anchor_only {
            link.status = VaultLinkStatus::Anchor;
            link.raw = format!("#{}", raw.fragment.as_deref().unwrap_or(""));
            link.anchor_missing = raw.fragment.as_deref().is_some_and(|f| !self.anchor_exists(source, f));
        } else {
            let resolved = self.resolve_target(&note.path, &raw.target);
            link.status = resolved.status;
            link.target = resolved.target;
            link.candidates = resolved.candidates;
            if let (Some(target), Some(fragment)) = (resolved.target, raw.fragment.as_deref()) {
                link.anchor_missing = !self.anchor_exists(target, fragment);
            }
        }
        link
    }

    /// Resolves a link target as written (without `#fragment`) found in the note `from`. Order: exact path relative to
    /// the note's folder, exact path from the vault root, unique path suffix, unique case-insensitive file name (names
    /// without a folder only). The `.md` extension is optional. Several matches are `Ambiguous`; none are `Missing`, or
    /// `NonDocument` when the target names another kind of file.
    pub(crate) fn resolve_target(&self, from: &str, target: &str) -> Resolved {
        let decoded = percent_decode(target).unwrap_or_else(|| target.to_string()).replace('\\', "/");
        let decoded = decoded.trim();
        if !decoded.is_empty() {
            if let Some(resolved) = self.lookup_named(from, decoded) {
                return resolved;
            }
        }
        let status = if has_other_extension(decoded) { VaultLinkStatus::NonDocument } else { VaultLinkStatus::Missing };
        Resolved { status, target: None, candidates: Vec::new() }
    }

    fn lookup_named(&self, from: &str, decoded: &str) -> Option<Resolved> {
        let named = if is_markdown_name(decoded) { decoded.to_string() } else { format!("{decoded}.md") };
        let directory = from.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        let by_path = |joined: Option<String>| joined.and_then(|p| self.by_path.get(&p).copied()).map(found_note);
        if let Some(resolved) = by_path(normalize_join(directory, &named)).or_else(|| by_path(normalize_join("", &named))) {
            return Some(resolved);
        }
        let suffix = named.trim_start_matches("./").trim_start_matches('/');
        if let Some(list) = self.by_suffix.get(suffix) {
            return Some(unique_or_ambiguous(list));
        }
        if suffix.contains('/') {
            return None;
        }
        self.by_filename_lower.get(&suffix.to_lowercase()).map(|list| unique_or_ambiguous(list))
    }

    fn note_index(&self, path: &str) -> Result<usize> {
        self.by_path.get(path).copied().ok_or_else(|| format!("Not a note in this vault: {}", fsio::shown(path)))
    }

    fn link_dto(&self, i: usize) -> VaultLinkRecord {
        let link = &self.links[i];
        VaultLinkRecord {
            id: link.id.clone(),
            source: self.notes[link.source].path.clone(),
            target: link.target.map(|t| self.notes[t].path.clone()),
            kind: link.kind,
            status: link.status,
            raw: display_prefix(&link.raw, MAX_TARGET_BYTES).to_string(),
            label: link.label.as_deref().map(|s| display_prefix(s, MAX_TARGET_BYTES).to_string()),
            fragment: link.fragment.as_deref().map(|s| display_prefix(s, MAX_TARGET_BYTES).to_string()),
            line: link.line,
            candidates: link.candidates.iter().take(MAX_LIMIT).map(|c| self.notes[*c].path.clone()).collect(),
            anchor_missing: link.anchor_missing,
            raw_truncated: link.raw.len() > MAX_TARGET_BYTES,
            label_truncated: link.label.as_ref().is_some_and(|s| s.len() > MAX_TARGET_BYTES),
            fragment_truncated: link.fragment.as_ref().is_some_and(|s| s.len() > MAX_TARGET_BYTES),
            candidate_total: link.candidates.len() as u64,
            candidates_truncated: link.candidates.len() > MAX_LIMIT,
        }
    }

    fn counts(&self, note: usize) -> (u64, u64) {
        let outgoing = self.outgoing[note].iter().filter(|l| self.links[**l].status == VaultLinkStatus::Resolved && self.links[**l].target != Some(note)).count();
        let incoming = self.incoming[note].iter().filter(|l| self.links[**l].source != note).count();
        (outgoing as u64, incoming as u64)
    }

    fn note_dto(&self, i: usize) -> VaultNoteRecord {
        let note = &self.notes[i];
        let (outgoing, incoming) = self.counts(i);
        VaultNoteRecord {
            path: note.path.clone(),
            title: display_prefix(&note.title, MAX_DISPLAY_BYTES).to_string(),
            title_truncated: note.title.len() > MAX_DISPLAY_BYTES,
            bytes: note.bytes,
            hash: note.hash.clone(),
            outgoing,
            incoming,
        }
    }

    pub(crate) fn note_hash(&self, path: &str) -> Option<(&str, u64)> {
        self.by_path.get(path).map(|i| (self.notes[*i].hash.as_str(), self.notes[*i].bytes))
    }

    pub(crate) fn link_total(&self) -> u64 {
        self.links.len() as u64
    }
}

fn window(offset: usize, limit: usize) -> Result<()> {
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(format!("Query limit must be between 1 and {MAX_LIMIT}"));
    }
    if offset > 100_000_000 {
        return Err("Query offset is out of range".into());
    }
    Ok(())
}

/// Count JSON bytes without allocating a second transport-sized buffer.
#[derive(Default)]
struct JsonSize(usize);

impl Write for JsonSize {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn serialized_size(value: &impl serde::Serialize) -> Result<usize> {
    let mut count = JsonSize::default();
    serde_json::to_writer(&mut count, value).map_err(|e| e.to_string())?;
    Ok(count.0)
}

fn paged<T: serde::Serialize>(vault_id: &str, revision: u64, total: usize, offset: usize, limit: usize, make: impl Fn(usize) -> T) -> Result<VaultQueryPage<T>> {
    window(offset, limit)?;
    let start = offset.min(total);
    let end = (start + limit).min(total);
    let mut page = VaultQueryPage { vault_id: vault_id.to_string(), revision, total: total as u64, offset: offset as u64, items: Vec::new(), next_offset: None };
    let envelope_bytes = serialized_size(&page)? - serialized_size(&Option::<u64>::None)?;
    page.next_offset = (start < total).then_some(start as u64);
    let mut item_bytes = 0;
    for i in start..end {
        let item = make(i);
        let additional = serialized_size(&item)? + usize::from(!page.items.is_empty());
        // Start with the empty-page envelope, substituting the exact next offset at this prefix length.
        // The untagged VaultQueryAnswer adds no outer bytes.
        let next_offset = (i + 1 < total).then_some((i + 1) as u64);
        if envelope_bytes + serialized_size(&next_offset)? + item_bytes + additional > MAX_PAGE_BYTES {
            if page.items.is_empty() {
                return Err("One query row exceeds the 1 MiB transport limit".into());
            }
            break;
        }
        item_bytes += additional;
        page.items.push(item);
        page.next_offset = next_offset;
    }
    if page.items.is_empty() && serialized_size(&page)? > MAX_PAGE_BYTES {
        return Err("Query envelope exceeds the 1 MiB transport limit".into());
    }
    Ok(page)
}

impl VaultIndex {
    pub(crate) fn answer(&self, vault_id: &str, query: &VaultLinkQuery) -> Result<VaultQueryAnswer> {
        let revision = self.revision;
        match query {
            VaultLinkQuery::Notes { offset, limit } => Ok(VaultQueryAnswer::Notes(paged(vault_id, revision, self.notes.len(), *offset, *limit, |i| self.note_dto(i))?)),
            VaultLinkQuery::Links { offset, limit } => Ok(VaultQueryAnswer::Links(paged(vault_id, revision, self.links.len(), *offset, *limit, |i| self.link_dto(i))?)),
            VaultLinkQuery::Backlinks { path, offset, limit } => {
                let note = self.note_index(path)?;
                let rows: Vec<usize> = self.incoming[note].iter().copied().filter(|l| self.links[*l].source != note).collect();
                Ok(VaultQueryAnswer::Links(paged(vault_id, revision, rows.len(), *offset, *limit, |i| self.link_dto(rows[i]))?))
            }
            VaultLinkQuery::Neighbors { path, offset, limit } => {
                let note = self.note_index(path)?;
                let mut rows: Vec<usize> = self.outgoing[note].iter().chain(self.incoming[note].iter()).copied().collect();
                rows.sort_unstable();
                rows.dedup();
                Ok(VaultQueryAnswer::Links(paged(vault_id, revision, rows.len(), *offset, *limit, |i| self.link_dto(rows[i]))?))
            }
            VaultLinkQuery::Search { text, offset, limit } => {
                window(*offset, *limit)?;
                let hits = self.search(text)?;
                Ok(VaultQueryAnswer::Search(paged(vault_id, revision, hits.len(), *offset, *limit, |i| hits[i].clone())?))
            }
            VaultLinkQuery::Resolve { from_path, targets } => {
                if targets.len() > MAX_LIMIT {
                    return Err(format!("At most {MAX_LIMIT} targets can be resolved at once"));
                }
                if !from_path.is_empty() {
                    validate_relative(from_path)?;
                }
                for target in targets {
                    if target.len() > MAX_TARGET_BYTES {
                        return Err(format!("Each resolve target must be at most {MAX_TARGET_BYTES} UTF-8 bytes"));
                    }
                }
                #[derive(serde::Serialize)]
                #[serde(rename_all = "camelCase")]
                struct ResolveTransport<'a> {
                    kind: &'static str,
                    from_path: &'a str,
                    targets: &'a [String],
                }
                if serialized_size(&ResolveTransport { kind: "resolve", from_path, targets })? > MAX_PAGE_BYTES {
                    return Err("Resolve query exceeds the 1 MiB transport limit".into());
                }
                if targets.is_empty() {
                    return Ok(VaultQueryAnswer::Resolve(VaultQueryPage {
                        vault_id: vault_id.to_string(), revision, total: 0, offset: 0, items: Vec::new(), next_offset: None,
                    }));
                }
                Ok(VaultQueryAnswer::Resolve(paged(vault_id, revision, targets.len(), 0, targets.len(), |i| self.resolve_written(from_path, &targets[i]))?))
            }
        }
    }

    fn resolve_written(&self, from: &str, raw: &str) -> VaultResolution {
        let (target, _fragment) = split_fragment(raw);
        let outcome = if looks_external(&target) {
            Resolved { status: VaultLinkStatus::External, target: None, candidates: Vec::new() }
        } else if target.is_empty() {
            Resolved { status: VaultLinkStatus::Anchor, target: None, candidates: Vec::new() }
        } else {
            self.resolve_target(from, &target)
        };
        VaultResolution {
            raw: raw.to_string(),
            status: outcome.status,
            path: outcome.target.map(|t| self.notes[t].path.clone()),
            candidates: outcome.candidates.iter().take(MAX_LIMIT).map(|c| self.notes[*c].path.clone()).collect(),
            candidate_total: outcome.candidates.len() as u64,
            candidates_truncated: outcome.candidates.len() > MAX_LIMIT,
        }
    }

    /// Path/title/heading search; every note (isolated ones included) is reachable.
    fn search(&self, text: &str) -> Result<Vec<VaultSearchHit>> {
        if text.len() > MAX_SEARCH_BYTES {
            return Err(format!("Search text must be at most {MAX_SEARCH_BYTES} UTF-8 bytes"));
        }
        let needle = text.trim();
        if needle.is_empty() {
            return Err("Search text is required".into());
        }
        let lowered = needle.to_lowercase();
        let mut hits: Vec<(u8, usize, VaultSearchHit)> = Vec::new();
        for (i, note) in self.notes.iter().enumerate() {
            let Some((rank, line, snippet)) = best_hit(note, needle, &lowered) else { continue };
            hits.push((rank, i, VaultSearchHit {
                path: note.path.clone(),
                title: display_prefix(&note.title, MAX_DISPLAY_BYTES).to_string(),
                title_truncated: note.title.len() > MAX_DISPLAY_BYTES,
                line,
                snippet: display_prefix(snippet, MAX_DISPLAY_BYTES).to_string(),
                snippet_truncated: snippet.len() > MAX_DISPLAY_BYTES,
            }));
        }
        hits.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
        Ok(hits.into_iter().map(|(_, _, hit)| hit).collect())
    }
}

/// Ranks a path/title match: exact name (0), exact substring (1), case-folded substring (2).
fn name_match<'a>(note: &'a Note, needle: &str, lowered: &str) -> Option<(u8, &'a str)> {
    let name = note.path.rsplit('/').next().unwrap_or(&note.path);
    let stem = &name[..name.len() - 3];
    let (title_lower, path_lower) = (note.title.to_lowercase(), note.path.to_lowercase());
    if stem == needle || note.title == needle || stem.to_lowercase() == lowered || title_lower == lowered {
        Some((0, &note.title))
    } else if note.path.contains(needle) || note.title.contains(needle) {
        Some((1, if note.title.contains(needle) { &note.title } else { &note.path }))
    } else if path_lower.contains(lowered) || title_lower.contains(lowered) {
        Some((2, if title_lower.contains(lowered) { &note.title } else { &note.path }))
    } else {
        None
    }
}

/// The best match of one note: a name match, otherwise (rank 3) its first matching heading.
fn best_hit<'a>(note: &'a Note, needle: &str, lowered: &str) -> Option<(u8, Option<u64>, &'a str)> {
    if let Some((rank, snippet)) = name_match(note, needle, lowered) {
        return Some((rank, None, snippet));
    }
    note.headings.iter().find(|heading| heading.text.to_lowercase().contains(lowered)).map(|heading| (3, Some(heading.line), heading.text.as_str()))
}

fn refresh_dto(vault_id: &str, index: &VaultIndex, changed: bool) -> VaultRefresh {
    VaultRefresh {
        vault_id: vault_id.to_string(),
        revision: index.revision,
        notes: index.notes.len() as u64,
        links: index.links.len() as u64,
        unreadable: index.unreadable,
        hash: index.hash.clone(),
        changed,
        warnings: index.warnings.clone(),
    }
}

impl KnowledgeService {
    fn slot(&self, vault_id: &str) -> Arc<Slot> {
        self.indexes.lock().entry(vault_id.to_string()).or_default().clone()
    }

    pub(crate) fn drop_index(&self, vault_id: &str) {
        self.indexes.lock().remove(vault_id);
    }

    /// The current index revision, 0 until the vault was indexed.
    pub(crate) fn current_revision(&self, vault_id: &str) -> u64 {
        self.indexes.lock().get(vault_id).and_then(|slot| slot.current.lock().as_ref().map(|i| i.revision)).unwrap_or(0)
    }

    pub(crate) fn cached_index(&self, vault_id: &str) -> Option<Arc<VaultIndex>> {
        self.indexes.lock().get(vault_id).and_then(|slot| slot.current.lock().clone())
    }

    fn build_index(&self, vault: &VaultRoot) -> Result<(Arc<VaultIndex>, bool)> {
        let _authority_and_root_pins = vault.pin()?;
        let slot = self.slot(&vault.vault_id);
        let _building = slot.build.lock();
        let previous = slot.current.lock().clone();
        let built = scan_vault(&vault.root, previous.as_deref())?;
        if let Some(current) = &previous {
            if current.hash == built.hash {
                return Ok((current.clone(), false));
            }
        }
        let revision = slot.revision.fetch_add(1, Ordering::SeqCst) + 1;
        let index = Arc::new(VaultIndex::assemble(revision, built));
        *slot.current.lock() = Some(index.clone());
        Ok((index, true))
    }

    fn index_for(&self, vault: &VaultRoot) -> Result<Arc<VaultIndex>> {
        {
            let _authority_and_root_pins = vault.pin()?;
            if let Some(index) = self.cached_index(&vault.vault_id) {
                return Ok(index);
            }
        }
        self.build_index(vault).map(|(index, _)| index)
    }

    /// Re-reads the vault and publishes a new revision when (and only when) its content changed.
    pub fn refresh_vault_index(&self, vault_id: &str) -> Result<VaultRefresh> {
        let vault = self.vault_root(vault_id)?;
        let (index, changed) = self.build_index(&vault)?;
        let _authority_and_root_pins = vault.pin()?;
        Ok(refresh_dto(&vault.vault_id, &index, changed))
    }

    /// Strict, paged link/graph/search queries over the vault index (built on first use).
    pub fn query_vault_links(&self, vault_id: &str, query: &VaultLinkQuery) -> Result<VaultQueryAnswer> {
        let vault = self.vault_root(vault_id)?;
        let index = self.index_for(&vault)?;
        let _authority_and_root_pins = vault.pin()?;
        index.answer(&vault.vault_id, query)
    }

    /// Hash and size of a note when the cached index still describes it.
    pub(crate) fn indexed_note(&self, vault_id: &str, path: &str, bytes: u64, mtime: u128) -> Option<String> {
        let index = self.cached_index(vault_id)?;
        let i = *index.by_path.get(path)?;
        let note = &index.notes[i];
        (note.bytes == bytes && note.mtime == mtime && mtime > 0 && u128::from(index.built_ms) * 1_000_000 >= mtime + RACY_NANOS).then(|| note.hash.clone())
    }
}

#[cfg(test)]
#[path = "index_regression.rs"]
mod regression;
