use std::{collections::{BTreeMap, BTreeSet}, fs, io::Write, path::Path, sync::Mutex};
use crate::derived_standard::MemoryEntry;
use serde::{Deserialize, Serialize};
pub const LANGUAGES: [&str; 4] = ["npm", "pypi", "maven", "cargo"];
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KnownStandard { pub items: BTreeMap<String, String>, pub counts: BTreeMap<String, usize>, pub derived_counts: BTreeMap<String, usize> }
#[derive(Default, Deserialize, Serialize)]
struct File { items: BTreeMap<String, Item> }
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Item { reason: String, added_at: u64 }
fn load(path: &Path, language: &str) -> File {
    let mut file: File = fs::metadata(path).ok().filter(|m| m.len() <= 5 * 1024 * 1024).and_then(|_| fs::read(path).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let prefix = format!("{language}:");
    file.items.retain(|id, _| id.starts_with(&prefix));
    file
}
fn save(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap();
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| { let mut f = fs::OpenOptions::new().write(true).create_new(true).open(&temp)?; f.write_all(bytes)?; f.sync_all()?; atomic_replace(&temp, path) })();
    let _ = fs::remove_file(temp);
    result
}
/// Held by `learn` and `clear` so a scan that races a clear can never resurrect derived facts.
static LOCK: Mutex<()> = Mutex::new(());
const MAX_DERIVED_IDS: usize = 20_000;
const MAX_DERIVED_VERSIONS: usize = 50;
#[derive(Default, Deserialize, Serialize)]
struct DerivedFile { items: BTreeMap<String, DerivedItem> }
#[derive(Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct DerivedItem {
    #[serde(default)] versions: BTreeSet<String>,
    #[serde(default)] unversioned: bool,
    #[serde(default)] added_at: u64,
}
impl DerivedItem {
    fn facts(&self) -> usize { self.versions.len() + usize::from(self.unversioned) }
}
fn derived_path(root: &Path, language: &str) -> std::path::PathBuf { root.join("architect/known-standard/derived").join(format!("{language}.json")) }
fn load_derived(root: &Path, language: &str) -> DerivedFile {
    let path = derived_path(root, language);
    let mut file: DerivedFile = fs::metadata(&path).ok().filter(|m| m.len() <= 5 * 1024 * 1024).and_then(|_| fs::read(&path).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let prefix = format!("{language}:");
    file.items.retain(|id, _| id.starts_with(&prefix));
    while file.items.len() > MAX_DERIVED_IDS { file.items.pop_last(); }
    for item in file.items.values_mut() { while item.versions.len() > MAX_DERIVED_VERSIONS { item.versions.pop_last(); } }
    file
}
/// The remembered versions of transitive packages of built-in packages, across all languages.
pub fn read_derived(root: &Path) -> BTreeMap<String, MemoryEntry> {
    let mut memory = BTreeMap::new();
    for language in LANGUAGES {
        for (id, item) in load_derived(root, language).items { memory.insert(id, MemoryEntry { versions: item.versions, unversioned: item.unversioned }); }
    }
    memory
}
/// Unions `entries` (the closure below `roots`) into the per-language files and returns the number of new facts.
/// Learns nothing when a root is no longer a remembered standard package (cleared while the scan ran).
pub fn learn(root: &Path, roots: &BTreeSet<String>, entries: &BTreeMap<String, MemoryEntry>) -> Result<usize, String> {
    let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let known = read(root).items;
    if roots.iter().any(|id| !known.contains_key(id)) { return Ok(0); }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    let mut learned = 0;
    for language in LANGUAGES {
        let prefix = format!("{language}:");
        let mut file = load_derived(root, language);
        let mut added = 0;
        for (id, entry) in entries.iter().filter(|(id, _)| id.starts_with(&prefix)) {
            if !file.items.contains_key(id) {
                if file.items.len() >= MAX_DERIVED_IDS || (entry.versions.is_empty() && !entry.unversioned) { continue; }
                file.items.insert(id.clone(), DerivedItem { added_at: now, ..DerivedItem::default() });
            }
            let item = file.items.get_mut(id).expect("inserted above");
            for version in &entry.versions {
                if item.versions.len() < MAX_DERIVED_VERSIONS && item.versions.insert(version.clone()) { added += 1; }
            }
            if entry.unversioned && !item.unversioned { item.unversioned = true; added += 1; }
        }
        if added > 0 {
            save(&derived_path(root, language), &serde_json::to_vec(&file).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
            learned += added;
        }
    }
    Ok(learned)
}
#[cfg(not(windows))]
fn atomic_replace(from: &Path, to: &Path) -> std::io::Result<()> { fs::rename(from, to) }
#[cfg(windows)]
fn atomic_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    extern "system" { fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32; }
    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0x1 | 0x8) } == 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}
