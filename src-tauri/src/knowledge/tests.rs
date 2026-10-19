//! Deterministic regression tests for the knowledge backend. Every fixture lives in a throwaway folder; no
//! application is launched, no network is used and no personal vault/Cognee/Graphify store is read or written.
use super::fsio::{self, sha256_hex, validate_relative};
use super::integrations::{validate_cognee_url, Environment, ExeSetting};
use super::markdown;
use super::*;
use serde_json::{json, Value};
use std::path::PathBuf;

struct Temp(PathBuf);

impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("omp knowledge ø {}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        Self(crate::setup::canonical(&path).unwrap())
    }
    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn service_with(t: &Temp, env: Environment) -> Arc<KnowledgeService> {
    fs::create_dir_all(t.join("home")).unwrap();
    KnowledgeService::with_environment(t.join("config"), t.join("home"), env)
}

fn service(t: &Temp) -> Arc<KnowledgeService> {
    service_with(t, Environment::isolated())
}

fn bind_global(service: &KnowledgeService, root: &Path) -> VaultBinding {
    let preparation = service.prepare_knowledge_vault(ScopeRequest::Global, None, &root.to_string_lossy(), VaultMode::Markdown).unwrap();
    assert!(preparation.conflicts.is_empty(), "{:?}", preparation.conflicts);
    service.confirm_knowledge_vault(&preparation.token).unwrap()
}

fn write_notes<P: AsRef<str>, T: AsRef<[u8]>>(root: &Path, notes: &[(P, T)]) {
    for (path, text) in notes {
        let target = root.join(path.as_ref());
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(target, text.as_ref()).unwrap();
    }
}

fn repository(t: &Temp) -> PathBuf {
    let repository = t.join("repo");
    fs::create_dir_all(repository.join(".git")).unwrap();
    crate::setup::canonical(&repository).unwrap()
}

fn links_of(answer: VaultQueryAnswer) -> Vec<VaultLinkRecord> {
    match answer {
        VaultQueryAnswer::Links(page) => page.items,
        _ => panic!("expected link records"),
    }
}

// ---------- paths and Markdown scanning ----------

#[test]
fn relative_vault_paths_are_strictly_validated() {
    for good in ["a.md", "folder/Note ø.md", "a/b/c.md"] {
        assert!(validate_relative(good).is_ok(), "{good}");
    }
    for bad in ["", "/abs.md", "a//b.md", "../x.md", "a/../b.md", "a\\b.md", "C:/x.md", "a.md:stream", ".obsidian/x.md", "a/.hidden.md", "NUL.md", "dir/COM1.md", "trailing./x.md", "a/ b.md", "a/b.md/"] {
        assert!(validate_relative(bad).is_err(), "{bad}");
    }
}

#[test]
fn scanner_ignores_code_front_matter_and_reads_every_link_form() {
    let text = "---\ntitle: [[hidden]]\n---\n# Title\nSee [[Alpha|a]] and [b](Beta.md#Sec) ![[Gamma#^blk]] `[[code]]` ``a ` [[x]]``\n```\n[[fenced]]\n```\n~~~\n[[tilde]]\n~~~\n[ref]: <Delta.md>\nText ^blk1\n[img](pic.png) [web](https://e.com/a) [self](#Title) [esc]\\(no)";
    let events = markdown::scan_text(text);
    let links: Vec<&markdown::RawLink> = events.iter().filter_map(|e| if let markdown::Event::Link(l) = e { Some(l) } else { None }).collect();
    let summary: Vec<(String, VaultLinkKind, Option<String>)> = links.iter().map(|l| (l.target.clone(), l.kind, l.fragment.clone())).collect();
    assert_eq!(
        summary,
        vec![
            ("Alpha".into(), VaultLinkKind::Wiki, None),
            ("Beta.md".into(), VaultLinkKind::Markdown, Some("Sec".into())),
            ("Gamma".into(), VaultLinkKind::Embed, Some("^blk".into())),
            ("Delta.md".into(), VaultLinkKind::Markdown, None),
            ("pic.png".into(), VaultLinkKind::Markdown, None),
            ("https://e.com/a".into(), VaultLinkKind::Markdown, None),
            ("".into(), VaultLinkKind::Markdown, Some("Title".into())),
        ]
    );
    assert_eq!(links[0].label.as_deref(), Some("a"));
    assert!(links[5].external && links[6].anchor_only);
    assert!(events.iter().any(|e| matches!(e, markdown::Event::Block(id) if id == "blk1")));
    assert!(events.iter().any(|e| matches!(e, markdown::Event::Heading(h) if h.level == 1 && h.text == "Title" && h.line == 4)));
}

#[test]
fn unclosed_dashes_are_not_front_matter() {
    let events = markdown::scan_text("---\n[[Kept]]\nno closing line");
    assert!(events.iter().any(|e| matches!(e, markdown::Event::Link(l) if l.target == "Kept")));
}

// ---------- vault registration ----------

#[test]
fn vault_registration_is_human_confirmed_token_bound_and_preserves_notes() {
    let t = Temp::new();
    let service = service(&t);
    write_notes(&t.join("existing"), &[("keep.md", "# Keep\n")]);
    let preparation = service.prepare_knowledge_vault(ScopeRequest::Global, None, &t.join("existing").to_string_lossy(), VaultMode::Markdown).unwrap();
    assert!(preparation.exists && preparation.existing_markdown_count == 1 && preparation.conflicts.is_empty());
    assert!(!t.join("config").join("knowledge").join("vaults.json").exists(), "preparing writes nothing");
    let binding = service.confirm_knowledge_vault(&preparation.token).unwrap();
    assert_eq!(fs::read_to_string(t.join("existing").join("keep.md")).unwrap(), "# Keep\n");
    assert_eq!(fs::read_dir(t.join("existing")).unwrap().count(), 1, "confirming adds nothing to an existing vault");
    assert!(service.confirm_knowledge_vault(&preparation.token).is_err(), "a token is consumed");
    // Persisted bindings survive a restart.
    let again = KnowledgeService::with_environment(t.join("config"), t.join("home"), Environment::isolated());
    assert_eq!(again.list_knowledge_vaults().unwrap(), vec![binding.clone()]);
    // Rebinding the same scope keeps the vault id (manifests keep pointing at it).
    let moved = bind_global(&service, &t.join("moved"));
    assert_eq!(moved.vault_id, binding.vault_id);
    assert!(t.join("moved").is_dir());
}

#[test]
fn unsafe_and_overlapping_destinations_are_rejected() {
    let t = Temp::new();
    let service = service(&t);
    let home = t.join("home");
    fs::create_dir_all(home.join(".omp")).unwrap();
    let ask = |destination: &Path| service.prepare_knowledge_vault(ScopeRequest::Global, None, &destination.to_string_lossy(), VaultMode::Markdown);
    assert!(ask(&home).is_err(), "home folder");
    assert!(ask(&t.0).is_err(), "a folder containing the home folder");
    assert!(ask(&home.join(".omp").join("notes")).is_err(), ".omp");
    assert!(ask(&t.join("config")).is_err(), "application configuration");
    assert!(ask(&t.join("config").join("knowledge")).is_err());
    assert!(ask(&t.join("a").join("..").join("b")).is_err(), "dot segments");
    assert!(ask(Path::new("relative/folder")).is_err(), "relative");
    assert!(ask(&t.join("x").join(".git").join("vault")).is_err(), "VCS internals");
    service.add_protected_roots(vec![t.join("native-agent")]);
    assert!(ask(&t.join("native-agent").join("skills")).is_err(), "registered protected root");
    // Overlap between two bindings, in both directions, and project/repository safety.
    let global = bind_global(&service, &t.join("g"));
    let repo = repository(&t);
    let project = |destination: &str| service.prepare_knowledge_vault(ScopeRequest::Project { repository_root: None }, Some(repo.to_string_lossy().as_ref()), destination, VaultMode::Markdown);
    let inner = project(&t.join("g").join("inner").to_string_lossy()).unwrap();
    assert!(inner.conflicts.iter().any(|c| c.contains("Overlaps")), "{:?}", inner.conflicts);
    assert!(service.confirm_knowledge_vault(&inner.token).is_err(), "conflicts block confirmation");
    assert!(project(&repo.to_string_lossy()).is_err(), "a project vault cannot be the repository itself");
    assert!(project(&t.0.to_string_lossy()).is_err());
    let default = project("").unwrap();
    assert_eq!(PathBuf::from(&default.root), repo.join("Knowledge"));
    assert!(matches!(&default.scope, KnowledgeScope::Project { repository_root } if fsio::same_path(Path::new(repository_root), &repo)));
    let mismatch = service.prepare_knowledge_vault(ScopeRequest::Project { repository_root: Some(t.join("other").to_string_lossy().into_owned()) }, Some(repo.to_string_lossy().as_ref()), "", VaultMode::Markdown);
    assert!(mismatch.is_err(), "a sent repository root must agree");
    let confirmed = service.confirm_knowledge_vault(&default.token).unwrap();
    assert_ne!(confirmed.vault_id, global.vault_id);
    let capabilities = service.get_knowledge_capabilities(Some(repo.to_string_lossy().as_ref())).unwrap();
    assert_eq!(capabilities.project_vault, Some(confirmed));
    assert_eq!(capabilities.global_vault, Some(global));
    assert_eq!(capabilities.suggested_project_root.as_deref(), Some(repo.join("Knowledge").to_string_lossy().as_ref()));
}

