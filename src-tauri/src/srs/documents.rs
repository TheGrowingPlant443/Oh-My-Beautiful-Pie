//! Pinned templates and the deliberately small, portable public Markdown dialect.
//! Source evidence and its citations belong in the private ledger, never in these notes.

use crate::code_quality::snapshot::{is_link, open_plain, relative_source_name};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

pub const TEMPLATE_REVISION: &str = "3965e951fa5ea1c1dd1bbc7a66b7f35f7e85c006";
pub const DOCUMENT_CHUNK_BYTES: usize = 64 * 1024;
pub const UI_ONLY_LABEL: &str = "UI-only requirement";
pub const IMPLEMENTATION_ONLY_LABEL: &str = "Implementation-only requirement";

pub static EMBEDDED_TEMPLATES: &[(&str, &str)] = &[
    ("srs-template.md", include_str!("../../../resources/srs-template/srs-template.md")),
    ("req-template.md", include_str!("../../../resources/srs-template/req-template.md")),
    ("srs-template-bare.md", include_str!("../../../resources/srs-template/srs-template-bare.md")),
    ("req-template-bare.md", include_str!("../../../resources/srs-template/req-template-bare.md")),
    ("LICENSE", include_str!("../../../resources/srs-template/LICENSE")),
    ("REVISION", include_str!("../../../resources/srs-template/REVISION")),
];

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TemplateFile {
    pub name: String,
    pub sha256: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TemplateManifest {
    pub revision: String,
    pub files: Vec<TemplateFile>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TemplateChunk {
    pub name: String,
    pub offset: usize,
    pub next_offset: Option<usize>,
    pub total_bytes: usize,
    pub sha256: String,
    pub text: String,
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn template_manifest() -> TemplateManifest {
    TemplateManifest {
        revision: TEMPLATE_REVISION.into(),
        files: EMBEDDED_TEMPLATES.iter().map(|(name, text)| TemplateFile {
            name: (*name).into(), sha256: hash(text.as_bytes()),
        }).collect(),
    }
}

pub fn template_chunk(name: &str, offset: usize) -> Result<TemplateChunk, String> {
    let (_, text) = EMBEDDED_TEMPLATES.iter().find(|(candidate, _)| *candidate == name)
        .ok_or_else(|| "Unknown pinned template".to_string())?;
    if offset > text.len() || !text.is_char_boundary(offset) {
        return Err("Template offset must be a UTF-8 byte boundary within the file".into());
    }
    let mut end = offset.saturating_add(DOCUMENT_CHUNK_BYTES).min(text.len());
    while !text.is_char_boundary(end) { end -= 1; }
    Ok(TemplateChunk {
        name: name.into(), offset, next_offset: (end < text.len()).then_some(end),
        total_bytes: text.len(), sha256: hash(text.as_bytes()), text: text[offset..end].into(),
    })
}

fn plain_directory(path: &Path) -> Result<(), String> {
    // Check all existing ancestors, including the caller-owned private job directory.
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(meta) if is_link(&meta) || !meta.is_dir() => return Err("Template directory contains a link or non-directory".into()),
            Ok(_) => {},
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
            Err(error) => return Err(format!("Cannot inspect template directory: {error}")),
        }
    }
    fs::create_dir_all(path).map_err(|error| format!("Cannot create template directory: {error}"))?;
    for ancestor in path.ancestors() {
        let meta = fs::symlink_metadata(ancestor).map_err(|error| format!("Cannot inspect template directory: {error}"))?;
        if is_link(&meta) || !meta.is_dir() { return Err("Unsafe template directory".into()); }
    }
    Ok(())
}

/// Materialize only these six embedded files in `<private job>/template`.
/// Existing files must match the pin; they are never overwritten or silently adopted.
pub fn materialize_templates(job_dir: &Path) -> Result<TemplateManifest, String> {
    let directory = job_dir.join("template");
    plain_directory(&directory)?;
    for (name, text) in EMBEDDED_TEMPLATES {
        let path = directory.join(name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                file.write_all(text.as_bytes()).map_err(|error| format!("Cannot write pinned template: {error}"))?;
                file.sync_all().map_err(|error| format!("Cannot persist pinned template: {error}"))?;
            },
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let mut file = open_plain(&path)?;
                let meta = file.metadata().map_err(|error| format!("Cannot inspect pinned template: {error}"))?;
                if meta.len() != text.len() as u64 { return Err("Pinned template differs from embedded revision".into()); }
                let mut bytes = vec![0; text.len()];
                file.read_exact(&mut bytes).map_err(|error| format!("Cannot read pinned template: {error}"))?;
                if bytes != text.as_bytes() { return Err("Pinned template differs from embedded revision".into()); }
            },
            Err(error) => return Err(format!("Cannot materialize pinned template: {error}")),
        }
    }
    Ok(template_manifest())
}

