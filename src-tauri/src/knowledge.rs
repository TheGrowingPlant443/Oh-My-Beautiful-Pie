//! Knowledge backend: human-registered Markdown vaults, a safe in-process vault reader/index, passive Obsidian,
//! Graphify and Cognee discovery, and immutable Graphify admission.
//!
//! Only a human picker/configuration registers a vault root (`prepare_knowledge_vault` → `confirm_knowledge_vault`);
//! no model or tool parameter can grant path access. Probes (`get_knowledge_capabilities`) never launch an
//! application, run a CLI, write another product's settings or copy notes. Vaults hold app-authored Markdown only;
//! private state (bindings, tokens, indexes, Graphify snapshots) lives under `<app-config>/knowledge/`.
//!
//! Layout: `fsio` (rooted, hardlink-safe IO), `markdown` (line scanner), `index` (link index/queries), `vault`
//! (registration, listing, document pages, writers), `integrations` (Obsidian/Cognee), `graphify`, `dto`.
mod dto;
pub(crate) mod fsio;
mod graphify;
mod index;
mod integrations;
mod markdown;
mod rooted;
mod note_publication;
mod publication_journal;
#[cfg(test)]
mod publication_recovery_regression;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod graphify_regression;
#[cfg(test)]
mod integration_regression;
mod vault;

pub use dto::*;
pub use graphify::{GraphifyArtifact, GraphifyArtifactCheck, GraphifyCounts, GraphifyIssue, GraphifyMetadataReceipt};
pub use vault::{VaultRoot, WritePrecondition, WriteReceipt};

use integrations::{Environment, Integrations};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(crate) type Result<T, E = String> = std::result::Result<T, E>;

const MAX_STATE_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredVault {
    vault_id: String,
    root: String,
    mode: VaultMode,
    scope: KnowledgeScope,
    #[serde(default)]
    identity: Option<String>,
    #[serde(default)]
    binding_revision: u64,
    #[serde(default)]
    created_ms: u64,
    #[serde(default)]
    updated_ms: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct VaultsFile {
    version: u32,
    vaults: Vec<StoredVault>,
}

#[derive(Default)]
pub(crate) struct State {
    vaults: Vec<StoredVault>,
    vaults_error: Option<String>,
    integrations: Integrations,
    integrations_error: Option<String>,
    pending: HashMap<String, vault::Pending>,
}

/// The knowledge service. Constructed once and owned by `AppState`; cheap to share (`Arc`).
pub struct KnowledgeService {
    /// `<app-config>/knowledge`
    dir: PathBuf,
    app_config: PathBuf,
    home: PathBuf,
    env: Environment,
    protected: Mutex<Vec<PathBuf>>,
    state: Mutex<State>,
    indexes: Mutex<HashMap<String, Arc<index::Slot>>>,
    graphify_cache: Mutex<HashMap<String, Arc<graphify::Loaded>>>,
    authorities: Mutex<HashMap<String, Arc<RwLock<u64>>>>,
}

fn load_file<T: serde::de::DeserializeOwned + Default>(path: &Path, version_of: impl Fn(&T) -> u32) -> (T, Option<String>) {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (T::default(), None),
        Err(e) => (T::default(), Some(e.to_string())),
        Ok(_) => match fsio::read_regular_bounded(path, MAX_STATE_BYTES).and_then(|bytes| serde_json::from_slice::<T>(&bytes).map_err(|e| e.to_string())) {
            Ok(value) if version_of(&value) > 1 => (T::default(), Some(format!("{} was written by a newer version", path.display()))),
            Ok(value) => (value, None),
            Err(e) => (T::default(), Some(e)),
        },
    }
}

impl KnowledgeService {
    /// `app_config` is the application's configuration folder (state goes to `<app_config>/knowledge`), `home` the
    /// user's home folder (`Registry.home`). The `Registry` itself is not needed.
    pub fn new(app_config: PathBuf, home: PathBuf) -> Arc<Self> {
        Self::with_environment(app_config, home, Environment::from_process())
    }