#[test]
fn managed_folder_conflicts_are_reported_and_other_content_is_not_adopted() {
    let t = Temp::new();
    let service = service(&t);
    fs::create_dir_all(t.join("v")).unwrap();
    fs::write(t.join("v").join("SRS"), "not a folder").unwrap();
    let preparation = service.prepare_knowledge_vault(ScopeRequest::Global, None, &t.join("v").to_string_lossy(), VaultMode::Markdown).unwrap();
    assert!(preparation.conflicts.iter().any(|c| c.contains("SRS")), "{:?}", preparation.conflicts);
    assert!(service.confirm_knowledge_vault(&preparation.token).is_err());
}

// ---------- reading, listing, paging ----------

#[test]
fn entries_are_one_level_folders_first_and_paged() {
    let t = Temp::new();
    let service = service(&t);
    let vault = bind_global(&service, &t.join("v"));
    write_notes(&t.join("v"), &[("b.md", "b"), ("A.md", "a"), ("zeta/inner.md", "i"), ("Alpha/x.md", "x"), ("data.json", "{}"), (".obsidian/app.json", "{}"), ("note.txt", "t")]);
    let first = service.list_vault_entries(&vault.vault_id, "", 0, 3).unwrap();
    assert_eq!(first.total, 6, "hidden entries are not listed");
    let names: Vec<_> = first.items.iter().map(|e| (e.name.as_str(), e.kind)).collect();
    assert_eq!(names, vec![("Alpha", VaultEntryKind::Folder), ("zeta", VaultEntryKind::Folder), ("A.md", VaultEntryKind::Markdown)]);
    assert_eq!(first.next_offset, Some(3));
    let second = service.list_vault_entries(&vault.vault_id, "", 3, 200).unwrap();
    assert_eq!(second.items.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), vec!["b.md", "data.json", "note.txt"]);
    assert_eq!(second.items[0].hash.as_deref(), Some(sha256_hex(b"b").as_str()));
    assert!(second.items[1].hash.is_none() && second.items[1].kind == VaultEntryKind::Other);
    assert_eq!(second.next_offset, None);
    let inner = service.list_vault_entries(&vault.vault_id, "zeta", 0, 10).unwrap();
    assert_eq!(inner.items[0].path, "zeta/inner.md");
    for bad in ["../x", "/abs", ".obsidian", "zeta/..", "missing"] {
        assert!(service.list_vault_entries(&vault.vault_id, bad, 0, 10).is_err(), "{bad}");
    }
    assert!(service.list_vault_entries(&vault.vault_id, "", 0, 201).is_err());
    assert!(service.list_vault_entries(&vault.vault_id, "", 0, 0).is_err());
    assert!(service.list_vault_entries("not-a-vault", "", 0, 10).is_err());
}

#[test]
fn documents_page_on_utf8_and_line_boundaries_and_carry_fence_state() {
    let t = Temp::new();
    let service = service(&t);
    let vault = bind_global(&service, &t.join("v"));
    let text = format!("```text\n{}```\nafter [[x]]\n", "line é ø\n".repeat(12_000));
    write_notes(&t.join("v"), &[("long.md", &text)]);
    let mut offset = 0u64;
    let mut joined = String::new();
    let mut pages = 0;
    let mut hash = None;
    loop {
        let page = service.read_vault_document(&vault.vault_id, "long.md", offset, hash.as_deref()).unwrap();
        assert!(page.length <= 64 * 1024 && page.text.len() as u64 == page.length && page.byte_offset == offset);
        assert_eq!(page.byte_length, text.len() as u64);
        assert!(page.text.ends_with('\n'), "pages end on a line boundary when one exists");
        assert!(!page.starts_mid_line);
        if pages == 0 {
            assert!(page.fence.is_none() && page.line_number == 1);
        } else {
            assert_eq!(page.fence.as_ref().map(|f| (f.marker.as_str(), f.length)), Some(("`", 3)), "the open fence continues on the next page");
            assert!(page.line_number > 1);
        }
        hash = Some(page.hash.clone());
        joined.push_str(&page.text);
        pages += 1;
        match page.next_offset {
            Some(next) => offset = next,
            None => break,
        }
    }
    assert!(pages >= 2);
    assert_eq!(joined, text);
    assert_eq!(hash.as_deref(), Some(sha256_hex(text.as_bytes()).as_str()));
    assert!(service.read_vault_document(&vault.vault_id, "long.md", 0, Some("deadbeef")).is_err(), "a changed hash is an error, not a mix");
    assert!(service.read_vault_document(&vault.vault_id, "long.md", text.len() as u64, None).is_err());
    for bad in ["../long.md", "long.txt", ".obsidian/x.md", "long.md/"] {
        assert!(service.read_vault_document(&vault.vault_id, bad, 0, None).is_err(), "{bad}");
    }
}

#[test]
fn one_huge_line_is_split_on_character_boundaries() {
    let t = Temp::new();
    let service = service(&t);
    let vault = bind_global(&service, &t.join("v"));
    let text = "€".repeat(100_000);
    write_notes(&t.join("v"), &[("line.md", &text)]);
    let first = service.read_vault_document(&vault.vault_id, "line.md", 0, None).unwrap();
    let second = service.read_vault_document(&vault.vault_id, "line.md", first.next_offset.unwrap(), Some(first.hash.as_str())).unwrap();
    assert!(second.starts_mid_line);
    assert!(first.length % 3 == 0 && first.length < 64 * 1024, "the page backs off to a character boundary");
    let mut joined = first.text.clone();
    let mut next = second.next_offset;
    joined.push_str(&second.text);
    while let Some(offset) = next {
        let page = service.read_vault_document(&vault.vault_id, "line.md", offset, Some(first.hash.as_str())).unwrap();
        joined.push_str(&page.text);
        next = page.next_offset;
    }
    assert_eq!(joined, text);
}

#[test]
fn invalid_utf8_notes_are_reported_not_shown() {
    let t = Temp::new();
    let service = service(&t);
    let vault = bind_global(&service, &t.join("v"));
    fs::create_dir_all(t.join("v")).unwrap();
    fs::write(t.join("v").join("bad.md"), [b'o', b'k', 0xFF, 0xFE, b'\n']).unwrap();
    write_notes(&t.join("v"), &[("good.md", "# Good\n")]);
    assert!(service.read_vault_document(&vault.vault_id, "bad.md", 0, None).unwrap_err().contains("UTF-8"));
    let refresh = service.refresh_vault_index(&vault.vault_id).unwrap();
    assert_eq!((refresh.notes, refresh.unreadable), (1, 1));
    assert!(refresh.warnings.iter().any(|w| w.contains("bad.md")));
}

#[test]
fn hard_linked_notes_never_leak_their_bytes() {
    let t = Temp::new();
    let service = service(&t);
    let vault = bind_global(&service, &t.join("v"));
    fs::write(t.join("secret.txt"), "TOKEN=hunter2").unwrap();
    if fs::hard_link(t.join("secret.txt"), t.join("v").join("leak.md")).is_err() {
        eprintln!("Skipping: hard links are not available here");
        return;
    }
    write_notes(&t.join("v"), &[("fine.md", "fine")]);
    let error = service.read_vault_document(&vault.vault_id, "leak.md", 0, None).unwrap_err();
    assert!(error.contains("multiply linked") && !error.contains("hunter2"), "{error}");
    let refresh = service.refresh_vault_index(&vault.vault_id).unwrap();
    assert_eq!((refresh.notes, refresh.unreadable), (1, 1));
    let root = service.vault_root(&vault.vault_id).unwrap();
    assert!(root.read_markdown("leak.md").is_err());
    let replace = root.write_markdown("leak.md", "x", WritePrecondition::Replace { expected_hash: sha256_hex(b"TOKEN=hunter2") });
    assert!(replace.is_err(), "a hard-linked target is never overwritten");
    assert_eq!(fs::read_to_string(t.join("secret.txt")).unwrap(), "TOKEN=hunter2");
}