/// Stable IDs use the backend's persisted ordinal, not a model-supplied name.
pub fn requirement_id(area: &str, ordinal: u64) -> Result<String, String> {
    if ordinal == 0 || area.is_empty() || !area.bytes().all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit()) {
        return Err("Requirement area must be uppercase ASCII letters/digits and ordinal must be positive".into());
    }
    Ok(format!("REQ-{area}-{ordinal:03}"))
}

fn valid_requirement_id(id: &str) -> bool {
    let Some(rest) = id.strip_prefix("REQ-") else { return false; };
    let Some((area, ordinal)) = rest.rsplit_once('-') else { return false; };
    if ordinal.len() < 3 || !ordinal.bytes().all(|byte| byte.is_ascii_digit()) { return false; }
    ordinal.parse::<u64>().ok().and_then(|number| requirement_id(area, number).ok()).as_deref() == Some(id)
}

fn known_type(document_type: &str) -> bool {
    matches!(document_type, "master" | "requirement" | "domain" | "flow" | "interface" | "glossary" |
        "uiObservations" | "structureObservations" | "traceability" | "coverage" | "questions")
}

/// Returns an allocated run-relative path. No model-provided title or path is used.
pub fn document_path(document_type: &str, document_id: &str, requirement: Option<&str>) -> Result<String, String> {
    let id = uuid::Uuid::parse_str(document_id).map_err(|_| "Document ID must be a backend UUID")?;
    if id.to_string() != document_id { return Err("Document ID must be a canonical backend UUID".into()); }
    let path = match document_type {
        "master" => "SRS.md".into(),
        "glossary" => "Glossary.md".into(),
        "uiObservations" => "User interface/Current UI observations.md".into(),
        "structureObservations" => "Implementation context/Current structure observations.md".into(),
        "traceability" => "Verification/Traceability.md".into(),
        "coverage" => "Verification/Coverage.md".into(),
        "questions" => "Verification/Open questions.md".into(),
        "requirement" => {
            let requirement = requirement.filter(|id| valid_requirement_id(id)).ok_or("Missing backend requirement ID")?;
            format!("Requirements/{requirement}.md")
        },
        "domain" => format!("Domains/{document_id}.md"),
        "flow" => format!("Flows/{document_id}.md"),
        "interface" => format!("Interfaces/{document_id}.md"),
        _ => return Err("Unknown public document type".into()),
    };
    if document_type != "requirement" && requirement.is_some() { return Err("Only requirement notes may have a requirement ID".into()); }
    Ok(path)
}

pub struct ValidationContext<'a> {
    /// An allocated run-relative path, or `SRS/<job ID>/<allocated path>`.
    pub path: &'a str,
    pub document_type: &'a str,
    /// `portable`, `uiSpecific`, or `implementationSpecific`.
    pub specificity: &'a str,
    pub requirement_id: Option<&'a str>,
    /// Private inventory paths, identifiers and symbols that must not leak to public notes.
    pub forbidden_terms: &'a [String],
}