    pub(crate) fn with_environment(app_config: PathBuf, home: PathBuf, env: Environment) -> Arc<Self> {
        let dir = app_config.join("knowledge");
        let (vaults_file, vaults_error) = load_file::<VaultsFile>(&dir.join("vaults.json"), |f| f.version);
        let (integrations, integrations_error) = load_file::<Integrations>(&dir.join("integrations.json"), |i| i.version);
        graphify::cleanup_incoming(&dir.join("graphify"));
        // Startup precedes every writer in this process: finish whatever a crash left half-published.
        if let Err(error) = note_publication::recover_publications(&dir.join("publications")) { eprintln!("Recovering vault publications: {error}"); }
        Arc::new(Self {
            dir,
            app_config,
            home,
            env,
            protected: Mutex::new(Vec::new()),
            state: Mutex::new(State { vaults: vaults_file.vaults, vaults_error, integrations, integrations_error, pending: HashMap::new() }),
            indexes: Mutex::new(HashMap::new()),
            graphify_cache: Mutex::new(HashMap::new()),
            authorities: Mutex::new(HashMap::new()),
        })
    }

    /// Adds folders a vault may never overlap (for example the native OMP agent folders from `setup::paths`).
    pub fn add_protected_roots(&self, roots: Vec<PathBuf>) {
        let mut protected = self.protected.lock();
        for root in roots {
            if !protected.iter().any(|p| fsio::same_path(p, &root)) {
                protected.push(root);
            }
        }
    }

    fn save_vaults(&self, vaults: &[StoredVault]) -> Result<()> {
        fs::create_dir_all(&self.dir).map_err(|e| fsio::err("Cannot create the knowledge folder", e))?;
        crate::dependency_security::atomic_json(&self.dir.join("vaults.json"), &VaultsFile { version: 1, vaults: vaults.to_vec() }, MAX_STATE_BYTES)
    }

    fn save_integrations(&self, integrations: &Integrations) -> Result<()> {
        fs::create_dir_all(&self.dir).map_err(|e| fsio::err("Cannot create the knowledge folder", e))?;
        let mut stored = integrations.clone();
        stored.version = 1;
        crate::dependency_security::atomic_json(&self.dir.join("integrations.json"), &stored, MAX_STATE_BYTES)
    }

    /// The offered global vault location: `<home>/OMP Knowledge/Global`.
    pub(crate) fn default_global_root(&self) -> PathBuf {
        self.home.join("OMP Knowledge").join("Global")
    }

    /// The repository root of a project folder (`setup::find_repository_root`), never the home folder.
    pub(crate) fn repository_root(&self, folder: &str) -> Result<PathBuf> {
        let folder = folder.trim();
        if folder.is_empty() {
            return Err("Choose a project folder first".into());
        }
        let root = crate::setup::find_repository_root(Path::new(folder))?;
        if crate::setup::canonical(&self.home).is_ok_and(|home| fsio::same_path(&home, &root)) {
            return Err("The home folder is not a project".into());
        }
        Ok(root)
    }

    /// Passive probe: no launch, no CLI, no import, no snapshot. `folder` selects the project-conditional parts.
    pub fn get_knowledge_capabilities(&self, folder: Option<&str>) -> Result<KnowledgeCapabilities> {
        let folder = folder.map(str::trim).filter(|f| !f.is_empty());
        let repository = folder.map(|f| self.repository_root(f));
        let repository_ref = repository.as_ref().and_then(|r| r.as_ref().ok());
        let declined = repository_ref.is_some_and(|r| self.state.lock().integrations.obsidian.declined_projects.contains(&fsio::norm(r)));
        let obsidian = self.probe_obsidian_for(declined);
        let cognee = self.probe_cognee();
        let graphify = self.graphify_capability(folder);
        let (global_vault, project_vault) = {
            let state = self.state.lock();
            let global = state.vaults.iter().find(|v| v.scope == KnowledgeScope::Global).map(StoredVault::binding);
            let project = repository_ref.and_then(|r| {
                let key = vault::scope_key(&KnowledgeScope::Project { repository_root: r.to_string_lossy().into_owned() });
                state.vaults.iter().find(|v| vault::scope_key(&v.scope) == key).map(StoredVault::binding)
            });
            (global, project)
        };
        Ok(KnowledgeCapabilities {
            folder: folder.map(String::from),
            obsidian,
            graphify,
            cognee,
            project_vault,
            global_vault,
            suggested_project_root: repository_ref.map(|r| r.join("Knowledge").to_string_lossy().into_owned()),
            suggested_global_root: Some(self.default_global_root().to_string_lossy().into_owned()),
        })
    }
}