#[test]
fn writers_are_atomic_precondition_checked_and_markdown_only() {
    let t = Temp::new();
    let service = service(&t);
    let vault = bind_global(&service, &t.join("v"));
    let root = service.vault_root(&vault.vault_id).unwrap();
    let receipt = root.write_markdown("SRS/job/SRS.md", "# SRS\n", WritePrecondition::Create).unwrap();
    assert_eq!(receipt.hash, sha256_hex(b"# SRS\n"));
    assert!(root.write_markdown("SRS/job/SRS.md", "again", WritePrecondition::Create).is_err());
    assert!(root.write_markdown("SRS/job/SRS.md", "x", WritePrecondition::Replace { expected_hash: "00".into() }).is_err());
    root.write_markdown("SRS/job/SRS.md", "# SRS 2\n", WritePrecondition::Replace { expected_hash: receipt.hash }).unwrap();
    assert_eq!(String::from_utf8(root.read_markdown("SRS/job/SRS.md").unwrap()).unwrap(), "# SRS 2\n");
    for bad in ["tool.py", "script.sh", "x.md.exe", "../x.md", ".obsidian/x.md", "SRS/job/srs.MD2"] {
        assert!(root.write_markdown(bad, "x", WritePrecondition::Create).is_err(), "{bad}");
    }
    assert!(root.write_markdown("SRS/job/srs.md", "x", WritePrecondition::Create).is_err(), "case-colliding sibling");
    let names: Vec<_> = fs::read_dir(t.join("v").join("SRS").join("job")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(names, vec!["SRS.md".to_string()], "no temporary files remain");
}

// ---------- link index ----------

fn link_fixture(t: &Temp, service: &KnowledgeService) -> VaultBinding {
    let vault = bind_global(service, &t.join("v"));
    write_notes(
        &t.join("v"),
        &[
            ("index.md", "# Index\n[[Alpha]] [[sub/Beta]] [[Dup]] [[missing]] [x](assets/logo.png) [[Alpha#Heading Two]] [[Alpha#Nope]] [[#Index]] ![[Alpha]] [[alpha]]\n"),
            ("Alpha.md", "# Alpha\n## Heading Two\nback [up](index.md)\n"),
            ("sub/Beta.md", "[[../Alpha]] [self](Beta.md)\n"),
            ("a/Dup.md", "dup a"),
            ("b/Dup.md", "dup b"),
            ("Isolated.md", "nothing links here"),
            ("code.md", "```\n[[Alpha]]\n```\n`[[Alpha]]`\n"),
        ],
    );
    vault
}

#[test]
fn links_resolve_with_one_authority_and_stay_explicit_when_unresolved() {
    let t = Temp::new();
    let service = service(&t);
    let vault = link_fixture(&t, &service);
    let refreshed = service.refresh_vault_index(&vault.vault_id).unwrap();
    assert_eq!((refreshed.notes, refreshed.revision, refreshed.changed), (7, 1, true));
    let records = links_of(service.query_vault_links(&vault.vault_id, &VaultLinkQuery::Neighbors { path: "index.md".into(), offset: 0, limit: 200 }).unwrap());
    let outgoing: Vec<&VaultLinkRecord> = records.iter().filter(|r| r.source == "index.md").collect();
    let status = |raw: &str| outgoing.iter().find(|r| r.raw == raw).unwrap_or_else(|| panic!("no link {raw}"));
    assert_eq!(status("Alpha").status, VaultLinkStatus::Resolved);
    assert_eq!(status("Alpha").target.as_deref(), Some("Alpha.md"));
    assert_eq!(status("sub/Beta").target.as_deref(), Some("sub/Beta.md"));
    assert_eq!(status("Dup").status, VaultLinkStatus::Ambiguous);
    assert_eq!(status("Dup").candidates, vec!["a/Dup.md".to_string(), "b/Dup.md".to_string()]);
    assert!(status("Dup").target.is_none());
    assert_eq!(status("missing").status, VaultLinkStatus::Missing);
    assert_eq!(status("assets/logo.png").status, VaultLinkStatus::NonDocument);
    assert_eq!(status("alpha").target.as_deref(), Some("Alpha.md"), "unique case-insensitive basename");
    assert_eq!(status("#Index").status, VaultLinkStatus::Anchor);
    assert!(outgoing.iter().any(|r| r.kind == VaultLinkKind::Embed && r.target.as_deref() == Some("Alpha.md")));
    let heading = outgoing.iter().find(|r| r.fragment.as_deref() == Some("Heading Two")).unwrap();
    assert!(!heading.anchor_missing);
    assert!(outgoing.iter().find(|r| r.fragment.as_deref() == Some("Nope")).unwrap().anchor_missing);
    // Backlinks come from real occurrences only: code spans and fences are not links.
    let backlinks = links_of(service.query_vault_links(&vault.vault_id, &VaultLinkQuery::Backlinks { path: "Alpha.md".into(), offset: 0, limit: 200 }).unwrap());
    assert_eq!(backlinks.iter().filter(|r| r.source == "index.md").count(), 5);
    assert_eq!(backlinks.iter().filter(|r| r.source == "sub/Beta.md").count(), 1);
    assert!(backlinks.iter().all(|r| r.source != "code.md"));
    // A self-link is a resolved link whose target is its source.
    let beta = links_of(service.query_vault_links(&vault.vault_id, &VaultLinkQuery::Neighbors { path: "sub/Beta.md".into(), offset: 0, limit: 200 }).unwrap());
    assert!(beta.iter().any(|r| r.source == "sub/Beta.md" && r.target.as_deref() == Some("sub/Beta.md")));
    // Every note, isolated ones included, is a record with honest counts.
    let notes = match service.query_vault_links(&vault.vault_id, &VaultLinkQuery::Notes { offset: 0, limit: 200 }).unwrap() {
        VaultQueryAnswer::Notes(page) => page,
        _ => panic!("expected notes"),
    };
    assert_eq!(notes.total, 7);
    let isolated = notes.items.iter().find(|n| n.path == "Isolated.md").unwrap();
    assert_eq!((isolated.outgoing, isolated.incoming), (0, 0));
    assert_eq!(notes.items.iter().find(|n| n.path == "code.md").unwrap().outgoing, 0);
    assert_eq!(notes.items.iter().find(|n| n.path == "Alpha.md").unwrap().title, "Alpha");
    let hits = match service.query_vault_links(&vault.vault_id, &VaultLinkQuery::Search { text: "isolat".into(), offset: 0, limit: 10 }).unwrap() {
        VaultQueryAnswer::Search(page) => page.items,
        _ => panic!("expected hits"),
    };
    assert_eq!(hits.first().map(|h| h.path.as_str()), Some("Isolated.md"));
    // Resolve answers in target order with the same algorithm.
    let resolved = match service
        .query_vault_links(&vault.vault_id, &VaultLinkQuery::Resolve { from_path: "sub/Beta.md".into(), targets: vec!["../Alpha".into(), "Alpha".into(), "https://x.y/z".into(), "#frag".into(), "Dup".into(), "nothing.txt".into(), "Alpha#Heading%20Two".into()] })
        .unwrap()
    {
        VaultQueryAnswer::Resolve(page) => page.items,
        _ => panic!("expected resolutions"),
    };
    let found: Vec<(VaultLinkStatus, Option<&str>)> = resolved.iter().map(|r| (r.status, r.path.as_deref())).collect();
    assert_eq!(
        found,
        vec![
            (VaultLinkStatus::Resolved, Some("Alpha.md")),
            (VaultLinkStatus::Resolved, Some("Alpha.md")),
            (VaultLinkStatus::External, None),
            (VaultLinkStatus::Anchor, None),
            (VaultLinkStatus::Ambiguous, None),
            (VaultLinkStatus::NonDocument, None),
            (VaultLinkStatus::Resolved, Some("Alpha.md")),
        ]
    );
    assert!(service.query_vault_links(&vault.vault_id, &VaultLinkQuery::Backlinks { path: "nope.md".into(), offset: 0, limit: 10 }).is_err());
    assert!(service.query_vault_links(&vault.vault_id, &VaultLinkQuery::Links { offset: 0, limit: 0 }).is_err());
    let too_many = VaultLinkQuery::Resolve { from_path: String::new(), targets: vec!["a".into(); 201] };
    assert!(service.query_vault_links(&vault.vault_id, &too_many).is_err());
}

#[test]
fn query_unions_are_strict() {
    assert!(serde_json::from_value::<VaultLinkQuery>(json!({"kind":"notes","offset":0,"limit":5})).is_ok());
    assert!(serde_json::from_value::<VaultLinkQuery>(json!({"kind":"resolve","fromPath":"a.md","targets":["b"]})).is_ok());
    assert!(serde_json::from_value::<VaultLinkQuery>(json!({"kind":"notes","offset":0,"limit":5,"extra":1})).is_err());
    assert!(serde_json::from_value::<VaultLinkQuery>(json!({"kind":"shell","command":"x"})).is_err());
    assert!(serde_json::from_value::<GraphifyQuery>(json!({"kind":"node","id":{"id":"a","idKind":"text"}})).is_ok());
    assert!(serde_json::from_value::<GraphifyQuery>(json!({"kind":"node","id":{"id":"a","idKind":"other"}})).is_err());
    assert!(serde_json::from_value::<GraphifyQuery>(json!({"kind":"search","text":"x","offset":0,"limit":1,"path":"/etc"})).is_err());
}

#[test]
fn refresh_publishes_a_revision_only_when_content_changes() {
    let t = Temp::new();
    let service = service(&t);
    let vault = link_fixture(&t, &service);
    let first = service.refresh_vault_index(&vault.vault_id).unwrap();
    let same = service.refresh_vault_index(&vault.vault_id).unwrap();
    assert_eq!((same.revision, same.changed, same.hash.as_str()), (first.revision, false, first.hash.as_str()));
    write_notes(&t.join("v"), &[("New.md", "# New\n[[Alpha]]\n")]);
    let third = service.refresh_vault_index(&vault.vault_id).unwrap();
    assert_eq!((third.revision, third.changed, third.notes), (first.revision + 1, true, 8));
    // Entries and documents report the current revision so a late reply can be fenced.
    assert_eq!(service.list_vault_entries(&vault.vault_id, "", 0, 1).unwrap().revision, third.revision);
    // A vanished vault folder is an explicit error, never an empty success.
    fs::remove_dir_all(t.join("v")).unwrap();
    assert!(service.refresh_vault_index(&vault.vault_id).unwrap_err().contains("missing"));
}

// ---------- Obsidian ----------

fn fake_obsidian(t: &Temp, registered: &Path) -> Environment {
    let exe = t.join("obsidian").join(if cfg!(windows) { "Obsidian.exe" } else { "obsidian" });
    write_notes(&t.join("obsidian"), &[("registry.json", &json!({"vaults": {"abc123": {"path": registered.to_string_lossy(), "ts": 1}, "gone": {"path": t.join("not-here").to_string_lossy()}}}).to_string())]);
    fs::write(&exe, b"not a real program").unwrap();
    Environment { obsidian_exe: ExeSetting::Fixed(exe), obsidian_registry: Some(t.join("obsidian").join("registry.json")), cognee_home: None, protected: Vec::new() }
}

#[test]
fn obsidian_is_discovered_passively_and_never_adopted() {
    let t = Temp::new();
    let personal = t.join("personal vault");
    fs::create_dir_all(&personal).unwrap();
    let service = service_with(&t, fake_obsidian(&t, &personal));
    let capability = service.get_knowledge_capabilities(None).unwrap().obsidian;
    assert_eq!(capability.state, ObsidianState::Installed);
    assert_eq!(capability.registered_vaults, vec![personal.to_string_lossy().into_owned()], "existing vaults are options only, and vanished ones are not offered");
    assert!(service.list_knowledge_vaults().unwrap().is_empty(), "a registered personal vault is not consent to use it");
    assert!(!t.join("config").join("knowledge").exists(), "probing writes nothing");
    // Absent: Obsidian mode is refused, Markdown mode still works.
    let absent = service_with(&Temp::new(), Environment::isolated());
    let absent_state = absent.get_knowledge_capabilities(None).unwrap().obsidian;
    assert_eq!((absent_state.state, absent_state.executable), (ObsidianState::Absent, None));
    assert!(absent.prepare_knowledge_vault(ScopeRequest::Global, None, "", VaultMode::Obsidian).is_err());
}

#[test]
fn open_in_obsidian_reports_unregistered_vaults_without_launching() {
    let t = Temp::new();
    let personal = t.join("personal");
    fs::create_dir_all(&personal).unwrap();
    let service = service_with(&t, fake_obsidian(&t, &personal));
    let preparation = service.prepare_knowledge_vault(ScopeRequest::Global, None, &t.join("fresh").to_string_lossy(), VaultMode::Obsidian).unwrap();
    let vault = service.confirm_knowledge_vault(&preparation.token).unwrap();
    write_notes(&t.join("fresh"), &[("SRS/job/SRS.md", "# SRS\n"), ("x.txt", "t")]);
    // The folder is unknown to the (fake) Obsidian registry: nothing is launched and the human is told what to do.
    let result = service.open_knowledge_note(&vault.vault_id, Some("SRS/job/SRS.md")).unwrap();
    assert!(!result.opened && result.needs_vault_registration);
    assert!(result.uri.starts_with("obsidian://open?path=") && !result.uri.contains('\\') && !result.uri.contains(' '), "{}", result.uri);
    assert!(result.uri.contains("SRS%2Fjob%2FSRS.md"));
    assert_eq!(PathBuf::from(&result.vault_path), t.join("fresh"));
    for bad in ["x.txt", "../x.md", "SRS/job/missing.md", ".obsidian/x.md"] {
        assert!(service.open_knowledge_note(&vault.vault_id, Some(bad)).is_err(), "{bad}");
    }
    // Markdown-mode vaults never go to Obsidian.
    let repo = repository(&t);
    let plain = service.prepare_knowledge_vault(ScopeRequest::Project { repository_root: None }, Some(repo.to_string_lossy().as_ref()), "", VaultMode::Markdown).unwrap();
    let plain = service.confirm_knowledge_vault(&plain.token).unwrap();
    assert!(service.open_knowledge_note(&plain.vault_id, None).is_err());
}

#[test]
fn declining_obsidian_is_remembered_per_project() {
    let t = Temp::new();
    let service = service(&t);
    let repo = repository(&t);
    let other = t.join("other");
    fs::create_dir_all(other.join(".git")).unwrap();
    service.record_obsidian_decline(&repo.to_string_lossy()).unwrap();
    assert!(service.get_knowledge_capabilities(Some(repo.to_string_lossy().as_ref())).unwrap().obsidian.declined_for_project);
    assert!(!service.get_knowledge_capabilities(Some(other.to_string_lossy().as_ref())).unwrap().obsidian.declined_for_project);
    let again = KnowledgeService::with_environment(t.join("config"), t.join("home"), Environment::isolated());
    assert!(again.get_knowledge_capabilities(Some(repo.to_string_lossy().as_ref())).unwrap().obsidian.declined_for_project);
}

// ---------- Cognee ----------

fn fake_cognee(root: &Path) {
    let (cli, packages) = if cfg!(windows) { ("venv/Scripts/cognee-cli.exe", "venv/Lib/site-packages") } else { ("venv/bin/cognee-cli", "venv/lib/python3.12/site-packages") };
    write_notes(root, &[(cli.to_string(), "binary"), (format!("{packages}/cognee-1.6.4.dist-info/METADATA"), "Name: cognee"), (format!("{packages}/cognee_db_workers/x.py"), "")]);
}

#[test]
fn cognee_urls_are_loopback_only_and_credential_free() {
    for good in ["http://127.0.0.1:3000", "https://localhost", "http://[::1]:8080/ui", "http://LOCALHOST:3000/app"] {
        assert!(validate_cognee_url(good).is_ok(), "{good}");
    }
    for bad in [
        "http://example.com",
        "http://localhost.evil.com",
        "http://user:pw@127.0.0.1",
        "http://127.0.0.1?x=1",
        "http://127.0.0.1#frag",
        "http://127.0.0.2",
        "file:///etc/passwd",
        "javascript:alert(1)",
        "ftp://127.0.0.1",
        "data:text/html,x",
        "http://127.0.0.1 /x",
        "",
        "not a url",
    ] {
        assert!(validate_cognee_url(bad).is_err(), "{bad}");
    }
}

#[test]
fn cognee_is_found_by_files_alone_and_opens_only_a_configured_ui() {
    let t = Temp::new();
    fake_cognee(&t.join("cognee"));
    fs::create_dir_all(t.join("empty")).unwrap();
    let env = Environment { cognee_home: Some(t.join("cognee")), ..Environment::isolated() };
    let service = service_with(&t, env);
    let probe = service.get_knowledge_capabilities(None).unwrap().cognee;
    assert_eq!(probe.installed, Some(true));
    assert_eq!(probe.version.as_deref(), Some("1.6.4"));
    assert_eq!(probe.message.as_deref(), Some("Cognee is installed; its UI is not configured"));
    assert!(probe.ui_url.is_none());
    assert!(service.open_cognee_ui().unwrap_err().contains("not configured"));
    assert!(service.configure_cognee_ui(Some("http://example.com"), None).is_err());
    assert!(service.configure_cognee_ui(Some("http://127.0.0.1:3000"), Some(&t.join("empty").to_string_lossy())).is_err(), "an alternate folder must contain Cognee");
    let configured = service.configure_cognee_ui(Some("http://127.0.0.1:3000"), None).unwrap();
    assert_eq!(configured.ui_url.as_deref(), Some("http://127.0.0.1:3000/"));
    assert!(configured.message.is_none());
    let restarted = KnowledgeService::with_environment(t.join("config"), t.join("home"), Environment { cognee_home: Some(t.join("cognee")), ..Environment::isolated() });
    assert_eq!(restarted.get_knowledge_capabilities(None).unwrap().cognee.ui_url.as_deref(), Some("http://127.0.0.1:3000/"));
    let cleared = restarted.configure_cognee_ui(None, None).unwrap();
    assert!(cleared.ui_url.is_none());
    // Missing installation: not installed and nothing to open.
    let missing = service_with(&Temp::new(), Environment { cognee_home: Some(t.join("empty")), ..Environment::isolated() });
    assert_eq!(missing.get_knowledge_capabilities(None).unwrap().cognee.installed, Some(false));
    assert!(missing.open_cognee_ui().is_err());
}

// ---------- Graphify ----------

fn write_graph(repository: &Path, graph: &Value, marker: Option<impl AsRef<str>>) {
    let out = repository.join("graphify-out");
    fs::create_dir_all(&out).unwrap();
    fs::write(out.join("graph.json"), serde_json::to_vec(graph).unwrap()).unwrap();
    match marker {
        Some(marker) => fs::write(out.join(".graphify_root"), marker.as_ref()).unwrap(),
        None => {
            let _ = fs::remove_file(out.join(".graphify_root"));
        }
    }
}

fn sample_graph() -> Value {
    json!({
        "directed": true,
        "multigraph": true,
        "graph": {"graphify_version": "0.9.92", "schema_version": 1},
        "nodes": [
            {"id": "a", "label": "A", "community": 1, "community_name": "Core", "source_file": "x.py", "source_location": "L1", "_origin": "ast", "weird": [1, 2]},
            {"id": 1, "label": "One"},
            {"id": "1", "label": "Text one"},
            {"id": "c", "label": "C", "community": 1, "community_name": "Core"}
        ],
        "links": [
            {"source": "a", "target": "c", "relation": "calls", "confidence": "EXTRACTED", "confidence_score": 1.0, "key": 0, "_origin": "ast", "source_file": "x.py", "source_location": "L9"},
            {"source": "a", "target": "c", "relation": "imports", "confidence": "INFERRED", "key": 1},
            {"source": "c", "target": "c", "relation": "self", "id": "edge-7", "confidence": "AMBIGUOUS"},
            {"source": 1, "target": "1", "relation": "numeric-vs-text", "directed": false},
            {"source": "a", "target": "ghost", "relation": "calls"}
        ],
        "hyperedges": [{"id": "h1", "label": "Group", "nodes": ["a", "c", 1], "confidence": "AMBIGUOUS", "source_file": "x.py"}]
    })
}

fn page_items(service: &KnowledgeService, id: &str, section: GraphifySection) -> Vec<Value> {
    service.read_graphify_graph(id, section, 0, 200).unwrap().items
}

#[test]
fn graphify_is_conditional_on_the_project_output_and_its_provenance() {
    let t = Temp::new();
    let service = service(&t);
    let repo = repository(&t);
    let folder = repo.to_string_lossy().into_owned();
    // Installed grammars or a copied workspace elsewhere are not evidence: no output, no button.
    let absent = service.get_knowledge_capabilities(Some(folder.as_str())).unwrap().graphify;
    assert_eq!((absent.present, absent.state), (false, GraphifyState::Absent));
    assert!(!t.join("config").join("knowledge").join("graphify").exists(), "a probe never creates a snapshot");
    write_graph(&repo, &sample_graph(), Some(&format!("{}\n", repo.to_string_lossy())));
    let present = service.get_knowledge_capabilities(Some(folder.as_str())).unwrap().graphify;
    assert_eq!((present.present, present.state), (true, GraphifyState::Present));
    assert!(present.artifact_id.is_none() && present.artifact_hash.is_none(), "an artifact is admitted only when Graphify is clicked");
    assert!(!t.join("config").join("knowledge").join("graphify").exists());
    write_graph(&repo, &sample_graph(), None::<&str>);
    let unverified = service.get_knowledge_capabilities(Some(folder.as_str())).unwrap().graphify;
    assert_eq!(unverified.state, GraphifyState::Unverified);
    assert_eq!(service.open_graphify_artifact(&folder).unwrap().provenance, "unverified");
    write_graph(&repo, &sample_graph(), Some("C:/somewhere/else/entirely"));
    let foreign = service.get_knowledge_capabilities(Some(folder.as_str())).unwrap().graphify;
    assert_eq!((foreign.state, foreign.source_root.as_deref()), (GraphifyState::Foreign, Some("C:/somewhere/else/entirely")));
    assert!(service.open_graphify_artifact(&folder).unwrap_err().contains("Foreign"));
    write_graph(&repo, &json!("not an object"), Some(&repo.to_string_lossy()));
    fs::write(repo.join("graphify-out").join("graph.json"), "<html>nope</html>").unwrap();
    let malformed = service.get_knowledge_capabilities(Some(folder.as_str())).unwrap().graphify;
    assert_eq!((malformed.present, malformed.state), (true, GraphifyState::Malformed));
    assert!(malformed.message.is_some(), "a malformed artifact explains itself");
    fs::write(repo.join("graphify-out").join("graph.json"), "").unwrap();
    assert_eq!(service.get_knowledge_capabilities(Some(folder.as_str())).unwrap().graphify.state, GraphifyState::Malformed);
    // No project folder at all: nothing to show.
    assert_eq!(service.get_knowledge_capabilities(None).unwrap().graphify.state, GraphifyState::Absent);
}

#[test]
fn graphify_admission_preserves_ids_directedness_multigraph_loops_and_provenance() {
    let t = Temp::new();
    let service = service(&t);
    let repo = repository(&t);
    let folder = repo.to_string_lossy().into_owned();
    write_graph(&repo, &sample_graph(), Some(&repo.to_string_lossy()));
    let artifact = service.open_graphify_artifact(&folder).unwrap();
    let expected_hash = sha256_hex(&fs::read(repo.join("graphify-out").join("graph.json")).unwrap());
    assert_eq!(artifact.artifact_hash, expected_hash);
    assert_eq!((artifact.counts.nodes, artifact.counts.links, artifact.counts.hyperedges, artifact.counts.communities), (4, 5, 1, 1));
    assert!(artifact.directed && artifact.multigraph && artifact.directed_declared);
    assert_eq!((artifact.provenance.as_str(), artifact.self_loops, artifact.dangling_links), ("verified", 1, 1));
    assert_eq!(artifact.metadata["graph"]["graphify_version"], "0.9.92");
    assert_eq!(artifact.provenance_counts["EXTRACTED"], 1);
    assert!(artifact.issues.iter().any(|i| i.message.contains("does not exist")), "dangling links are reported, not dropped");
    assert_eq!(service.open_graphify_artifact(&folder).unwrap().artifact_id, artifact.artifact_id, "the same bytes keep the same immutable artifact");

    let nodes = page_items(&service, &artifact.artifact_id, GraphifySection::Nodes);
    assert_eq!(nodes.len(), 4);
    assert_eq!(nodes[1]["ref"], json!({"id": "1", "idKind": "number"}));
    assert_eq!(nodes[2]["ref"], json!({"id": "1", "idKind": "text"}));
    assert_eq!(nodes[0]["attributes"]["weird"], json!([1, 2]));
    assert_eq!((nodes[0]["community"].clone(), nodes[0]["origin"].clone(), nodes[0]["sourceLocation"].clone()), (json!(1), json!("ast"), json!("L1")));

    let links = page_items(&service, &artifact.artifact_id, GraphifySection::Links);
    assert_eq!(links.len(), 5);
    assert_eq!(links[0]["id"], format!("graphify:{expected_hash}:link:0"));
    assert_eq!(links[1]["id"], format!("graphify:{expected_hash}:link:1"));
    assert_eq!((links[0]["confidence"].clone(), links[0]["confidenceScore"].clone(), links[0]["key"].clone()), (json!("EXTRACTED"), json!(1.0), json!("0")));
    assert_eq!(links[0]["attributes"]["key"], json!(0), "the original multigraph key stays inspectable");
    assert_eq!(links[2]["id"], "edge-7");
    assert_eq!(links[2]["nativeId"], "edge-7");
    assert_eq!(links[2]["confidence"], "AMBIGUOUS");
    assert_eq!((links[3]["directed"].clone(), links[3]["source"].clone(), links[3]["target"].clone()), (json!(false), json!({"id": "1", "idKind": "number"}), json!({"id": "1", "idKind": "text"})));
    assert_eq!((links[0]["directed"].clone(), links[4]["dangling"].clone(), links[0]["dangling"].clone()), (json!(true), json!(true), json!(false)));
    assert_eq!(links[0]["source"], links[1]["source"]);
    assert_ne!(links[0]["id"], links[1]["id"], "parallel edges stay separate records");

    let hyperedges = page_items(&service, &artifact.artifact_id, GraphifySection::Hyperedges);
    assert_eq!(hyperedges[0]["id"], "h1");
    assert_eq!(hyperedges[0]["nodeIds"], json!([{"id": "a", "idKind": "text"}, {"id": "c", "idKind": "text"}, {"id": "1", "idKind": "number"}]));
    assert_eq!(page_items(&service, &artifact.artifact_id, GraphifySection::Communities), vec![json!({"id": 1, "label": "Core", "size": 2})]);
    let metadata = service.read_graphify_graph(&artifact.artifact_id, GraphifySection::Metadata, 0, 1).unwrap();
    assert_eq!((metadata.total, metadata.items.len(), metadata.artifact_hash.as_str()), (1, 1, expected_hash.as_str()));
    let paged = service.read_graphify_graph(&artifact.artifact_id, GraphifySection::Links, 2, 2).unwrap();
    assert_eq!((paged.total, paged.items.len(), paged.next_offset), (5, 2, Some(4)));
    assert!(service.read_graphify_graph(&artifact.artifact_id, GraphifySection::Links, 0, 201).is_err());
    assert!(service.read_graphify_graph("../../etc", GraphifySection::Nodes, 0, 10).is_err());
    assert!(service.read_graphify_graph(&"0".repeat(32), GraphifySection::Nodes, 0, 10).unwrap_err().contains("no longer available"));
}

#[test]
fn graphify_queries_are_exact_directional_and_type_aware() {
    let t = Temp::new();
    let service = service(&t);
    let repo = repository(&t);
    write_graph(&repo, &sample_graph(), Some(&repo.to_string_lossy()));
    let id = service.open_graphify_artifact(&repo.to_string_lossy()).unwrap().artifact_id;
    let text = |id: &str| GraphifyRef { id: id.into(), id_kind: GraphifyIdKind::Text };
    let number = |id: &str| GraphifyRef { id: id.into(), id_kind: GraphifyIdKind::Number };
    let ordinals = |page: &GraphifyQueryPage| page.items.iter().map(|i| i["ordinal"].as_u64().unwrap()).collect::<Vec<_>>();

    let out = service.query_graphify_graph(&id, &GraphifyQuery::Neighbors { id: text("a"), direction: GraphifyDirection::Out, offset: 0, limit: 10 }).unwrap();
    assert_eq!((out.total, ordinals(&out)), (3, vec![0, 1, 4]));
    assert_eq!(out.items[0]["sourceNode"]["label"], "A");
    assert_eq!(out.items[0]["targetNode"]["label"], "C");
    assert_eq!(out.items[2]["targetNode"], Value::Null, "a dangling endpoint is not invented");
    let incoming = service.query_graphify_graph(&id, &GraphifyQuery::Neighbors { id: text("c"), direction: GraphifyDirection::In, offset: 0, limit: 10 }).unwrap();
    assert_eq!(ordinals(&incoming), vec![0, 1, 2], "the self-loop appears once");
    // An undirected link is a neighbour in both directions; the number 1 is not the text "1".
    let undirected = service.query_graphify_graph(&id, &GraphifyQuery::Neighbors { id: number("1"), direction: GraphifyDirection::In, offset: 0, limit: 10 }).unwrap();
    assert_eq!(ordinals(&undirected), vec![3]);
    let directed_in = service.query_graphify_graph(&id, &GraphifyQuery::Neighbors { id: text("a"), direction: GraphifyDirection::In, offset: 0, limit: 10 }).unwrap();
    assert_eq!(directed_in.total, 0);
    let both = service.query_graphify_graph(&id, &GraphifyQuery::Neighbors { id: text("1"), direction: GraphifyDirection::Both, offset: 0, limit: 10 }).unwrap();
    assert_eq!(ordinals(&both), vec![3]);
    assert!(service.query_graphify_graph(&id, &GraphifyQuery::Neighbors { id: text("nope"), direction: GraphifyDirection::Both, offset: 0, limit: 10 }).is_err());
    assert!(service.query_graphify_graph(&id, &GraphifyQuery::Neighbors { id: number("a"), direction: GraphifyDirection::Both, offset: 0, limit: 10 }).is_err());

    let node = service.query_graphify_graph(&id, &GraphifyQuery::Node { id: number("1") }).unwrap();
    assert_eq!(node.items[0]["label"], "One");
    assert_eq!(service.query_graphify_graph(&id, &GraphifyQuery::Node { id: text("1") }).unwrap().items[0]["label"], "Text one");
    let members = service.query_graphify_graph(&id, &GraphifyQuery::CommunityMembers { community: 1, offset: 0, limit: 10 }).unwrap();
    assert_eq!(members.items.iter().map(|n| n["label"].as_str().unwrap().to_string()).collect::<Vec<_>>(), vec!["A", "C"]);
    assert_eq!(service.query_graphify_graph(&id, &GraphifyQuery::CommunityMembers { community: 99, offset: 0, limit: 10 }).unwrap().total, 0);
    let groups = service.query_graphify_graph(&id, &GraphifyQuery::HyperedgesOf { id: text("c"), offset: 0, limit: 10 }).unwrap();
    assert_eq!(groups.items[0]["id"], "h1");
    assert_eq!(service.query_graphify_graph(&id, &GraphifyQuery::HyperedgesOf { id: text("1"), offset: 0, limit: 10 }).unwrap().total, 0, "text 1 is not a member; number 1 is");
    assert_eq!(service.query_graphify_graph(&id, &GraphifyQuery::HyperedgesOf { id: number("1"), offset: 0, limit: 10 }).unwrap().total, 1);
    let search = service.query_graphify_graph(&id, &GraphifyQuery::Search { text: "Core".into(), offset: 0, limit: 10 }).unwrap();
    assert_eq!(search.items.iter().filter(|i| i["kind"] == "node").count(), 2);
    let exact = service.query_graphify_graph(&id, &GraphifyQuery::Search { text: "a".into(), offset: 0, limit: 1 }).unwrap();
    assert_eq!(exact.items[0]["node"]["ref"]["id"], "a", "an exact id ranks first");
    assert!(exact.total > 1 && exact.next_offset == Some(1));
    assert!(service.query_graphify_graph(&id, &GraphifyQuery::Search { text: "  ".into(), offset: 0, limit: 10 }).is_err());
}

#[test]
fn graphify_replacement_never_mixes_builds_and_bad_graphs_are_rejected() {
    let t = Temp::new();
    let service = service(&t);
    let repo = repository(&t);
    let folder = repo.to_string_lossy().into_owned();
    write_graph(&repo, &sample_graph(), Some(&folder));
    let old = service.open_graphify_artifact(&folder).unwrap();
    // Live replacement: the old artifact stops answering, a refresh admits a new immutable one.
    let mut next = sample_graph();
    next["nodes"].as_array_mut().unwrap().push(json!({"id": "extra", "label": "Extra"}));
    write_graph(&repo, &next, Some(&folder));
    assert_eq!(service.read_graphify_graph(&old.artifact_id, GraphifySection::Nodes, 0, 10).unwrap().total, 4, "the admitted snapshot still serves its own build");
    assert!(!service.check_graphify_artifact(&old.artifact_id).unwrap().current, "a replacement is reported, not mixed in");
    let new = service.open_graphify_artifact(&folder).unwrap();
    assert_ne!(new.artifact_id, old.artifact_id);
    assert_eq!(new.counts.nodes, 5);
    assert_eq!(service.read_graphify_graph(&new.artifact_id, GraphifySection::Nodes, 0, 10).unwrap().total, 5);
    // Vanished output.
    fs::remove_dir_all(repo.join("graphify-out")).unwrap();
    assert_eq!(service.read_graphify_graph(&new.artifact_id, GraphifySection::Nodes, 0, 10).unwrap().total, 5, "an admitted snapshot is immutable even when the live output vanishes");
    assert!(service.open_graphify_artifact(&folder).is_err());

    let reject = |graph: Value, needle: &str| {
        write_graph(&repo, &graph, Some(&folder));
        let error = service.open_graphify_artifact(&folder).unwrap_err();
        assert!(error.contains(needle), "{needle}: {error}");
    };
    reject(json!({"nodes": [{"id": "a"}, {"id": "a"}], "links": []}), "Duplicate node id");
    reject(json!({"nodes": [{"label": "no id"}], "links": []}), "no id");
    reject(json!({"nodes": [{"id": 1.5}], "links": []}), "unusable id");
    reject(json!({"nodes": [{"id": true}], "links": []}), "unusable id");
    reject(json!({"nodes": [{"id": "a"}], "links": [{"source": "a"}]}), "no target");
    reject(json!({"nodes": [{"id": "a"}], "links": [5]}), "not an object");
    reject(json!({"nodes": [{"id": "a"}], "links": [], "edges": []}), "ambiguous");
    reject(json!({"links": []}), "no \"nodes\"");
    reject(json!({"nodes": {"a": 1}, "links": []}), "nodes");
    reject(json!({"nodes": [{"id": "a"}], "links": [], "directed": "yes"}), "true or false");
    reject(json!({"nodes": [{"id": "a", "blob": "x".repeat(600_000)}], "links": []}), "record limit");
    fs::write(repo.join("graphify-out").join("graph.json"), "{\"nodes\": [{\"id\": \"a\"}], \"links\": [] trailing").unwrap();
    assert!(service.open_graphify_artifact(&folder).is_err());
    let leftovers: Vec<_> = fs::read_dir(t.join("config").join("knowledge").join("graphify")).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with(".incoming-")).collect();
    assert!(leftovers.is_empty(), "failed admissions leave nothing behind");
}