#[derive(Debug, Default)]
pub struct ValidationReport {
    pub links: Vec<String>,
    /// The caller MUST validate the private evidence behind every N/A disposition.
    pub not_applicable_sections: Vec<String>,
}

fn run_prefix(path: &str) -> Result<usize, String> {
    let relative = relative_source_name(Path::new(path))?;
    if relative != path || path.contains('\\') || !path.ends_with(".md") { return Err("Invalid public document path".into()); }
    let parts: Vec<_> = path.split('/').collect();
    if parts.first() == Some(&"SRS") {
        if parts.len() < 3 || parts[1].is_empty() || !parts[1].bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_') {
            return Err("Invalid SRS run path".into());
        }
        Ok(2)
    } else { Ok(0) }
}

fn validate_allocated_path(context: &ValidationContext<'_>) -> Result<usize, String> {
    let prefix = run_prefix(context.path)?;
    let local = if prefix == 2 { context.path.splitn(3, '/').nth(2).unwrap_or("") } else { context.path };
    // The UUID is only used for the three dynamically named document types.
    let placeholder_id = "00000000-0000-4000-8000-000000000000";
    let id = if matches!(context.document_type, "domain" | "flow" | "interface") {
        local.rsplit('/').next().and_then(|name| name.strip_suffix(".md")).ok_or("Invalid allocated document path")?
    } else { placeholder_id };
    if document_path(context.document_type, id, context.requirement_id)? != local {
        return Err("Public document path does not match its allocated type and ID".into());
    }
    Ok(prefix)
}

fn link_target(target: &str, current: &str, prefix: usize) -> Result<String, String> {
    // Spaces in allocated names use Markdown's backslash-free angle-free %20 form.
    // Decode ONLY %20; all other escapes are rejected rather than interpreted twice.
    let decoded = target.replace("%20", " ");
    if decoded.is_empty() || decoded.contains(['%', ':', '\\', '?', '<', '>', '"', '\'']) ||
        decoded.starts_with('/') || decoded.chars().any(char::is_control) {
        return Err("Only admitted local Markdown document links are permitted".into());
    }
    let (path, fragment) = decoded.split_once('#').map_or((decoded.as_str(), None), |(path, fragment)| (path, Some(fragment)));
    if let Some(fragment) = fragment {
        if fragment.is_empty() || !fragment.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.')) ||
            fragment.strip_prefix('L').is_some_and(|tail| tail.starts_with(|c: char| c.is_ascii_digit())) {
            return Err("Invalid public heading anchor or source line citation".into());
        }
    }
    if path.is_empty() { return Ok(current.into()); }
    if !path.ends_with(".md") { return Err("Public links must target admitted Markdown notes".into()); }
    let mut parts: Vec<&str> = current.split('/').collect();
    parts.pop();
    for part in path.split('/') {
        match part {
            "" | "." => return Err("Non-canonical public document link".into()),
            ".." => {
                if parts.len() <= prefix { return Err("Document link escapes this SRS run".into()); }
                parts.pop();
            },
            _ => { relative_source_name(Path::new(part))?; parts.push(part); },
        }
    }
    Ok(parts.join("/"))
}

fn has_word(text: &str, word: &str) -> bool {
    text.match_indices(word).any(|(start, _)| {
        let end = start + word.len();
        let boundary = |c: char| !c.is_alphanumeric() && c != '_';
        text[..start].chars().next_back().is_none_or(boundary) && text[end..].chars().next().is_none_or(boundary)
    })
}

fn has_line_reference(lower: &str) -> bool {
    let mut words = lower.split_whitespace().peekable();
    while let Some(word) = words.next() {
        if matches!(word, "line" | "lines") && words.peek().is_some_and(|next| next.starts_with(|c: char| c.is_ascii_digit())) { return true; }
    }
    false
}

