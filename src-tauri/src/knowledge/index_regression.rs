//! Transport and binding regressions. Fixtures are synthetic or isolated temporary vaults; no external stores.
use super::*;
use super::super::integrations::Environment;
use std::path::PathBuf;
use std::sync::mpsc;

struct Temp(PathBuf);

impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("omp-index-regression-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        Self(crate::setup::canonical(&path).unwrap())
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn note(path: String, title: String) -> Note {
    Note {
        path, title, bytes: 0, hash: "fixture-hash".into(), mtime: 0, state: NoteState::Indexed,
        headings: Vec::new(), blocks: HashSet::new(), links: Vec::new(),
    }
}

fn index(notes: Vec<Note>) -> VaultIndex {
    VaultIndex::assemble(7, Built {
        notes: notes.into_iter().map(Arc::new).collect(), unreadable: 0, warnings: Vec::new(), hash: "fixture-index".into(),
    })
}

fn answer_page(answer: &VaultQueryAnswer) -> serde_json::Value {
    let bytes = serde_json::to_vec(answer).unwrap();
    assert!(bytes.len() <= MAX_PAGE_BYTES, "entire response must fit, not just its rows");
    serde_json::from_slice(&bytes).unwrap()
}

#[test]
fn longest_page_prefix_includes_json_escaping_and_envelope() {
    let rows: Vec<String> = (0..200).map(|i| format!("{i}:{}", "\u{1f}\"\\🦀".repeat(1600))).collect();
    let vault_id = "quoted\"vault\\🦀";
    let page = paged(vault_id, u64::MAX, rows.len(), 0, 200, |i| rows[i].clone()).unwrap();
    let count = page.items.len();
    assert!(count > 0 && count < 200);
    assert_eq!(page.total, 200);
    assert_eq!(page.offset, 0);
    assert_eq!(page.next_offset, Some(count as u64));
    assert!(serde_json::to_vec(&page).unwrap().len() <= MAX_PAGE_BYTES);
    let mut extended = page.clone();
    extended.items.push(rows[count].clone());
    extended.next_offset = Some((count + 1) as u64);
    assert!(serde_json::to_vec(&extended).unwrap().len() > MAX_PAGE_BYTES);
    let mut offset = count;
    while offset < rows.len() {
        let next = paged(vault_id, u64::MAX, rows.len(), offset, 200, |i| rows[i].clone()).unwrap();
        assert_eq!(next.total, 200);
        assert_eq!(next.offset, offset as u64);
        assert_eq!(next.items, rows[offset..offset + next.items.len()]);
        offset += next.items.len();
        assert_eq!(next.next_offset, (offset < rows.len()).then_some(offset as u64));
        assert!(serde_json::to_vec(&next).unwrap().len() <= MAX_PAGE_BYTES);
    }
    assert_eq!(offset, 200);
    let past_end = paged(vault_id, 7, 200, 250, 200, |_| String::new()).unwrap();
    assert_eq!(past_end.offset, 250);
    assert!(past_end.items.is_empty());
    assert_eq!(past_end.next_offset, None);
}

#[test]
fn two_hundred_note_query_succeeds_with_a_smaller_prefix() {
    let indexed = index((0..200).map(|i| note(format!("{i:03}.md"), "\u{1f}".repeat(MAX_DISPLAY_BYTES))).collect());
    let answer = indexed.answer("vault", &VaultLinkQuery::Notes { offset: 0, limit: 200 }).unwrap();
    let value = answer_page(&answer);
    let count = value["items"].as_array().unwrap().len();
    assert!(count > 0 && count < 200);
    assert_eq!(value["total"], 200);
    assert_eq!(value["nextOffset"], count);
}

#[test]
fn near_eight_mib_heading_is_exactly_indexed_but_bounded_for_display() {
    let temp = Temp::new();
    let heading = format!("{} rare-needle", "🦀".repeat((MAX_LINE - 64) / 4));
    let text = format!("# {heading}\n{}", "ordinary body line\n".repeat(100));
    assert!(text.len() > MAX_LINE, "there is no total document-size cap");
    fs::write(temp.0.join("huge.md"), &text).unwrap();
    let built = scan_vault(&temp.0, None).unwrap();
    assert_eq!(built.unreadable, 0);
    let indexed = VaultIndex::assemble(1, built);
    assert_eq!(indexed.notes[0].title, heading);
    assert_eq!(indexed.notes[0].headings[0].text, heading);
    assert!(indexed.anchor_exists(0, &heading));
    let notes = indexed.answer("vault", &VaultLinkQuery::Notes { offset: 0, limit: 200 }).unwrap();
    let value = answer_page(&notes);
    let title = value["items"][0]["title"].as_str().unwrap();
    assert!(title.len() <= MAX_DISPLAY_BYTES);
    assert!(heading.starts_with(title));
    assert_eq!(value["items"][0]["titleTruncated"], true);
    let search = indexed.answer("vault", &VaultLinkQuery::Search { text: "rare-needle".into(), offset: 0, limit: 200 }).unwrap();
    let value = answer_page(&search);
    assert_eq!(value["total"], 1, "matching uses full heading, not its display prefix");
    assert_eq!(value["items"][0]["path"], "huge.md");
    assert_eq!(value["items"][0]["titleTruncated"], true);
    assert_eq!(value["items"][0]["snippetTruncated"], true);
    assert!(value["items"][0]["snippet"].as_str().unwrap().len() <= MAX_DISPLAY_BYTES);
}

fn ambiguous_index(candidate_count: usize, occurrences: usize) -> VaultIndex {
    let mut notes: Vec<Note> = (0..candidate_count).map(|i| {
        let folder = "x".repeat(230);
        note(format!("{i:03}/{folder}/{folder}/{folder}/{folder}/Shared.md"), "Shared".into())
    }).collect();
    let mut source = note("source.md".into(), "Source".into());
    source.links = (0..occurrences).map(|i| RawLink {
        kind: VaultLinkKind::Wiki, line: i as u64 + 1, target: "Shared".into(), fragment: None,
        label: None, external: false, anchor_only: false,
    }).collect();
    notes.push(source);
    index(notes)
}

#[test]
fn large_ambiguous_links_are_paged_without_shortening_path_identities() {
    let indexed = ambiguous_index(201, 200);
    let answer = indexed.answer("vault", &VaultLinkQuery::Links { offset: 0, limit: 200 }).unwrap();
    let value = answer_page(&answer);
    let items = value["items"].as_array().unwrap();
    assert!(!items.is_empty() && items.len() < 200);
    assert_eq!(value["total"], 200);
    assert_eq!(value["nextOffset"], items.len());
    assert_eq!(items[0]["candidateTotal"], 201);
    assert_eq!(items[0]["candidatesTruncated"], true);
    assert_eq!(items[0]["candidates"].as_array().unwrap().len(), 200);
    assert_eq!(indexed.links[0].candidates.len(), 201, "the index retains every identity");
    for (candidate, path) in items[0]["candidates"].as_array().unwrap().iter().zip(&indexed.notes) {
        assert_eq!(candidate.as_str().unwrap(), path.path);
    }
    let next = indexed.answer("vault", &VaultLinkQuery::Links { offset: items.len(), limit: 200 }).unwrap();
    let next = answer_page(&next);
    assert_eq!(next["offset"], items.len());
    assert_ne!(items[0]["id"], next["items"][0]["id"]);
}

#[test]
fn long_link_fields_keep_originals_and_explicit_unicode_safe_display_metadata() {
    let target = format!("Missing{}suffix", "🦀".repeat(300));
    let fragment = "🦀".repeat(300);
    let label = "ø".repeat(600);
    let events = super::super::markdown::scan_text(&format!("[[{target}#{fragment}|{label}]]"));
    let mut source = note("source.md".into(), "Source".into());
    source.links = events.into_iter().filter_map(|event| match event { Event::Link(link) => Some(link), _ => None }).collect();
    assert_eq!(source.links.len(), 1);
    assert_eq!(source.links[0].target, target);
    assert_eq!(source.links[0].fragment.as_deref(), Some(fragment.as_str()));
    assert_eq!(source.links[0].label.as_deref(), Some(label.as_str()));
    let indexed = index(vec![source]);
    assert_eq!(indexed.links[0].raw, target);
    let answer = indexed.answer("vault", &VaultLinkQuery::Links { offset: 0, limit: 200 }).unwrap();
    let value = answer_page(&answer);
    let row = &value["items"][0];
    for (field, original) in [("raw", &target), ("fragment", &fragment), ("label", &label)] {
        let display = row[field].as_str().unwrap();
        assert!(display.len() <= MAX_TARGET_BYTES);
        assert!(original.starts_with(display));
        assert_eq!(row[format!("{field}Truncated")], true);
    }
    assert_eq!(row["id"], "source.md#0");
}

#[test]
fn resolve_validates_each_field_and_the_escaped_request_transport() {
    let indexed = index(Vec::new());
    for targets in [vec!["x".repeat(MAX_TARGET_BYTES + 1)], vec!["x".into(); 201], vec!["\u{1f}".repeat(MAX_TARGET_BYTES); 200]] {
        assert!(indexed.answer("vault", &VaultLinkQuery::Resolve { from_path: String::new(), targets }).is_err());
    }
    assert!(indexed.answer("vault", &VaultLinkQuery::Resolve { from_path: "x".repeat(1025), targets: Vec::new() }).is_err());
    assert!(indexed.answer("vault", &VaultLinkQuery::Search { text: "x".repeat(MAX_SEARCH_BYTES + 1), offset: 0, limit: 200 }).is_err());
    let empty = indexed.answer("vault", &VaultLinkQuery::Resolve { from_path: String::new(), targets: Vec::new() }).unwrap();
    let value = answer_page(&empty);
    assert_eq!(value["total"], 0);
    assert_eq!(value["nextOffset"], serde_json::Value::Null);
}

#[test]
fn resolve_response_uses_transport_bounded_prefix_and_exact_total() {
    let indexed = ambiguous_index(201, 0);
    let answer = indexed.answer("vault", &VaultLinkQuery::Resolve { from_path: "source.md".into(), targets: vec!["Shared".into(); 200] }).unwrap();
    let value = answer_page(&answer);
    let count = value["items"].as_array().unwrap().len();
    assert!(count > 0 && count < 200);
    assert_eq!(value["total"], 200);
    assert_eq!(value["nextOffset"], count);
    assert_eq!(value["items"][0]["candidateTotal"], 201);
    assert_eq!(value["items"][0]["candidatesTruncated"], true);
}

fn bind(service: &KnowledgeService, root: &Path) -> VaultBinding {
    let prepared = service.prepare_knowledge_vault(ScopeRequest::Global, None, &root.to_string_lossy(), VaultMode::Markdown).unwrap();
    assert!(prepared.conflicts.is_empty());
    service.confirm_knowledge_vault(&prepared.token).unwrap()
}

#[test]
fn delayed_lazy_build_cannot_publish_an_old_root_after_rebind() {
    let temp = Temp::new();
    fs::create_dir_all(temp.0.join("home")).unwrap();
    let service = KnowledgeService::with_environment(temp.0.join("config"), temp.0.join("home"), Environment::isolated());
    let old_root = temp.0.join("old");
    let new_root = temp.0.join("new");
    fs::create_dir_all(&old_root).unwrap();
    fs::create_dir_all(&new_root).unwrap();
    fs::write(old_root.join("old.md"), "# Old").unwrap();
    fs::write(new_root.join("new.md"), "# New").unwrap();
    let binding = bind(&service, &old_root);
    let (captured_tx, captured_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let worker = service.clone();
    let id = binding.vault_id.clone();
    let delayed = std::thread::spawn(move || {
        let snapshot = worker.vault_root(&id).unwrap();
        captured_tx.send(()).unwrap();
        resume_rx.recv().unwrap();
        worker.build_index(&snapshot).is_err()
    });
    captured_rx.recv().unwrap();
    let rebound = bind(&service, &new_root);
    assert_eq!(rebound.vault_id, binding.vault_id);
    resume_tx.send(()).unwrap();
    assert!(delayed.join().unwrap(), "the stale captured binding must be refused before publication");
    assert!(service.cached_index(&binding.vault_id).is_none());
    let answer = service.query_vault_links(&binding.vault_id, &VaultLinkQuery::Notes { offset: 0, limit: 200 }).unwrap();
    let value = answer_page(&answer);
    assert_eq!(value["total"], 1);
    assert_eq!(value["items"][0]["path"], "new.md");
}