#[test]
fn legacy_edges_are_parsed_explicitly_and_the_home_of_the_output_is_registered_by_a_human() {
    let t = Temp::new();
    let service = service(&t);
    let repo = repository(&t);
    let folder = repo.to_string_lossy().into_owned();
    let legacy = json!({"nodes": [{"id": "a"}, {"id": "b"}], "edges": [{"source": "a", "target": "b", "relation": "calls"}]});
    // A graph living outside the repository is not associated automatically, even if it is "the same name".
    let external = t.join("external").join("graphify-out");
    fs::create_dir_all(&external).unwrap();
    fs::write(external.join("graph.json"), serde_json::to_vec(&legacy).unwrap()).unwrap();
    assert_eq!(service.get_knowledge_capabilities(Some(folder.as_str())).unwrap().graphify.state, GraphifyState::Absent);
    assert!(service.register_graphify_output(&folder, &external.to_string_lossy()).is_err(), "a folder without .graphify_root cannot be tied to the project");
    fs::write(external.join(".graphify_root"), "C:/another/project").unwrap();
    assert!(service.register_graphify_output(&folder, &external.to_string_lossy()).is_err(), "a contradictory marker is foreign output");
    assert_eq!(service.get_knowledge_capabilities(Some(folder.as_str())).unwrap().graphify.state, GraphifyState::Absent);
    fs::write(external.join(".graphify_root"), &folder).unwrap();
    let capabilities = service.register_graphify_output(&folder, &t.join("external").to_string_lossy()).unwrap();
    assert_eq!((capabilities.graphify.present, capabilities.graphify.state), (true, GraphifyState::Present));
    let artifact = service.open_graphify_artifact(&folder).unwrap();
    assert_eq!((artifact.schema.as_str(), artifact.counts.links, artifact.output_kind.as_str(), artifact.directed_declared), ("legacyEdges", 1, "registered", false));
    assert!(!artifact.directed, "undeclared directedness is reported as not declared");
    // The registration survives a restart; the human can see it.
    let again = KnowledgeService::with_environment(t.join("config"), t.join("home"), Environment::isolated());
    assert_eq!(again.get_knowledge_capabilities(Some(folder.as_str())).unwrap().graphify.state, GraphifyState::Present);
    // A local output wins once it exists.
    write_graph(&repo, &sample_graph(), Some(&folder));
    assert_eq!(again.open_graphify_artifact(&folder).unwrap().output_kind, "local");
}