/// Bare filenames may be public product contracts; paths and citations are not.
fn is_internal_reference(token: &str) -> bool {
    let token = token.trim_matches(|c: char| matches!(c, '*' | '_' | '.' | '!' | '?'));
    let lower = token.to_ascii_lowercase();
    let without_line = lower.split([':', '#']).next().unwrap_or("");
    lower.starts_with('/') || lower.as_bytes().get(1) == Some(&b':') && lower.as_bytes().get(2) == Some(&b'/') ||
        without_line.contains('/') && without_line.rsplit('/').next().is_some_and(|name| name.contains('.')) ||
        lower.split_once(':').is_some_and(|(_, tail)| tail.starts_with(|c: char| c.is_ascii_digit())) ||
        lower.contains("#l") && lower.split("#l").nth(1).is_some_and(|tail| tail.starts_with(|c: char| c.is_ascii_digit())) ||
        lower.starts_with("src/") || lower.starts_with("src-tauri/") || lower.starts_with("evid-") ||
        token.strip_prefix('L').is_some_and(|tail| !tail.is_empty() && tail.bytes().all(|byte| byte.is_ascii_digit()))
}

fn public_prose(text: &str, forbidden: &[String]) -> Result<(), String> {
    let lower = text.to_ascii_lowercase();
    if ["{{", "}}", "<%", "%>", "to be determined", "to be completed", "insert here", "system shall...", "system shall…"].iter().any(|marker| lower.contains(marker)) ||
        ["todo", "tbd", "fixme", "placeholder"].iter().any(|word| has_word(&lower, word)) {
        return Err("Public note contains an unresolved placeholder".into());
    }
    if ["http:", "https:", "file:", "javascript:", "data:", "vbscript:", "command:", "vscode:", "blob:", "::", "()", "evidenceid", "receiptid", "source excerpt", "source citation", "source://", "citation:", "【", "†"].iter().any(|marker| lower.contains(marker)) {
        return Err("Public note contains a source citation, symbol, or active/remote destination".into());
    }
    if forbidden.iter().any(|term| !term.is_empty() && has_word(text, term)) { return Err("Public note contains private evidence terminology".into()); }
    if has_line_reference(&lower) { return Err("Public note contains an internal source line reference".into()); }
    if text.split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | '[' | ']' | '"' | '\'' | ',' | ';')).any(is_internal_reference) {
        return Err("Public note contains an internal source path or line reference".into());
    }
    Ok(())
}

fn has_forbidden_syntax(line: &str) -> bool {
    line.contains(['`', '<', '\\']) || line.trim_start().starts_with("~~~") ||
        line.contains("![") || line.contains("[[") || line.contains("]]") ||
        (line.starts_with("    ") && !line.trim_start().starts_with("* [") && !line.trim_start().starts_with("- [")) ||
        line.starts_with('\t')
}

/// Entity decoding must not introduce hidden HTML, executable schemes or paths.
fn has_entity(line: &str) -> bool {
    line.match_indices('&').any(|(index, _)| {
        line[index..].find(';').is_some_and(|end| end <= 32 && !line[index..index + end].contains(char::is_whitespace))
    })
}

/// Validates the link opening at `rest[open]` and returns the text after it.
fn validate_link<'a>(
    rest: &'a str, open: usize, context: &ValidationContext<'_>, prefix: usize,
    lookup: &mut impl FnMut(&str) -> bool, report: &mut ValidationReport,
) -> Result<&'a str, String> {
    let label_start = open + 1;
    let close = rest[label_start..].find(']').map(|index| label_start + index).ok_or("Malformed public Markdown link")?;
    let label = &rest[label_start..close];
    if label.is_empty() || label.contains('[') { return Err("Nested or empty public Markdown link".into()); }
    public_prose(label, context.forbidden_terms)?;
    let destination = rest[close + 1..].strip_prefix('(').ok_or("Reference, wiki and shortcut links are not portable public links")?;
    let end = destination.find(')').ok_or("Unterminated public Markdown link")?;
    let target = &destination[..end];
    if target.contains('(') || target.chars().any(char::is_whitespace) { return Err("Ambiguous public Markdown link destination".into()); }
    let resolved = link_target(target, context.path, prefix)?;
    if !lookup(&resolved) { return Err("Public Markdown link does not name an admitted run document".into()); }
    report.links.push(resolved);
    Ok(&destination[end + 1..])
}