fn import(root: &Path) -> std::io::Result<()> {
    let dir = root.join("architect/known-standard");
    let mut files: BTreeMap<&str, File> = LANGUAGES.iter().map(|l| (*l, load(&dir.join(format!("{l}.json")), l))).collect();
    let mut changed = std::collections::BTreeSet::new();
    let source = root.join("architect/classifications");
    match fs::read_dir(source) {
        Ok(entries) => for entry in entries {
            let entry = entry?;
            if entry.path().extension().is_none_or(|e| e != "json") { continue; }
            let metadata = entry.metadata()?;
            if !metadata.is_file() || metadata.len() > 5 * 1024 * 1024 { continue; }
            let bytes = fs::read(entry.path())?;
            let Ok(classification) = serde_json::from_slice::<crate::architect::Classification>(&bytes) else { continue; };
            for (id, item) in classification.items {
                if !item.basic { continue; }
                let Some((language, _)) = id.split_once(':') else { continue; };
                if let Some(file) = files.get_mut(language) {
                    if !file.items.contains_key(&id) {
                        changed.insert(language.to_string());
                        file.items.insert(id, Item { reason: item.reason, added_at: classification.updated_at.unwrap_or(0) });
                    }
                }
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
        Err(e) => return Err(e),
    }
    for (language, file) in files { if changed.contains(language) { save(&dir.join(format!("{language}.json")), &serde_json::to_vec(&file)?)?; } }
    save(&dir.join(".imported"), b"1")
}
pub fn read(root: &Path) -> KnownStandard {
    let dir = root.join("architect/known-standard");
    if !dir.join(".imported").exists() { let _ = import(root); }
    let mut result = KnownStandard { items: BTreeMap::new(), counts: BTreeMap::new(), derived_counts: BTreeMap::new() };
    for language in LANGUAGES {
        let file = load(&dir.join(format!("{language}.json")), language);
        result.counts.insert(language.into(), file.items.len());
        result.derived_counts.insert(language.into(), load_derived(root, language).items.values().map(DerivedItem::facts).sum());
        result.items.extend(file.items.into_iter().map(|(id,item)| (id,item.reason)));
    }
    result
}
pub fn clear(root: &Path, language: Option<&str>) -> Result<usize, String> {
    if language.is_some_and(|l| !LANGUAGES.contains(&l)) { return Err("Unknown language".into()); }
    let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let previous = read(root);
    let dir = root.join("architect/known-standard");
    // Do not clear unless the import marker is durable: otherwise a later import could resurrect entries.
    if !dir.join(".imported").exists() { import(root).map_err(|e| e.to_string())?; }
    let mut removed = 0;
    for l in LANGUAGES {
        if language.is_none_or(|selected| selected == l) {
            save(&dir.join(format!("{l}.json")), b"{\"items\":{}}").map_err(|e| e.to_string())?;
            save(&derived_path(root, l), b"{\"items\":{}}").map_err(|e| e.to_string())?;
            removed += previous.counts[l] + previous.derived_counts[l];
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn temp() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("omp-standard-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("architect/classifications")).unwrap();
        root
    }
    fn classification(root: &Path) {
        fs::write(root.join("architect/classifications/a.json"), br#"{"items":{"npm:a":{"basic":true,"reason":"core"},"pypi:b":{"basic":true,"reason":"base"},"npm:no":{"basic":false},"java:no":{"basic":true}},"updatedAt":123}"#).unwrap();
    }
    #[test]
    fn imports_once_and_clear_never_resurrects_entries() {
        let root = temp();
        classification(&root);
        let result = read(&root);
        assert_eq!(result.items.len(), 2);
        assert_eq!(result.counts.len(), 4);
        assert_eq!(result.counts["cargo"], 0);
        assert!(root.join("architect/known-standard/.imported").exists());
        assert_eq!(clear(&root, Some("java")).unwrap_err(), "Unknown language");
        assert_eq!(clear(&root, Some("npm")).unwrap(), 1);
        assert_eq!(read(&root).items.len(), 1);
        fs::remove_file(root.join("architect/classifications/a.json")).unwrap();
        assert_eq!(read(&root).items.len(), 1);
        assert_eq!(clear(&root, None).unwrap(), 1);
        assert!(read(&root).items.is_empty());
        fs::remove_dir_all(root).unwrap();
        let root = temp();
        classification(&root);
        assert_eq!(clear(&root, None).unwrap(), 2);
        assert!(read(&root).items.is_empty());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn corrupt_oversized_and_wrong_prefix_entries_are_ignored() {
        let root = temp();
        let dir = root.join("architect/known-standard");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(".imported"), "1").unwrap();
        fs::write(dir.join("npm.json"), "not json").unwrap();
        fs::write(dir.join("pypi.json"), vec![b' '; 5 * 1024 * 1024 + 1]).unwrap();
        fs::write(dir.join("cargo.json"), br#"{"items":{"npm:wrong":{"reason":"x","addedAt":1},"cargo:right":{"reason":"yes","addedAt":2}}}"#).unwrap();
        let result = read(&root);
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items["cargo:right"], "yes");
        assert_eq!(result.counts.len(), 4);
        fs::remove_dir_all(root).unwrap();
    }
    fn entry(versions: &[&str], unversioned: bool) -> MemoryEntry {
        MemoryEntry { versions: versions.iter().map(|v| v.to_string()).collect(), unversioned }
    }
    fn roots(ids: &[&str]) -> BTreeSet<String> { ids.iter().map(|id| id.to_string()).collect() }
    #[test]
    fn learn_unions_facts_counts_them_and_clear_forgets_them() {
        let root = temp();
        classification(&root);
        let entries = BTreeMap::from([
            ("npm:p".to_string(), entry(&["1", "2"], false)),
            ("npm:q".to_string(), entry(&[], true)),
            ("pypi:x".to_string(), entry(&["3"], false)),
            ("java:z".to_string(), entry(&["1"], false)),
            ("npm:empty".to_string(), entry(&[], false)),
        ]);
        assert_eq!(learn(&root, &roots(&["npm:a"]), &entries).unwrap(), 4);
        assert_eq!(learn(&root, &roots(&["npm:a"]), &entries).unwrap(), 0);
        let memory = read_derived(&root);
        assert_eq!(memory.keys().collect::<Vec<_>>(), ["npm:p", "npm:q", "pypi:x"]);
        assert_eq!(memory["npm:p"], entry(&["1", "2"], false));
        assert_eq!(memory["npm:q"], entry(&[], true));
        let known = read(&root);
        assert_eq!(known.derived_counts.len(), 4);
        assert_eq!((known.derived_counts["npm"], known.derived_counts["pypi"], known.derived_counts["maven"], known.derived_counts["cargo"]), (3, 1, 0, 0));
        let more = BTreeMap::from([("npm:p".to_string(), entry(&["2", "3"], false))]);
        assert_eq!(learn(&root, &roots(&["npm:a", "pypi:b"]), &more).unwrap(), 1);
        assert_eq!(read_derived(&root)["npm:p"], entry(&["1", "2", "3"], false));
        assert_eq!(clear(&root, Some("npm")).unwrap(), 1 + 4);
        let memory = read_derived(&root);
        assert_eq!(memory.keys().collect::<Vec<_>>(), ["pypi:x"]);
        assert_eq!(fs::read_to_string(root.join("architect/known-standard/derived/npm.json")).unwrap(), "{\"items\":{}}");
        assert_eq!(read(&root).derived_counts["npm"], 0);
        // The cleared root is gone: a scan that raced the clear learns nothing.
        assert_eq!(learn(&root, &roots(&["npm:a"]), &more).unwrap(), 0);
        assert!(!read_derived(&root).contains_key("npm:p"));
        assert_eq!(clear(&root, None).unwrap(), 1 + 1);
        assert!(read_derived(&root).is_empty());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn derived_memory_honours_the_id_and_version_caps() {
        let root = temp();
        classification(&root);
        let many: Vec<String> = (0..60).map(|n| format!("{n:03}")).collect();
        let versions: Vec<&str> = many.iter().map(String::as_str).collect();
        let entries = BTreeMap::from([("npm:wide".to_string(), entry(&versions, false))]);
        assert_eq!(learn(&root, &BTreeSet::new(), &entries).unwrap(), 50);
        assert_eq!(read_derived(&root)["npm:wide"].versions.len(), 50);
        let ids: BTreeMap<String, MemoryEntry> = (0..20_001).map(|n| (format!("cargo:c{n:05}"), entry(&["1"], false))).collect();
        assert_eq!(learn(&root, &BTreeSet::new(), &ids).unwrap(), 20_000);
        assert_eq!(read_derived(&root).keys().filter(|id| id.starts_with("cargo:")).count(), 20_000);
        fs::remove_dir_all(root).unwrap();
    }
}