#[test]
fn graphify_snapshots_are_never_pruned() {
    let t = Temp::new();
    let service = service(&t);
    let repo = repository(&t);
    let folder = repo.to_string_lossy().into_owned();
    let mut ids = Vec::new();
    for i in 0..6 {
        write_graph(&repo, &json!({"nodes": [{"id": format!("n{i}")}], "links": []}), Some(&folder));
        ids.push(service.open_graphify_artifact(&folder).unwrap().artifact_id);
    }
    for id in &ids {
        assert_eq!(service.read_graphify_graph(id, GraphifySection::Nodes, 0, 10).unwrap().total, 1, "every admitted snapshot stays readable");
    }
}

#[test]
fn a_retained_root_capability_dies_when_the_vault_is_rebound() {
    let t = Temp::new();
    let service = service(&t);
    let first = bind_global(&service, &t.join("one"));
    let stale = service.vault_root(&first.vault_id).unwrap();
    stale.write_markdown("a.md", "# A\n", WritePrecondition::Create).unwrap();
    let second = bind_global(&service, &t.join("two"));
    assert_eq!(second.vault_id, first.vault_id);
    assert!(stale.write_markdown("b.md", "x", WritePrecondition::Create).unwrap_err().contains("rebound"));
    assert!(stale.read_markdown("a.md").is_err());
    assert!(!t.join("one").join("b.md").exists() && !t.join("two").join("b.md").exists(), "a stale capability writes nowhere");
    service.vault_root(&second.vault_id).unwrap().write_markdown("b.md", "x", WritePrecondition::Create).unwrap();
    assert!(t.join("two").join("b.md").is_file());
}