fn validate_line(
    line: &str, context: &ValidationContext<'_>, prefix: usize,
    lookup: &mut impl FnMut(&str) -> bool, report: &mut ValidationReport,
) -> Result<(), String> {
    if has_forbidden_syntax(line) { return Err("Public notes cannot contain HTML, code, source excerpts, or embeds".into()); }
    if has_entity(line) { return Err("HTML entities are not part of the public Markdown dialect".into()); }
    let mut rest = line;
    while let Some(open) = rest.find('[') {
        public_prose(&rest[..open], context.forbidden_terms)?;
        rest = validate_link(rest, open, context, prefix, lookup, report)?;
    }
    if rest.contains(']') { return Err("Malformed public Markdown link".into()); }
    public_prose(rest, context.forbidden_terms)
}

fn heading(line: &str) -> Option<(usize, &str)> {
    let trimmed = line.trim_end();
    let level = trimmed.bytes().take_while(|byte| *byte == b'#').count();
    if level == 0 || level > 6 || trimmed.as_bytes().get(level) != Some(&b' ') { return None; }
    Some((level, trimmed[level + 1..].trim_end_matches('#').trim()))
}

/// Titles are derived from the pinned annotated template, not a parallel checklist.
pub fn master_sections() -> Vec<&'static str> {
    EMBEDDED_TEMPLATES[0].1.lines().filter_map(heading).map(|(_, title)| title)
        .filter(|title| title.starts_with(|c: char| c.is_ascii_digit()) || matches!(*title, "Table of Contents" | "Revision History"))
        .collect()
}

fn meaningful_body(body: &str) -> bool {
    body.lines().any(|line| {
        let line = line.trim();
        !line.is_empty() && line.chars().any(char::is_alphanumeric) &&
            !line.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
    })
}

fn collect_sections(text: &str) -> Result<BTreeMap<&str, &str>, String> {
    let mut sections: BTreeMap<&str, &str> = BTreeMap::new();
    let mut current: Option<(&str, usize)> = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if let Some((_, title)) = heading(line) {
            if let Some((previous, start)) = current.take() {
                if sections.insert(previous, &text[start..offset]).is_some() { return Err(format!("Duplicate public section: {previous}")); }
            }
            current = Some((title, offset + line.len()));
        }
        offset += line.len();
    }
    if let Some((title, start)) = current {
        if sections.insert(title, &text[start..]).is_some() { return Err(format!("Duplicate public section: {title}")); }
    }
    Ok(sections)
}

fn check_sections(text: &str, required: &[&str], report: &mut ValidationReport) -> Result<(), String> {
    let sections = collect_sections(text)?;
    for title in required {
        let body = sections.get(title).ok_or_else(|| format!("Required template section is missing: {title}"))?;
        if !meaningful_body(body) { return Err(format!("Template section is not populated: {title}")); }
    }
    for (title, body) in sections {
        let trimmed = body.trim();
        if trimmed.starts_with("N/A") || trimmed.starts_with("Not applicable") {
            trimmed.strip_prefix("N/A:").or_else(|| trimmed.strip_prefix("Not applicable:"))
                .map(str::trim).filter(|reason| reason.chars().filter(|c| c.is_alphanumeric()).count() >= 8)
                .ok_or_else(|| format!("N/A requires an explicit evidence-backed rationale: {title}"))?;
            report.not_applicable_sections.push(title.into());
        }
    }
    Ok(())
}

