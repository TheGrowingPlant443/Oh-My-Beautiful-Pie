//! Versioned on-disk state under `<app-config>/code-quality/`. Nonsecret only.
//! Corrupt or unknown-version files are reported, never silently reset (ownership records must survive).
use super::secrets::sha256_hex;
use super::types::*;
use crate::dependency_security::{atomic_json, bounded_json};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const MAX_STATE_BYTES: u64 = 1024 * 1024;
pub const MAX_RESULT_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConfigFile {
    pub version: u32,
    pub config: QualityConfig,
    pub revision: u64,
    pub activated: bool,
}

impl Default for ConfigFile {
    fn default() -> Self {
        ConfigFile { version: 1, config: QualityConfig::default(), revision: 0, activated: false }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InstallationFile {
    version: u32,
    id: String,
}

/// One container the app created. Recorded as soon as it is discoverable.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceRecord {
    pub id: String,
    pub name: String,
    pub role: String,
    pub runtime: QualityRuntime,
    pub fingerprint: String,
    pub created_at: u64,
    /// Hash of the creation-relevant settings (image, port, limits); a mismatch recreates the container.
    pub spec_hash: String,
}

/// Per-runtime ownership: which daemon, and whether managed server containers may still be running there.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeRecord {
    pub runtime: QualityRuntime,
    pub fingerprint: String,
    pub possibly_running: bool,
    pub port: u16,
}

/// A submitted Sonar compute-engine task whose terminal state was not observed.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PendingTask {
    pub scan_id: String,
    pub task_id: String,
    pub config: QualityConfig,
    pub project_key: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceFile {
    pub version: u32,
    pub containers: Vec<ResourceRecord>,
    pub runtimes: Vec<RuntimeRecord>,
    pub tasks: Vec<PendingTask>,
}