#[test]
fn a_replaced_vault_root_directory_is_refused() {
    let t = Temp::new();
    let service = service(&t);
    let vault = bind_global(&service, &t.join("v"));
    let root = service.vault_root(&vault.vault_id).unwrap();
    fs::remove_dir(t.join("v")).unwrap();
    fs::create_dir(t.join("v")).unwrap();
    let error = root.write_markdown("late.md", "x", WritePrecondition::Create).unwrap_err();
    assert!(error.contains("moved or replaced") || error.contains("Cannot"), "{error}");
    assert!(!t.join("v").join("late.md").exists());
}

#[test]
fn replacements_with_the_same_expected_hash_cannot_both_win() {
    let t = Temp::new();
    let service = service(&t);
    let vault = bind_global(&service, &t.join("v"));
    let root = service.vault_root(&vault.vault_id).unwrap();
    let base = root.write_markdown("n.md", "# base\n", WritePrecondition::Create).unwrap();
    let wins: usize = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|i| {
                let (root, hash) = (root.clone(), base.hash.clone());
                scope.spawn(move || root.write_markdown("n.md", &format!("# writer {i}\n"), WritePrecondition::Replace { expected_hash: hash }).is_ok())
            })
            .collect();
        handles.into_iter().map(|h| usize::from(h.join().unwrap())).sum()
    });
    assert_eq!(wins, 1, "exactly one writer may replace the version that all of them read");
}