fn validate_specificity(text: &str, context: &ValidationContext<'_>) -> Result<(), String> {
    let required_label = match context.specificity {
        "portable" => None,
        "uiSpecific" => Some(UI_ONLY_LABEL),
        "implementationSpecific" => Some(IMPLEMENTATION_ONLY_LABEL),
        _ => return Err("Unknown document specificity".into()),
    };
    if context.document_type == "uiObservations" && context.specificity != "uiSpecific" ||
        context.document_type == "structureObservations" && context.specificity != "implementationSpecific" {
        return Err("Observation notes must carry their UI-only or implementation-only specificity".into());
    }
    if let Some(label) = required_label {
        if !text.lines().any(|line| line.trim().trim_matches('*') == label) {
            return Err(format!("Public note must display the mandatory label: {label}"));
        }
    } else if text.contains(UI_ONLY_LABEL) || text.contains(IMPLEMENTATION_ONLY_LABEL) {
        return Err("Portable note cannot claim a UI-only or implementation-only label".into());
    }
    Ok(())
}

fn check_type_sections(text: &str, context: &ValidationContext<'_>, report: &mut ValidationReport) -> Result<(), String> {
    match context.document_type {
        "master" => check_sections(text, &master_sections(), report),
        "requirement" => {
            let id = context.requirement_id.filter(|id| valid_requirement_id(id)).ok_or("Requirement note lacks its backend ID")?;
            if !text.lines().filter_map(heading).any(|(level, title)| level == 1 && title.split_whitespace().next() == Some(id)) {
                return Err("Requirement title must start with its backend-allocated stable REQ ID".into());
            }
            check_sections(text, &["Statement", "Rationale", "Acceptance Criteria", "Verification Method", "More Information"], report)
        },
        _ => check_sections(text, &[], report),
    }
}

