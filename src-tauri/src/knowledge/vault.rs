//! Vault registration (human-confirmed, token based), safe listing/reading/writing of vault Markdown, and the
//! destination rules that keep vaults away from credentials, application data and each other.
use super::dto::*;
use super::fsio::{self, err, is_markdown_name, sha256_hex, shown, validate_prefix, validate_relative};
use super::markdown::Scanner;
use super::{KnowledgeService, Result, StoredVault};
use crate::code_quality::snapshot::is_link;
use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

const PAGE_BYTES: usize = 64 * 1024;
const MAX_LIMIT: usize = 200;
const TOKEN_TTL: Duration = Duration::from_secs(10 * 60);
const MAX_PENDING: usize = 32;
/// Folders OMP Manager creates inside a vault; anything already there is preserved.
const MANAGED_FOLDERS: [&str; 2] = ["Agent setup", "SRS"];
/// A destination may not be (or sit inside) a folder with one of these names: credentials, tool caches, VCS internals.
const DENIED_COMPONENTS: [&str; 11] = [".omp", ".pi", ".ssh", ".gnupg", ".aws", ".azure", ".kube", ".docker", ".git", "node_modules", ".cargo"];

pub(crate) struct Pending {
    scope: KnowledgeScope,
    mode: VaultMode,
    root: PathBuf,
    exists: bool,
    identity: Option<String>,
    conflicts: Vec<String>,
    expires: Instant,
}

struct Destination {
    root: PathBuf,
    exists: bool,
    identity: Option<String>,
}

struct VaultEntryRow {
    name: String,
    kind: VaultEntryKind,
    bytes: u64,
    mtime: u128,
}

/// A validated vault capability. It carries the binding revision and root identity it was issued for; every
/// operation re-validates them (see [`VaultRoot::pin`]). Obtain it only from `KnowledgeService::vault_root`.
#[derive(Clone, Debug)]
pub struct VaultRoot {
    pub vault_id: String,
    pub root: PathBuf,
    pub mode: VaultMode,
    pub scope: KnowledgeScope,
    pub binding_revision: u64,
    identity: Option<String>,
    authority: std::sync::Arc<parking_lot::RwLock<u64>>,
    /// Private application folder (outside every vault) for publication journals and original-note backups.
    journal_dir: PathBuf,
}

#[derive(Clone, Debug)]
pub enum WritePrecondition {
    /// The note must not exist yet.
    Create,
    /// The note must still have this SHA-256 (it was read, then changed by someone else otherwise).
    Replace { expected_hash: String },
}

#[derive(Clone, Debug)]
pub struct WriteReceipt {
    pub path: String,
    pub hash: String,
    pub bytes: u64,
}

pub(crate) fn valid_id(id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok_and(|u| u.to_string() == id)
}

pub(crate) fn scope_key(scope: &KnowledgeScope) -> String {
    match scope {
        KnowledgeScope::Global => "global".into(),
        KnowledgeScope::Project { repository_root } => format!("project:{}", fsio::norm(Path::new(repository_root))),
    }
}

fn scope_label(scope: &KnowledgeScope) -> String {
    match scope {
        KnowledgeScope::Global => "the global vault".into(),
        KnowledgeScope::Project { repository_root } => format!("the project vault of {repository_root}"),
    }
}

impl VaultRoot {
    pub(crate) fn pin(&self) -> Result<(parking_lot::RwLockReadGuard<'_, u64>, fsio::DirectoryPins)> {
        self.pin_with(false)
    }

    /// Pins the admitted root and every ancestor and holds the binding authority (a rebind waits for it).
    /// Fails when this capability is stale: another binding revision, or the root was replaced or moved.
    pub(crate) fn pin_with(&self, writable_root: bool) -> Result<(parking_lot::RwLockReadGuard<'_, u64>, fsio::DirectoryPins)> {
        let guard = self.authority.read_recursive();
        if *guard != self.binding_revision {
            return Err("This vault was rebound since the capability was issued; open the vault again".into());
        }
        let pins = fsio::pin_directory_leaf(&self.root, writable_root)?;
        if let Some(expected) = &self.identity {
            if pins.identity().ok().as_ref() != Some(expected) {
                return Err(format!("The vault folder was moved or replaced: {}", self.root.display()));
            }
        }
        Ok((guard, pins))
    }