#[cfg(windows)]
#[test]
fn an_external_writer_holding_the_note_open_conflicts_instead_of_being_overwritten() {
    use std::os::windows::fs::OpenOptionsExt;
    let t = Temp::new();
    let service = service(&t);
    let vault = bind_global(&service, &t.join("v"));
    let root = service.vault_root(&vault.vault_id).unwrap();
    let base = root.write_markdown("n.md", "# base\n", WritePrecondition::Create).unwrap();
    // An external editor that has the note open for writing (full sharing, as Obsidian/editors do).
    let external = fs::OpenOptions::new().write(true).share_mode(7).open(t.join("v").join("n.md")).unwrap();
    let result = root.write_markdown("n.md", "# ours\n", WritePrecondition::Replace { expected_hash: base.hash.clone() });
    drop(external);
    assert!(result.is_err(), "the open external writer must produce a conflict");
    assert_eq!(fs::read_to_string(t.join("v").join("n.md")).unwrap(), "# base\n");
    root.write_markdown("n.md", "# ours\n", WritePrecondition::Replace { expected_hash: base.hash }).unwrap();
}

#[test]
fn a_document_page_always_carries_the_hash_of_the_bytes_it_returns() {
    let t = Temp::new();
    let service = service(&t);
    let vault = bind_global(&service, &t.join("v"));
    write_notes(&t.join("v"), &[("d.md", &"word\n".repeat(30_000))]);
    let page = service.read_vault_document(&vault.vault_id, "d.md", 0, None).unwrap();
    let bytes = fs::read(t.join("v").join("d.md")).unwrap();
    assert_eq!(page.hash, sha256_hex(&bytes));
    assert_eq!(page.text.as_bytes(), &bytes[..page.length as usize]);
}