/// Validate one complete document without consulting or loading an entire job catalog.
/// `lookup` must consult the admitted on-disk document catalog, including this document.
/// Its argument is normalized in the same path namespace as `context.path`.
/// Citations are deliberately absent from this API: validate/store those privately.
pub fn validate_document(
    text: &str, context: &ValidationContext<'_>, mut lookup: impl FnMut(&str) -> bool,
) -> Result<ValidationReport, String> {
    if !known_type(context.document_type) { return Err("Unknown public document type".into()); }
    let prefix = validate_allocated_path(context)?;
    if text.trim().is_empty() || text.chars().any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')) {
        return Err("Public note is empty or contains control characters".into());
    }
    validate_specificity(text, context)?;
    let mut report = ValidationReport::default();
    for line in text.lines() { validate_line(line, context, prefix, &mut lookup, &mut report)?; }
    if !text.lines().any(|line| heading(line).is_some_and(|(level, _)| level == 1)) {
        return Err("Public note must have a Markdown title".into());
    }
    check_type_sections(text, context, &mut report)?;
    // Avoid repeated catalog edges in the caller's validation result.
    report.links.sort_unstable();
    report.links.dedup();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    const ID: &str = "b2db7e3d-95dc-46fc-bf70-38c5cb3f56a9";

    fn context<'a>(path: &'a str, document_type: &'a str) -> ValidationContext<'a> {
        ValidationContext { path, document_type, specificity: "portable", requirement_id: None, forbidden_terms: &[] }
    }

    fn requirement() -> String {
        "# REQ-CORE-001 Save a note\n\n## Statement\nThe system shall save an accepted note.\n\n## Rationale\nUsers retain their work after closing the application.\n\n## Acceptance Criteria\nA saved note remains available when reopened.\n\n## Verification Method\nTest: save, close, reopen, and compare the note.\n\n## More Information\nRelated behavior is described in the [overview](../SRS.md).\n".into()
    }

    #[test]
    fn portable_requirement_and_catalog_links() {
        let mut context = context("Requirements/REQ-CORE-001.md", "requirement");
        context.requirement_id = Some("REQ-CORE-001");
        let report = validate_document(&requirement(), &context, |path| path == "SRS.md").unwrap();
        assert_eq!(report.links, ["SRS.md"]);
        assert!(report.not_applicable_sections.is_empty());
        assert!(validate_document(&requirement(), &context, |_| false).is_err());
    }

    #[test]
    fn portable_requirements_preserve_public_filenames() {
        let mut context = context("Requirements/REQ-CORE-001.md", "requirement");
        context.requirement_id = Some("REQ-CORE-001");
        for filename in ["orders.json", "settings.yaml", "report.xml"] {
            let text = requirement().replace(
                "The system shall save an accepted note.",
                &format!("The system shall export {filename}."),
            );
            assert!(validate_document(&text, &context, |path| path == "SRS.md").is_ok(), "{filename}");
            let forbidden = vec![filename.to_string()];
            let private_context = ValidationContext {
                path: context.path, document_type: context.document_type,
                specificity: context.specificity, requirement_id: context.requirement_id,
                forbidden_terms: &forbidden,
            };
            assert!(validate_document(&text, &private_context, |path| path == "SRS.md").is_err(), "{filename}");
        }
    }

    #[test]
    fn paths_are_fixed_or_backend_ids() {
        assert_eq!(document_path("master", ID, None).unwrap(), "SRS.md");
        assert_eq!(document_path("uiObservations", ID, None).unwrap(), "User interface/Current UI observations.md");
        assert_eq!(document_path("coverage", ID, None).unwrap(), "Verification/Coverage.md");
        assert_eq!(document_path("domain", ID, None).unwrap(), format!("Domains/{ID}.md"));
        assert!(document_path("domain", "../../outside", None).is_err());
        assert!(document_path("requirement", ID, Some("../../outside")).is_err());
        assert!(document_path("unknown", ID, None).is_err());
        assert!(document_path("master", ID, Some("REQ-CORE-001")).is_err());
    }

    #[test]
    fn ids_have_no_five_hundred_item_ceiling() {
        let mut ids = BTreeSet::new();
        for number in 1..=1501 { assert!(ids.insert(requirement_id("CORE", number).unwrap())); }
        assert_eq!(requirement_id("CORE", 501).unwrap(), "REQ-CORE-501");
        assert_eq!(requirement_id("CORE", 1501).unwrap(), "REQ-CORE-1501");
        assert!(valid_requirement_id("REQ-CORE-1501"));
        assert!(requirement_id("../SRC", 1).is_err());
        assert!(requirement_id("CORE", 0).is_err());
    }

    #[test]
    fn malicious_and_unadmitted_links_are_rejected() {
        let context = context("SRS/job/Glossary.md", "glossary");
        for destination in ["javascript:alert(1)", "https://example.com/a.md", "//example.com/a.md", "file:///x.md", "data:text/html,x", "../../outside.md", "%2e%2e/outside.md", "SRS.md#L12", "SRS.md?x=1", "SRS.md%00", "SRS.md\"onclick=\"x"] {
            let text = format!("# Glossary\n[entry]({destination})\n");
            assert!(validate_document(&text, &context, |_| true).is_err(), "{destination}");
        }
        for text in ["# Glossary\n![remote](https://example.com/a.png)", "# Glossary\n<script>alert(1)</script>", "# Glossary\n&#106;avascript:alert(1)", "# Glossary\n[[../outside]]", "# Glossary\n[entry][ref]\n[ref]: SRS.md", "# Glossary\n```rust\nfn main() {}\n```", "# Glossary\n    source excerpt"] {
            assert!(validate_document(text, &context, |_| true).is_err(), "{text}");
        }
        assert!(validate_document("# Glossary\n[entry](SRS.md)", &context, |_| false).is_err());
        assert!(validate_document("# Glossary\n[entry](SRS.md)", &context, |path| path == "SRS/job/SRS.md").is_ok());
    }

    #[test]
    fn allocated_paths_cannot_escape_or_misrepresent_types() {
        for path in ["../Glossary.md", "/Glossary.md", "SRS/job/../Glossary.md", "Other.md", "SRS/job/Requirements/x.md", "SRS/job/Glossary.md:stream"] {
            assert!(validate_document("# Glossary\nTerms.", &context(path, "glossary"), |_| true).is_err(), "{path}");
        }
    }

    #[test]
    fn template_completeness_and_explicit_na() {
        let sections = master_sections();
        assert!(sections.contains(&"3.6.6 Model Lifecycle and Operations"));
        assert!(sections.contains(&"3.5.10 Change Management"));
        let mut text = "# Software Requirements Specification\n".to_string();
        for section in &sections { text.push_str(&format!("\n## {section}\nN/A: Recorded evidence excludes this capability from the product scope.\n")); }
        let context = context("SRS.md", "master");
        let report = validate_document(&text, &context, |_| true).unwrap();
        assert_eq!(report.not_applicable_sections.len(), sections.len());
        assert!(validate_document(&text.replace("## 3.6.6 Model Lifecycle and Operations", "## Removed"), &context, |_| true).is_err());
        assert!(validate_document(&text.replace("N/A: Recorded evidence excludes this capability from the product scope.", "N/A"), &context, |_| true).is_err());
        assert!(validate_document(EMBEDDED_TEMPLATES[2].1, &context, |_| true).is_err());
    }

    #[test]
    fn private_paths_symbols_and_placeholders_cannot_leak() {
        let forbidden = vec!["saveInternalState".into()];
        let mut context = context("Glossary.md", "glossary");
        context.forbidden_terms = &forbidden;
        for body in ["See src/secret.rs:12", "See src/internal", "See src-tauri/private", "See internal/settings.yaml", "See /private/orders.json", "See C:/private/report.xml", "See orders.json:12", "See settings.yaml#L12", "Call saveInternalState", "Call persist()", "Inspect State::persist", "See L123", "See lines 12-18", "【1†source】", "TODO finish this", "{{project name}}", "The system shall..."] {
            assert!(validate_document(&format!("# Glossary\n{body}"), &context, |_| true).is_err(), "{body}");
        }
    }

    #[test]
    fn specificity_labels_are_mandatory_and_visible() {
        let mut context = context("User interface/Current UI observations.md", "uiObservations");
        context.specificity = "uiSpecific";
        assert!(validate_document("# Current UI\nA dialog exists.", &context, |_| true).is_err());
        assert!(validate_document("# Current UI\nUI-only requirement\nA dialog exists.", &context, |_| true).is_ok());
        context.specificity = "portable";
        assert!(validate_document("# Current UI\nUI-only requirement\nA dialog exists.", &context, |_| true).is_err());
    }

    #[test]
    fn embedded_templates_are_complete_and_chunked() {
        let manifest = template_manifest();
        assert_eq!(manifest.revision, TEMPLATE_REVISION);
        assert_eq!(manifest.files.len(), 6);
        for (name, text) in EMBEDDED_TEMPLATES {
            let chunk = template_chunk(name, 0).unwrap();
            assert!(chunk.text.len() <= DOCUMENT_CHUNK_BYTES);
            assert_eq!(chunk.sha256, hash(text.as_bytes()));
            assert_eq!(chunk.total_bytes, text.len());
            assert!(template_chunk(name, text.len() + 1).is_err());
        }
        assert!(template_chunk("../../outside", 0).is_err());
    }

    #[test]
    fn materialization_preserves_pin_and_refuses_modified_files() {
        let directory = std::env::temp_dir().join(format!("omp-srs-template-{}", uuid::Uuid::new_v4()));
        materialize_templates(&directory).unwrap();
        materialize_templates(&directory).unwrap();
        fs::write(directory.join("template/LICENSE"), b"modified").unwrap();
        assert!(materialize_templates(&directory).is_err());
        fs::remove_dir_all(directory).unwrap();
    }
}