    /// The bytes of one vault Markdown note (handle-checked, single-link, `.md` only). No size cap.
    pub fn read_markdown(&self, relative: &str) -> Result<Vec<u8>> {
        let components = validate_relative(relative)?;
        if !components.last().is_some_and(|name| is_markdown_name(name)) {
            return Err("Not a Markdown vault document".into());
        }
        let (_authority, _pins) = self.pin()?;
        let (mut file, _) = fsio::open_components(&self.root, &components)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(|e| err("Cannot read note", e))?;
        Ok(bytes)
    }

    /// Publishes one Markdown note. Writes are serialized per note; a replacement's expected hash is verified under
    /// an operating-system sharing lock held through the atomic rename, so an external edit conflicts instead of
    /// being overwritten. Refuses links, hard-linked targets and case collisions; only `.md`.
    pub fn write_markdown(&self, relative: &str, text: &str, precondition: WritePrecondition) -> Result<WriteReceipt> {
        let components = validate_relative(relative)?;
        let (_authority, _pins) = self.pin_with(components.len() == 1)?;
        let expected = match &precondition {
            WritePrecondition::Create => None,
            WritePrecondition::Replace { expected_hash } => Some(expected_hash.as_str()),
        };
        if fsio::publications_need_recovery(&self.journal_dir)? { fsio::recover_publications(&self.journal_dir)?; }
        fsio::write_relative_markdown_journaled(&self.root, relative, text, expected, &self.journal_dir)?;
        Ok(WriteReceipt { path: components.join("/"), hash: sha256_hex(text.as_bytes()), bytes: text.len() as u64 })
    }
}

fn count_markdown(root: &Path, depth: usize) -> u64 {
    let Ok(entries) = fs::read_dir(root) else { return 0 };
    let mut count = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with('.') {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(entry.path()) else { continue };
        if is_link(&meta) {
            continue;
        }
        if meta.is_dir() && depth < 64 {
            count += count_markdown(&entry.path(), depth + 1);
        } else if meta.is_file() && is_markdown_name(name) {
            count += 1;
        }
    }
    count
}

fn resolve_destination_root(requested: &Path) -> Result<(PathBuf, bool)> {
    let mut missing = Vec::new();
    let mut cursor = requested;
    let existing_meta = loop {
        match fs::symlink_metadata(cursor) {
            Ok(meta) => break meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let name = cursor.file_name().ok_or("The vault folder has no usable name")?;
                missing.push(name);
                cursor = cursor.parent().ok_or("The vault folder has no parent")?;
            }
            Err(e) => return Err(err("Cannot inspect the vault folder", e)),
        }
    };
    let exists = missing.is_empty();
    if (exists && is_link(&existing_meta)) || existing_meta.file_type().is_symlink() {
        return Err("A vault cannot be (or sit inside) a link, junction or reparse point".into());
    }
    if !existing_meta.is_dir() {
        return Err(format!("{} is not a folder", cursor.display()));
    }
    let mut root = crate::setup::canonical(cursor).map_err(|e| err("Cannot resolve the vault folder", e))?;
    if !fsio::same_path(&root, cursor) {
        return Err("The vault folder resolves through a link or alias; choose its real location".into());
    }
    for name in missing.iter().rev() {
        let name = name.to_str().ok_or("Vault folder names must be Unicode")?;
        validate_relative(name).map_err(|_| format!("Unsafe vault folder name: {}", shown(name)))?;
        root.push(name);
    }
    Ok((root, exists))
}

fn vault_entry_row(entry: fs::DirEntry) -> Option<VaultEntryRow> {
    let name = entry.file_name().into_string().ok()?;
    if name.starts_with('.') || validate_relative(&name).is_err() {
        return None;
    }
    let meta = fs::symlink_metadata(entry.path()).ok()?;
    if is_link(&meta) {
        return None;
    }
    let kind = if meta.is_dir() {
        VaultEntryKind::Folder
    } else if meta.is_file() && is_markdown_name(&name) {
        VaultEntryKind::Markdown
    } else if meta.is_file() {
        VaultEntryKind::Other
    } else {
        return None;
    };
    Some(VaultEntryRow { name, kind, bytes: meta.len(), mtime: fsio::mtime_nanos(&meta) })
}