#[cfg(windows)]
mod publication_race {
    use super::super::note_publication::{HookAction, Stage, HOOKS};
    use super::*;
    use parking_lot::Mutex;
    use std::sync::Arc;

    fn names(dir: &Path) -> Vec<String> {
        let mut found: Vec<String> = fs::read_dir(dir).map(|e| e.map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
        found.sort();
        found
    }

    struct Fixture {
        _t: Temp,
        root: PathBuf,
        journal: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let t = Temp::new();
            let root = t.0.join("native/agent");
            fs::create_dir_all(&root).unwrap();
            fs::write(root.join("n.md"), "base").unwrap();
            let journal = t.0.join("journal");
            Self { _t: t, root, journal }
        }
        fn write(&self, text: &str) -> Result<()> {
            fsio::write_relative_markdown_journaled(&self.root, "n.md", text, Some(&sha256_hex(b"base")), &self.journal)
        }
        fn hook(&self, hook: impl Fn(Stage, &Path, &str) -> HookAction + Send + 'static) {
            HOOKS.lock().insert(fsio::norm(&self.root), Box::new(hook));
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            HOOKS.lock().remove(&fsio::norm(&self.root));
        }
    }

    #[test]
    fn a_root_spelled_with_mixed_separators_publishes_over_an_existing_note() {
        let f = Fixture::new();
        f.write("original").unwrap();
        assert_eq!(fs::read_to_string(f.root.join("n.md")).unwrap(), "original");
        fsio::write_relative_markdown_journaled(&f.root, "NEW.md", "created", None, &f.journal).unwrap();
        assert_eq!(fs::read_to_string(f.root.join("NEW.md")).unwrap(), "created");
        assert!(fsio::write_relative_markdown_journaled(&f.root, "NEW.md", "again", None, &f.journal).is_err(), "create-only never replaces");
        fsio::remove_relative_checked(&f.root, "NEW.md", &sha256_hex(b"created")).unwrap();
        assert_eq!(names(&f.root), vec!["n.md".to_string()], "no staged or reserved file is left behind");
        assert!(!fsio::publications_need_recovery(&f.journal).unwrap());
        assert!(names(&f.journal).is_empty(), "a successful replacement keeps no journal and no backup");
    }

    #[test]
    fn a_foreign_process_cannot_write_rename_or_delete_the_note_between_the_hash_check_and_publication() {
        let f = Fixture::new();
        let outcomes = Arc::new(Mutex::new(Vec::new()));
        let seen = outcomes.clone();
        f.hook(move |stage, parent, name| {
            if stage == Stage::Locked {
                let path = parent.join(name);
                let mut seen = seen.lock();
                seen.push(fs::OpenOptions::new().write(true).open(&path).is_ok());
                seen.push(fs::rename(&path, parent.join("moved.md")).is_ok());
                seen.push(fs::remove_file(&path).is_ok());
                seen.push(fs::write(&path, "foreign edit").is_ok());
            }
            HookAction::Continue
        });
        f.write("ours").unwrap();
        assert_eq!(*outcomes.lock(), vec![false, false, false, false], "every external mutation is refused while the note is locked");
        assert_eq!(fs::read_to_string(f.root.join("n.md")).unwrap(), "ours");
        assert_eq!(names(&f.root), vec!["n.md".to_string()]);
    }

    #[test]
    fn a_note_created_in_the_empty_name_gap_is_kept_and_the_original_is_backed_up_privately() {
        let f = Fixture::new();
        f.hook(|stage, parent, name| {
            if stage == Stage::Reserved {
                fs::write(parent.join(name), "foreign").unwrap();
            }
            HookAction::Continue
        });
        let error = f.write("ours").unwrap_err();
        assert!(error.contains("replaced by someone else"), "{error}");
        assert_eq!(fs::read_to_string(f.root.join("n.md")).unwrap(), "foreign", "the foreign note is never overwritten");
        assert_eq!(names(&f.root), vec!["n.md".to_string()], "nothing app-owned is left in the vault");
        let backups: Vec<String> = names(&f.journal).into_iter().filter(|n| n.ends_with(".base.md")).collect();
        assert_eq!(backups.len(), 1, "{:?}", names(&f.journal));
        assert_eq!(fs::read_to_string(f.journal.join(&backups[0])).unwrap(), "base", "the original bytes are preserved outside the vault");
        assert!(error.contains(&backups[0]), "the error names the backup: {error}");
        assert!(!fsio::publications_need_recovery(&f.journal).unwrap(), "a conflicted record is terminal");
        fsio::recover_publications(&f.journal).unwrap();
        assert_eq!(fs::read_to_string(f.root.join("n.md")).unwrap(), "foreign");
        assert!(f.journal.join(&backups[0]).is_file(), "recovery never deletes the preserved original");
    }

    #[test]
    fn a_crash_in_the_gap_is_rolled_back_by_recovery_without_losing_the_original() {
        let f = Fixture::new();
        f.hook(|stage, _, _| if stage == Stage::Reserved { HookAction::Crash } else { HookAction::Continue });
        assert!(f.write("ours").unwrap_err().contains("simulated crash"));
        assert!(!f.root.join("n.md").exists(), "the name is empty in the gap");
        assert!(names(&f.root).iter().any(|n| n.starts_with(".omp-reserved-")), "the original sits in its reservation: {:?}", names(&f.root));
        assert!(fsio::publications_need_recovery(&f.journal).unwrap());
        fsio::recover_publications(&f.journal).unwrap();
        assert_eq!(fs::read_to_string(f.root.join("n.md")).unwrap(), "base", "the original is restored");
        assert_eq!(names(&f.root), vec!["n.md".to_string()], "reservation and staged file are cleaned up");
        assert!(!fsio::publications_need_recovery(&f.journal).unwrap());
        fsio::recover_publications(&f.journal).unwrap(); // repeatable no-op
        assert_eq!(fs::read_to_string(f.root.join("n.md")).unwrap(), "base");
    }

    #[test]
    fn recovery_never_overwrites_or_undoes_a_note_someone_wrote_after_the_crash() {
        let f = Fixture::new();
        f.hook(|stage, _, _| if stage == Stage::Reserved { HookAction::Crash } else { HookAction::Continue });
        assert!(f.write("ours").is_err());
        fs::write(f.root.join("n.md"), "external edit made while the app was down").unwrap();
        fsio::recover_publications(&f.journal).unwrap();
        assert_eq!(fs::read_to_string(f.root.join("n.md")).unwrap(), "external edit made while the app was down");
        assert_eq!(names(&f.root), vec!["n.md".to_string()]);
        let backups: Vec<String> = names(&f.journal).into_iter().filter(|n| n.ends_with(".base.md")).collect();
        assert_eq!(backups.len(), 1, "the original stays recoverable: {:?}", names(&f.journal));
        assert_eq!(fs::read_to_string(f.journal.join(&backups[0])).unwrap(), "base");
        assert!(!fsio::publications_need_recovery(&f.journal).unwrap());
    }

    #[test]
    fn a_live_writer_is_never_recovered_and_one_of_several_same_hash_writers_wins() {
        let f = Fixture::new();
        let wins: usize = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..6)
                .map(|i| {
                    let (root, journal, hash) = (f.root.clone(), f.journal.clone(), sha256_hex(b"base"));
                    scope.spawn(move || fsio::write_relative_markdown_journaled(&root, "n.md", &format!("writer {i}"), Some(&hash), &journal).is_ok())
                })
                .collect();
            handles.into_iter().map(|h| usize::from(h.join().unwrap())).sum()
        });
        assert_eq!(wins, 1);
        assert!(fs::read_to_string(f.root.join("n.md")).unwrap().starts_with("writer "));
        assert_eq!(names(&f.root), vec!["n.md".to_string()]);
    }
}