impl ResourceFile {
    pub fn new() -> Self {
        ResourceFile { version: 1, ..Default::default() }
    }
    pub fn runtime_mut(&mut self, runtime: &QualityRuntime) -> Option<&mut RuntimeRecord> {
        self.runtimes.iter_mut().find(|r| &r.runtime == runtime)
    }
    pub fn runtime(&self, runtime: &QualityRuntime) -> Option<&RuntimeRecord> {
        self.runtimes.iter().find(|r| &r.runtime == runtime)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectFile {
    pub version: u32,
    pub project: Option<QualityProject>,
    /// Newest first; capped at 20.
    pub recent: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConsentFile {
    pub version: u32,
    /// `<folder-hash>:<recipe-hash>` pairs.
    pub approved: Vec<String>,
}

/// Request persisted with each job (provenance + restart recovery).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JobRequest {
    pub version: u32,
    pub id: String,
    pub project: QualityProject,
    pub config: QualityConfig,
    pub recipe_hash: String,
    pub started_at: u64,
}

pub struct Paths {
    pub root: PathBuf,
}

impl Paths {
    pub fn new(root: PathBuf) -> Self {
        Paths { root }
    }
    pub fn config(&self) -> PathBuf {
        self.root.join("config.json")
    }
    pub fn installation(&self) -> PathBuf {
        self.root.join("installation.json")
    }
    pub fn resources(&self) -> PathBuf {
        self.root.join("resources.json")
    }
    pub fn consents(&self) -> PathBuf {
        self.root.join("consents.json")
    }
    pub fn project(&self, folder: &str) -> PathBuf {
        self.root.join("projects").join(format!("{}.json", folder_hash(folder)))
    }
    pub fn jobs(&self) -> PathBuf {
        self.root.join("jobs")
    }
    pub fn job(&self, id: &str) -> PathBuf {
        self.root.join("jobs").join(id)
    }
    pub fn result(&self, id: &str) -> PathBuf {
        self.job(id).join("result.json")
    }
}

pub fn folder_hash(folder: &str) -> String {
    sha256_hex(&folder.to_lowercase())[..32].to_string()
}

pub fn valid_job_id(id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok()
}

fn read_versioned<T: for<'de> Deserialize<'de>>(path: &Path, max: u64, what: &str) -> Result<Option<T>> {
    bounded_json::<T>(path, max).map_err(|e| {
        format!(
            "The code quality {what} file {} is unreadable ({e}). Fix or remove it manually; the app will not reset it because it records container ownership.",
            path.display()
        )
    })
}

pub fn load_config(paths: &Paths) -> Result<ConfigFile> {
    match read_versioned::<ConfigFile>(&paths.config(), MAX_STATE_BYTES, "settings")? {
        None => Ok(ConfigFile::default()),
        Some(file) if file.version == 1 && file.config.version == 1 => Ok(file),
        Some(_) => Err("The code quality settings file has an unsupported version. Update OMP Manager or remove the file manually.".into()),
    }
}

pub fn save_config(paths: &Paths, file: &ConfigFile) -> Result<()> {
    std::fs::create_dir_all(&paths.root).map_err(|e| e.to_string())?;
    atomic_json(&paths.config(), file, MAX_STATE_BYTES)
}

pub fn load_resources(paths: &Paths) -> Result<ResourceFile> {
    match read_versioned::<ResourceFile>(&paths.resources(), MAX_STATE_BYTES, "resource")? {
        None => Ok(ResourceFile::new()),
        Some(file) if file.version == 1 => Ok(file),
        Some(_) => Err("The code quality resource file has an unsupported version. Update OMP Manager or remove the file manually.".into()),
    }
}

pub fn save_resources(paths: &Paths, file: &ResourceFile) -> Result<()> {
    std::fs::create_dir_all(&paths.root).map_err(|e| e.to_string())?;
    atomic_json(&paths.resources(), file, MAX_STATE_BYTES)
}

/// Stable random installation id; owner label value for every container the app creates.
pub fn load_installation(paths: &Paths) -> Result<String> {
    if let Some(file) = read_versioned::<InstallationFile>(&paths.installation(), MAX_STATE_BYTES, "installation")? {
        if file.version == 1 && uuid::Uuid::parse_str(&file.id).is_ok() {
            return Ok(file.id);
        }
        return Err("The code quality installation file is invalid. Remove it manually only if no managed containers exist.".into());
    }
    std::fs::create_dir_all(&paths.root).map_err(|e| e.to_string())?;
    let id = uuid::Uuid::new_v4().to_string();
    atomic_json(&paths.installation(), &InstallationFile { version: 1, id: id.clone() }, MAX_STATE_BYTES)?;
    Ok(id)
}

pub fn load_project(paths: &Paths, folder: &str) -> Result<ProjectFile> {
    Ok(read_versioned::<ProjectFile>(&paths.project(folder), MAX_STATE_BYTES, "project")?.unwrap_or(ProjectFile { version: 1, project: None, recent: Vec::new() }))
}

pub fn save_project(paths: &Paths, folder: &str, file: &ProjectFile) -> Result<()> {
    std::fs::create_dir_all(paths.root.join("projects")).map_err(|e| e.to_string())?;
    atomic_json(&paths.project(folder), file, MAX_STATE_BYTES)
}

pub fn load_consents(paths: &Paths) -> Result<ConsentFile> {
    Ok(read_versioned::<ConsentFile>(&paths.consents(), MAX_STATE_BYTES, "consent")?.unwrap_or(ConsentFile { version: 1, approved: Vec::new() }))
}

pub fn save_consents(paths: &Paths, file: &ConsentFile) -> Result<()> {
    std::fs::create_dir_all(&paths.root).map_err(|e| e.to_string())?;
    atomic_json(&paths.consents(), file, MAX_STATE_BYTES)
}

pub fn consent_key(folder: &str, recipe_hash: &str) -> String {
    format!("{}:{}", folder_hash(folder), recipe_hash)
}

pub fn load_scan(paths: &Paths, id: &str) -> Result<Option<QualityScan>> {
    if !valid_job_id(id) {
        return Err("Invalid scan id".into());
    }
    bounded_json::<QualityScan>(&paths.result(id), MAX_RESULT_BYTES)
}

pub fn save_scan(paths: &Paths, scan: &QualityScan) -> Result<()> {
    if !valid_job_id(&scan.id) {
        return Err("Invalid scan id".into());
    }
    std::fs::create_dir_all(paths.job(&scan.id)).map_err(|e| e.to_string())?;
    atomic_json(&paths.result(&scan.id), scan, MAX_RESULT_BYTES)
}

pub fn save_request(paths: &Paths, request: &JobRequest) -> Result<()> {
    std::fs::create_dir_all(paths.job(&request.id)).map_err(|e| e.to_string())?;
    atomic_json(&paths.job(&request.id).join("request.json"), request, MAX_STATE_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_paths() -> (Paths, PathBuf) {
        let dir = std::env::temp_dir().join(format!("omp-cq-store-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        (Paths::new(dir.clone()), dir)
    }

    #[test]
    fn missing_config_is_default_and_roundtrips() {
        let (paths, dir) = temp_paths();
        assert_eq!(load_config(&paths).unwrap(), ConfigFile::default());
        let mut file = ConfigFile::default();
        file.revision = 4;
        file.activated = true;
        save_config(&paths, &file).unwrap();
        assert_eq!(load_config(&paths).unwrap(), file);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn corrupt_or_future_state_is_rejected_not_reset() {
        let (paths, dir) = temp_paths();
        std::fs::write(paths.resources(), b"{ not json").unwrap();
        let error = load_resources(&paths).unwrap_err();
        assert!(error.contains("ownership"), "{error}");
        assert_eq!(std::fs::read(paths.resources()).unwrap(), b"{ not json");
        std::fs::write(paths.config(), br#"{"version":2,"config":{"version":2,"mode":"alternate","runtime":{"kind":"dockerDesktop"},"communityPort":9000,"enterpriseUrl":""},"revision":0,"activated":false}"#).unwrap();
        assert!(load_config(&paths).unwrap_err().contains("unsupported version"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn installation_id_is_stable() {
        let (paths, dir) = temp_paths();
        let first = load_installation(&paths).unwrap();
        assert_eq!(load_installation(&paths).unwrap(), first);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn job_ids_must_be_uuids() {
        let (paths, dir) = temp_paths();
        assert!(load_scan(&paths, "..\\..\\x").is_err());
        assert_eq!(load_scan(&paths, &uuid::Uuid::new_v4().to_string()).unwrap(), None);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