fn scan_before_offset(reader: &mut fsio::HashedReader, offset: u64) -> Result<(Scanner, bool)> {
    let mut scanner = Scanner::default();
    let mut sink = Vec::new();
    let mut line = Vec::new();
    let mut first = true;
    while let Some(read) = reader.next_line(&mut line, 8 * 1024 * 1024).map_err(|e| err("Cannot read note", e))? {
        if reader.position() > offset {
            return Ok((scanner, read.start < offset));
        }
        let at_start = first;
        first = false;
        if !read.too_long {
            if let Ok(text) = std::str::from_utf8(fsio::trim_line(&line, at_start)) {
                sink.clear();
                scanner.feed(text, &mut sink);
            }
        }
    }
    Ok((Scanner::default(), false))
}

fn protected_equal_or_ancestor(path: &Path, protected: &Path) -> bool {
    fsio::same_or_inside(path, protected)
}

impl KnowledgeService {
    /// Roots a vault may not overlap: application data, native OMP agent folders, other products' stores.
    fn protected_roots(&self) -> Vec<PathBuf> {
        let mut roots = vec![self.app_config.clone(), self.home.join(".omp"), self.home.join(".pi")];
        for variable in ["PI_CODING_AGENT_DIR", "PI_CONFIG_DIR", "PI_NATIVES_DIR"] {
            if let Some(value) = std::env::var_os(variable).filter(|v| !v.is_empty()) {
                roots.push(PathBuf::from(value));
            }
        }
        for variable in ["LOCALAPPDATA", "APPDATA"] {
            if let Some(base) = std::env::var_os(variable).filter(|v| !v.is_empty()).map(PathBuf::from) {
                roots.push(base.join("Cognee"));
                roots.push(base.join("Graphify"));
                roots.push(base.join("obsidian"));
            }
        }
        roots.extend(self.env.protected.clone());
        roots.extend(self.protected.lock().iter().cloned());
        roots
    }

    /// Roots a vault may be inside of, but may never be or contain.
    fn broad_roots(&self) -> Vec<PathBuf> {
        let mut roots = vec![self.home.clone()];
        for variable in ["LOCALAPPDATA", "APPDATA", "USERPROFILE", "ProgramFiles", "ProgramFiles(x86)", "SystemRoot"] {
            if let Some(value) = std::env::var_os(variable).filter(|v| !v.is_empty()) {
                roots.push(PathBuf::from(value));
            }
        }
        roots
    }

