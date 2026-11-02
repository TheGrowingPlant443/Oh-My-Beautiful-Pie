//! Knowledge service state, startup recovery and passive capability aggregation.
use super::dto::{KnowledgeCapabilities, KnowledgeScope, VaultMode};
use super::integrations::{Environment, Integrations};
use super::{cognee_ui, fsio, graphify, index, note_publication, vault};
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
    pub(super) vault_id: String,
    pub(super) root: String,
    pub(super) mode: VaultMode,
    pub(super) scope: KnowledgeScope,
    #[serde(default)]
    pub(super) identity: Option<String>,
    #[serde(default)]
    pub(super) binding_revision: u64,
    #[serde(default)]
    pub(super) created_ms: u64,
    #[serde(default)]
    pub(super) updated_ms: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct VaultsFile {
    version: u32,
    vaults: Vec<StoredVault>,
}

#[derive(Default)]
pub(crate) struct State {
    pub(super) vaults: Vec<StoredVault>,
    pub(super) vaults_error: Option<String>,
    pub(super) integrations: Integrations,
    pub(super) integrations_error: Option<String>,
    pub(super) pending: HashMap<String, vault::Pending>,
}

/// The knowledge service. Constructed once and owned by `AppState`; cheap to share (`Arc`).
pub struct KnowledgeService {
    /// `<app-config>/knowledge`
    pub(super) dir: PathBuf,
    pub(super) app_config: PathBuf,
    pub(super) home: PathBuf,
    pub(super) env: Environment,
    pub(super) protected: Mutex<Vec<PathBuf>>,
    pub(super) state: Mutex<State>,
    pub(super) indexes: Mutex<HashMap<String, Arc<index::Slot>>>,
    pub(super) graphify_cache: Mutex<HashMap<String, Arc<graphify::Loaded>>>,
    pub(super) authorities: Mutex<HashMap<String, Arc<RwLock<u64>>>>,
    pub(super) cognee_ui: cognee_ui::CogneeUi,
}

fn load_file<T: serde::de::DeserializeOwned + Default>(
    path: &Path,
    version_of: impl Fn(&T) -> u32,
) -> (T, Option<String>) {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (T::default(), None),
        Err(e) => (T::default(), Some(e.to_string())),
        Ok(_) => match fsio::read_regular_bounded(path, MAX_STATE_BYTES)
            .and_then(|bytes| serde_json::from_slice::<T>(&bytes).map_err(|e| e.to_string()))
        {
            Ok(value) if version_of(&value) > 1 => (
                T::default(),
                Some(format!("{} was written by a newer version", path.display())),
            ),
            Ok(value) => (value, None),
            Err(e) => (T::default(), Some(e)),
        },
    }
}

impl KnowledgeService {
    /// `app_config` is the application's configuration folder (state goes to `<app_config>/knowledge`), `home` the
    /// user's home folder (`Registry.home`). The `Registry` itself is not needed.
    #[must_use]
    pub fn new(app_config: PathBuf, home: PathBuf) -> Arc<Self> {
        Self::with_environment(app_config, home, Environment::from_process())
    }

    pub(crate) fn with_environment(
        app_config: PathBuf,
        home: PathBuf,
        env: Environment,
    ) -> Arc<Self> {
        let dir = app_config.join("knowledge");
        let (vaults_file, vaults_error) =
            load_file::<VaultsFile>(&dir.join("vaults.json"), |f| f.version);
        let (integrations, integrations_error) =
            load_file::<Integrations>(&dir.join("integrations.json"), |i| i.version);
        graphify::cleanup_incoming(&dir.join("graphify"));
        // Startup precedes every writer in this process: finish whatever a crash left half-published.
        if let Err(error) = note_publication::recover_publications(&dir.join("publications")) {
            eprintln!("Recovering vault publications: {error}");
        }
        Arc::new(Self {
            dir,
            app_config,
            home,
            env,
            protected: Mutex::new(Vec::new()),
            state: Mutex::new(State {
                vaults: vaults_file.vaults,
                vaults_error,
                integrations,
                integrations_error,
                pending: HashMap::new(),
            }),
            indexes: Mutex::new(HashMap::new()),
            graphify_cache: Mutex::new(HashMap::new()),
            authorities: Mutex::new(HashMap::new()),
            cognee_ui: cognee_ui::CogneeUi::new(),
        })
    }

    /// Application shutdown: cancels a Cognee UI start that is still in progress (no browser is opened afterwards) and
    /// retires the frontend this app started. A Cognee UI or backend started by anyone else is never touched.
    pub fn shutdown(&self) {
        self.cognee_ui.shutdown();
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

    pub(super) fn save_vaults(&self, vaults: &[StoredVault]) -> Result<()> {
        fs::create_dir_all(&self.dir)
            .map_err(|e| fsio::err("Cannot create the knowledge folder", e))?;
        #[derive(Serialize)]
        struct SavedVaults<'vaults> {
            version: u32,
            vaults: &'vaults [StoredVault],
        }
        crate::dependency_security::atomic_json(
            &self.dir.join("vaults.json"),
            &SavedVaults { version: 1, vaults },
            MAX_STATE_BYTES,
        )
    }

    pub(super) fn save_integrations(&self, integrations: &Integrations) -> Result<()> {
        fs::create_dir_all(&self.dir)
            .map_err(|e| fsio::err("Cannot create the knowledge folder", e))?;
        let mut stored = integrations.clone();
        stored.version = 1;
        crate::dependency_security::atomic_json(
            &self.dir.join("integrations.json"),
            &stored,
            MAX_STATE_BYTES,
        )
    }

    /// The offered global vault location: `<home>/OMP Knowledge/Global`.
    #[must_use]
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
    pub fn get_knowledge_capabilities(
        &self,
        folder: Option<&str>,
    ) -> Result<KnowledgeCapabilities> {
        let folder = folder.map(str::trim).filter(|f| !f.is_empty());
        let repository = folder.map(|f| self.repository_root(f));
        let repository_ref = repository.as_ref().and_then(|r| r.as_ref().ok());
        let declined = repository_ref.is_some_and(|r| {
            self.state
                .lock()
                .integrations
                .obsidian
                .declined_projects
                .contains(&fsio::norm(r))
        });
        let obsidian = self.probe_obsidian_for(declined);
        let cognee = self.probe_cognee();
        let graphify = self.graphify_capability(folder);
        let (global_vault, project_vault) = {
            let state = self.state.lock();
            let global = state
                .vaults
                .iter()
                .find(|v| v.scope == KnowledgeScope::Global)
                .map(StoredVault::binding);
            let project = repository_ref.and_then(|r| {
                let key = vault::scope_key(&KnowledgeScope::Project {
                    repository_root: r.to_string_lossy().into_owned(),
                });
                state
                    .vaults
                    .iter()
                    .find(|v| vault::scope_key(&v.scope) == key)
                    .map(StoredVault::binding)
            });
            (global, project)
        };
        Ok(KnowledgeCapabilities {
            folder: folder.map(String::from),
            project_key: repository_ref.map(|r| fsio::norm(r)),
            obsidian,
            graphify,
            cognee,
            project_vault,
            global_vault,
            suggested_project_root: repository_ref
                .map(|r| r.join("Knowledge").to_string_lossy().into_owned()),
            suggested_global_root: Some(self.default_global_root().to_string_lossy().into_owned()),
        })
    }
}