    /// Resolves a human-chosen destination to a canonical planned root and rejects unsafe choices outright.
    fn resolve_destination(&self, requested: &Path, repository: Option<&Path>) -> Result<Destination> {
        if !requested.is_absolute() {
            return Err("Choose an absolute folder for the vault".into());
        }
        if requested.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir)) {
            return Err("The vault folder may not contain '.' or '..' segments".into());
        }
        let (root, exists) = resolve_destination_root(requested)?;
        if root.parent().is_none() {
            return Err("A drive or filesystem root cannot be a vault".into());
        }
        self.validate_destination_roots(&root, repository)?;
        for component in root.components() {
            let Component::Normal(part) = component else { continue };
            let Some(part) = part.to_str() else { continue };
            if DENIED_COMPONENTS.iter().any(|denied| part.eq_ignore_ascii_case(denied)) {
                return Err(format!("A vault cannot be inside a {part} folder"));
            }
        }
        let identity = if exists { fsio::dir_identity(&root) } else { None };
        Ok(Destination { root, exists, identity })
    }

    fn validate_destination_roots(&self, root: &Path, repository: Option<&Path>) -> Result<()> {
        for broad in self.broad_roots() {
            if fsio::same_or_inside(root, &broad) {
                return Err(format!("A vault cannot be, or contain, {}", broad.display()));
            }
        }
        if let Some(repository) = repository {
            if fsio::same_or_inside(root, repository) {
                return Err("A project vault cannot be the repository root or a folder containing it; use a subfolder such as Knowledge".into());
            }
        }
        for protected in self.protected_roots() {
            if protected_equal_or_ancestor(root, &protected) || fsio::same_or_inside(&protected, root) {
                return Err(format!("A vault cannot overlap application, agent or credential data: {}", protected.display()));
            }
        }
        Ok(())
    }

    fn binding_conflicts(vaults: &[StoredVault], root: &Path, scope: &KnowledgeScope) -> Vec<String> {
        let key = scope_key(scope);
        let mut conflicts = Vec::new();
        for vault in vaults {
            if scope_key(&vault.scope) == key {
                continue;
            }
            let other = Path::new(&vault.root);
            if fsio::same_or_inside(other, root) || fsio::same_or_inside(root, other) {
                conflicts.push(format!("Overlaps {} ({})", scope_label(&vault.scope), vault.root));
            }
        }
        conflicts
    }

    fn managed_conflicts(root: &Path, notes: &mut Vec<String>) -> Vec<String> {
        let mut conflicts = Vec::new();
        let names: BTreeSet<String> = fs::read_dir(root).map(|entries| entries.flatten().filter_map(|e| e.file_name().into_string().ok()).collect()).unwrap_or_default();
        for managed in MANAGED_FOLDERS {
            for existing in names.iter().filter(|n| n.eq_ignore_ascii_case(managed) && n.as_str() != managed) {
                conflicts.push(format!("'{existing}' differs from OMP Manager's '{managed}' folder only by letter case"));
            }
            match fs::symlink_metadata(root.join(managed)) {
                Ok(meta) if is_link(&meta) => conflicts.push(format!("'{managed}' is a link or reparse point")),
                Ok(meta) if !meta.is_dir() => conflicts.push(format!("'{managed}' exists and is not a folder")),
                Ok(_) => notes.push(format!("'{managed}' already exists; its notes are preserved and OMP Manager only adds its own documents there")),
                Err(_) => {}
            }
        }
        conflicts
    }

    fn vault_scope(&self, scope: ScopeRequest, folder: Option<&str>) -> Result<(KnowledgeScope, Option<PathBuf>)> {
        match scope {
            ScopeRequest::Global => Ok((KnowledgeScope::Global, None)),
            ScopeRequest::Project { repository_root } => {
                let repository = self.repository_root(folder.ok_or("A project vault needs a project folder")?)?;
                if let Some(sent) = repository_root.as_deref().filter(|s| !s.is_empty()) {
                    if !fsio::same_path(Path::new(sent), &repository) {
                        return Err("The repository root does not match the project folder".into());
                    }
                }
                let scope = KnowledgeScope::Project { repository_root: repository.to_string_lossy().into_owned() };
                Ok((scope, Some(repository)))
            }
        }
    }

    /// Previews a vault destination without changing anything. The token is consumed by [`Self::confirm_knowledge_vault`].
    pub fn prepare_knowledge_vault(&self, scope: ScopeRequest, folder: Option<&str>, destination: &str, mode: VaultMode) -> Result<VaultPreparation> {
        if mode == VaultMode::Obsidian && self.probe_obsidian().state != ObsidianState::Installed {
            return Err("Obsidian is not installed; choose Markdown mode or install Obsidian first".into());
        }
        let (scope, repository) = self.vault_scope(scope, folder)?;
        let requested = if destination.trim().is_empty() {
            match &repository {
                Some(repository) => repository.join("Knowledge"),
                None => self.default_global_root(),
            }
        } else {
            PathBuf::from(destination.trim())
        };
        let resolved = self.resolve_destination(&requested, repository.as_deref())?;
        let mut notes = Vec::new();
        let mut conflicts = {
            let state = self.state.lock();
            if let Some(error) = &state.vaults_error {
                return Err(format!("Vault bindings are unreadable: {error}"));
            }
            let conflicts = Self::binding_conflicts(&state.vaults, &resolved.root, &scope);
            if let Some(current) = state.vaults.iter().find(|v| scope_key(&v.scope) == scope_key(&scope)) {
                notes.push(format!("Replaces the current vault binding ({}); notes there are left where they are", current.root));
            }
            conflicts
        };
        let mut existing_markdown_count = 0;
        if resolved.exists {
            conflicts.extend(Self::managed_conflicts(&resolved.root, &mut notes));
            existing_markdown_count = count_markdown(&resolved.root, 0);
            if existing_markdown_count > 0 {
                notes.push("Existing notes are preserved: nothing is moved, rewritten or adopted".into());
            }
        } else {
            notes.push("The folder will be created when you confirm".into());
        }
        if mode == VaultMode::Obsidian {
            notes.push("OMP Manager never edits Obsidian's settings; register the folder in Obsidian's vault manager to open it there".into());
        }
        let token = uuid::Uuid::new_v4().to_string();
        let preparation = VaultPreparation {
            token: token.clone(),
            scope: scope.clone(),
            mode,
            root: resolved.root.to_string_lossy().into_owned(),
            exists: resolved.exists,
            existing_markdown_count,
            conflicts: conflicts.clone(),
            notes,
        };
        let mut state = self.state.lock();
        let now = Instant::now();
        state.pending.retain(|_, p| p.expires > now);
        while state.pending.len() >= MAX_PENDING {
            let Some(oldest) = state.pending.iter().min_by_key(|(_, p)| p.expires).map(|(k, _)| k.clone()) else { break };
            state.pending.remove(&oldest);
        }
        state.pending.insert(token, Pending { scope, mode, root: resolved.root, exists: resolved.exists, identity: resolved.identity, conflicts, expires: now + TOKEN_TTL });
        Ok(preparation)
    }

    fn validate_preparation(&self, pending: &Pending) -> Result<Destination> {
        if pending.expires <= Instant::now() {
            return Err("This preparation expired; prepare the vault again".into());
        }
        if !pending.conflicts.is_empty() {
            return Err(format!("Resolve these conflicts first: {}", pending.conflicts.join("; ")));
        }
        if pending.mode == VaultMode::Obsidian && self.probe_obsidian().state != ObsidianState::Installed {
            return Err("Obsidian is no longer available; choose Markdown mode".into());
        }
        let repository = match &pending.scope {
            KnowledgeScope::Project { repository_root } => Some(PathBuf::from(repository_root)),
            KnowledgeScope::Global => None,
        };
        let resolved = self.resolve_destination(&pending.root, repository.as_deref())?;
        if !fsio::same_path(&resolved.root, &pending.root) || resolved.exists != pending.exists || resolved.identity != pending.identity {
            return Err("The destination changed since it was previewed; prepare the vault again".into());
        }
        Ok(resolved)
    }

    /// Consumes a preparation token, revalidates the filesystem and registers (or re-binds) the vault.
    pub fn confirm_knowledge_vault(&self, token: &str) -> Result<VaultBinding> {
        let pending = {
            let mut state = self.state.lock();
            state.pending.remove(token).ok_or("This preparation expired or was already used; prepare the vault again")?
        };
        let resolved = self.validate_preparation(&pending)?;
        let existing = self.state.lock().vaults.iter().find(|v| scope_key(&v.scope) == scope_key(&pending.scope)).map(|v| (v.vault_id.clone(), v.binding_revision));
        let authority = existing.map(|(id, revision)| self.authority(&id, revision));
        let mut rebind = authority.as_ref().map(|a| a.write());
        let mut state = self.state.lock();
        if let Some(error) = &state.vaults_error {
            return Err(format!("Vault bindings are unreadable: {error}"));
        }
        let overlaps = Self::binding_conflicts(&state.vaults, &resolved.root, &pending.scope);
        if !overlaps.is_empty() {
            return Err(format!("Resolve these conflicts first: {}", overlaps.join("; ")));
        }
        if !resolved.exists {
            fs::create_dir_all(&resolved.root).map_err(|e| err("Cannot create the vault folder", e))?;
            match fs::symlink_metadata(&resolved.root) {
                Ok(meta) if meta.is_dir() && !is_link(&meta) => {}
                _ => return Err("The created vault folder is not a plain folder".into()),
            }
        }
        let identity = fsio::dir_identity(&resolved.root);
        let now = fsio::now_ms();
        let root_text = resolved.root.to_string_lossy().into_owned();
        let mut vaults = state.vaults.clone();
        let key = scope_key(&pending.scope);
        let position = vaults.iter().position(|v| scope_key(&v.scope) == key);
        let (vault_id, replaced) = match position {
            Some(i) => {
                let existing = &mut vaults[i];
                let replaced = !fsio::same_path(Path::new(&existing.root), &resolved.root);
                existing.root = root_text.clone();
                existing.mode = pending.mode;
                existing.identity = identity;
                existing.binding_revision += 1;
                existing.updated_ms = now;
                (existing.vault_id.clone(), replaced)
            }
            None => {
                let vault_id = uuid::Uuid::new_v4().to_string();
                vaults.push(StoredVault { vault_id: vault_id.clone(), root: root_text.clone(), mode: pending.mode, scope: pending.scope.clone(), identity, binding_revision: 1, created_ms: now, updated_ms: now });
                (vault_id, false)
            }
        };
        self.save_vaults(&vaults)?;
        state.vaults = vaults;
        if let (Some(guard), Some(stored)) = (rebind.as_mut(), state.vaults.iter().find(|v| v.vault_id == vault_id)) {
            **guard = stored.binding_revision;
        }
        drop(state);
        if replaced || self.cached_index(&vault_id).is_some() {
            self.drop_index(&vault_id);
        }
        Ok(VaultBinding { vault_id, root: root_text, mode: pending.mode, scope: pending.scope })
    }

    pub fn list_knowledge_vaults(&self) -> Result<Vec<VaultBinding>> {
        let state = self.state.lock();
        if let Some(error) = &state.vaults_error {
            return Err(format!("Vault bindings are unreadable: {error}"));
        }
        Ok(state.vaults.iter().map(StoredVault::binding).collect())
    }

    /// The reachable vault root for `vault_id`, or why it is not (missing, link, replaced). Nothing is created.
    pub fn vault_root(&self, vault_id: &str) -> Result<VaultRoot> {
        if !valid_id(vault_id) {
            return Err("Unknown vault".into());
        }
        let stored = {
            let state = self.state.lock();
            if let Some(error) = &state.vaults_error {
                return Err(format!("Vault bindings are unreadable: {error}"));
            }
            state.vaults.iter().find(|v| v.vault_id == vault_id).cloned().ok_or("Unknown vault")?
        };
        let root = PathBuf::from(&stored.root);
        match fs::symlink_metadata(&root) {
            Ok(meta) if is_link(&meta) => return Err(format!("The vault folder is now a link: {}", stored.root)),
            Ok(meta) if !meta.is_dir() => return Err(format!("The vault path is not a folder: {}", stored.root)),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(format!("The vault folder is missing: {}. Locate it again or restore it.", stored.root)),
            Err(e) => return Err(err("Cannot inspect the vault folder", e)),
        }
        if let Some(expected) = &stored.identity {
            if fsio::dir_identity(&root).as_ref() != Some(expected) {
                return Err(format!("The vault folder was moved or replaced: {}. Locate it again.", stored.root));
            }
        }
        let authority = self.authority(&stored.vault_id, stored.binding_revision);
        if *authority.read() != stored.binding_revision {
            return Err("The vault binding is changing; try again".into());
        }
        Ok(VaultRoot { vault_id: stored.vault_id, root, mode: stored.mode, scope: stored.scope, binding_revision: stored.binding_revision, identity: stored.identity, authority, journal_dir: self.dir.join("publications") })
    }

    /// The binding authority of a vault: readers (every root operation) hold it shared; a rebind holds it exclusively.
    pub(crate) fn authority(&self, vault_id: &str, revision: u64) -> std::sync::Arc<parking_lot::RwLock<u64>> {
        self.authorities.lock().entry(vault_id.to_string()).or_insert_with(|| std::sync::Arc::new(parking_lot::RwLock::new(revision))).clone()
    }

    /// One folder level, folders first, then case-insensitive names. Hidden (dot) entries and links are not listed.
    pub fn list_vault_entries(&self, vault_id: &str, prefix: &str, offset: usize, limit: usize) -> Result<VaultEntryPage> {
        if !(1..=MAX_LIMIT).contains(&limit) {
            return Err(format!("Listing limit must be between 1 and {MAX_LIMIT}"));
        }
        if offset > 100_000_000 {
            return Err("Listing offset is out of range".into());
        }
        let vault = self.vault_root(vault_id)?;
        let (_authority, _root_pins) = vault.pin()?;
        let components = validate_prefix(prefix)?;
        let pinned = fsio::pin_descendants(&vault.root, &components, false, false)?;
        let dir = pinned.path();
        let normalized = components.join("/");
        let mut rows = Vec::new();
        for entry in fs::read_dir(dir).map_err(|e| err("Cannot list folder", e))? {
            let entry = entry.map_err(|e| err("Cannot list folder", e))?;
            if let Some(row) = vault_entry_row(entry) {
                rows.push(row);
            }
        }
        rows.sort_by(|a, b| ((a.kind != VaultEntryKind::Folder), a.name.to_lowercase(), &a.name).cmp(&((b.kind != VaultEntryKind::Folder), b.name.to_lowercase(), &b.name)));
        let total = rows.len();
        let start = offset.min(total);
        let end = (start + limit).min(total);
        let mut items = Vec::with_capacity(end - start);
        for row in &rows[start..end] {
            let path = if normalized.is_empty() { row.name.clone() } else { format!("{normalized}/{}", row.name) };
            let hash = if row.kind == VaultEntryKind::Markdown {
                self.indexed_note(&vault.vault_id, &path, row.bytes, row.mtime).or_else(|| {
                    let (file, _) = fsio::open_relative(&vault.root, &path).ok()?;
                    fsio::hash_file(file).ok().filter(|d| d.utf8_ok).map(|d| d.hash)
                })
            } else {
                None
            };
            items.push(VaultEntry { path, name: row.name.clone(), kind: row.kind, bytes: (row.kind != VaultEntryKind::Folder).then_some(row.bytes), hash });
        }
        Ok(VaultEntryPage {
            vault_id: vault.vault_id.clone(),
            revision: self.current_revision(&vault.vault_id),
            prefix: normalized,
            total: total as u64,
            offset: offset as u64,
            items,
            next_offset: (end < total).then_some(end as u64),
        })
    }

    /// One page (at most 64 KiB, cut on a UTF-8 boundary and preferably after a newline) of a Markdown note.
    /// `hash`/`byte_length` describe the whole file; `expected_hash` makes a changed note an error instead of a mix.
    pub fn read_vault_document(&self, vault_id: &str, path: &str, offset: u64, expected_hash: Option<&str>) -> Result<VaultDocumentPage> {
        let vault = self.vault_root(vault_id)?;
        let (_authority, _pins) = vault.pin()?;
        let components = validate_relative(path)?;
        if !components.last().is_some_and(|name| is_markdown_name(name)) {
            return Err("Not a Markdown vault document".into());
        }
        let (file, _) = fsio::open_components(&vault.root, &components)?;
        let length = file.metadata().map_err(|e| err("Cannot inspect note", e))?.len();
        if offset > length || (offset == length && length > 0) {
            return Err("The page offset is past the end of the document".into());
        }
        let mut reader = fsio::HashedReader::new(file);
        reader.capture(offset, PAGE_BYTES + 3);
        let (scanner, starts_mid_line) = scan_before_offset(&mut reader, offset)?;
        let (mut digest, file) = reader.finish().map_err(|e| err("Cannot read note", e))?;
        let mut buffer = std::mem::take(&mut digest.captured);
        if !digest.utf8_ok {
            return Err("The note is not valid UTF-8 and cannot be shown".into());
        }
        if digest.bytes != length || file.metadata().map_err(|e| err("Cannot inspect note", e))?.len() != length {
            return Err("The note changed while it was being read; try again".into());
        }
        if expected_hash.is_some_and(|expected| expected != digest.hash) {
            return Err("The note changed since the previous page; reload it".into());
        }
        let remaining = length - offset;
        let mut end = if remaining <= PAGE_BYTES as u64 {
            remaining as usize
        } else {
            buffer[..PAGE_BYTES].iter().rposition(|b| *b == b'\n').map(|i| i + 1).unwrap_or(PAGE_BYTES)
        };
        while end > 0 && end < buffer.len() && (buffer[end] & 0xC0) == 0x80 {
            end -= 1;
        }
        buffer.truncate(end);
        let text = String::from_utf8(buffer).map_err(|_| "The note is not valid UTF-8".to_string())?;
        let next = offset + end as u64;
        Ok(VaultDocumentPage {
            vault_id: vault.vault_id.clone(),
            revision: self.current_revision(&vault.vault_id),
            path: components.join("/"),
            hash: digest.hash,
            byte_length: length,
            byte_offset: offset,
            length: end as u64,
            next_offset: (next < length).then_some(next),
            text,
            starts_mid_line,
            fence: scanner.fence(),
            frontmatter: scanner.in_frontmatter(),
            line_number: scanner.lines_seen() + 1,
        })
    }
}

impl StoredVault {
    pub(crate) fn binding(&self) -> VaultBinding {
        VaultBinding { vault_id: self.vault_id.clone(), root: self.root.clone(), mode: self.mode, scope: self.scope.clone() }
    }
}
