use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub type Result<T, E = String> = std::result::Result<T, E>;

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigPreview {
    pub yaml: String,
    pub model_roles: serde_json::Map<String, Value>,
    pub omitted: Vec<String>,
}

pub fn materialize(root: &Path) -> Result<()> {
    fs::create_dir_all(root).map_err(|e| e.to_string())?;
    let path = root.join("omp-config.mjs");
    let source = include_str!("../../bridge/omp-config.mjs");
    if fs::read_to_string(&path).ok().as_deref() != Some(source) {
        fs::write(path, source).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command
}

fn sdk_dir(home: &Path) -> Result<PathBuf> {
    let mut candidates = vec![];
    if let Some(path) = std::env::var_os("OMP_SDK_DIR") {
        candidates.push(PathBuf::from(path));
    } else {
        if let Some(root) = std::env::var_os("BUN_INSTALL") {
            candidates.push(
                PathBuf::from(root).join("install/global/node_modules/@oh-my-pi/pi-coding-agent"),
            );
        }
        candidates.push(home.join(".bun/install/global/node_modules/@oh-my-pi/pi-coding-agent"));
    }
    for candidate in candidates {
        let package = fs::read(candidate.join("package.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        if package
            .as_ref()
            .is_some_and(|p| p["name"] == "@oh-my-pi/pi-coding-agent")
            && candidate.join("src/config/settings.ts").is_file()
            && candidate.join("src/config/registry.ts").is_file()
        {
            let path = fs::canonicalize(candidate).map_err(|e| e.to_string())?;
            // Bun package resolution cannot consume Windows extended-length prefixes.
            return Ok(PathBuf::from(
                path.to_string_lossy().trim_start_matches("\\\\?\\"),
            ));
        }
    }
    Err("Portable config requires Bun and the installed OMP SDK. Set BUN_BIN / OMP_SDK_DIR if they are not discoverable.".into())
}

pub fn native_request(root: &Path, home: &Path, mut request: Value) -> Result<Value> {
    let sdk = sdk_dir(home)?;
    let package: Value =
        serde_json::from_slice(&fs::read(sdk.join("package.json")).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let sdk_version = package["version"]
        .as_str()
        .ok_or("OMP SDK has no version")?;
    let omp = std::env::var_os("OMP_BIN").unwrap_or_else(|| "omp".into());
    let version = command(omp)
        .arg("--version")
        .output()
        .map_err(|e| format!("Read OMP version: {e}"))?;
    let runtime_version = String::from_utf8_lossy(&version.stdout);
    if !version.status.success() || runtime_version.trim() != format!("omp/{sdk_version}") {
        return Err(format!("OMP SDK {sdk_version} does not match executable {}. Set OMP_SDK_DIR to its installed SDK.", runtime_version.trim()));
    }
    request["sdkDir"] = json!(sdk);
    let cwd = request["cwd"]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or_else(|| home.to_path_buf());
    let bun = std::env::var_os("BUN_BIN").unwrap_or_else(|| "bun".into());
    let mut child = command(bun)
        .arg(root.join("omp-config.mjs"))
        .current_dir(cwd)
        .env("PI_COMPILED", "false")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            format!("Portable config requires Bun. Set BUN_BIN if it is not discoverable: {e}")
        })?;
    let encoded = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
    let written = child
        .stdin
        .take()
        .ok_or("config helper stdin unavailable")?
        .write_all(&encoded);
    if let Err(error) = written {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("Write native config request: {error}"));
    }
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    let response: Value = serde_json::from_slice(&output.stdout).map_err(|_| {
        "OMP config helper returned invalid output. Check Bun and OMP SDK compatibility."
            .to_string()
    })?;
    if !output.status.success() || response["ok"] != true {
        return Err(response["error"]
            .as_str()
            .unwrap_or("OMP config helper failed")
            .into());
    }
    Ok(response["data"].clone())
}

pub fn preview(root: &Path, home: &Path, request: Value) -> Result<ConfigPreview> {
    serde_json::from_value(native_request(root, home, request)?).map_err(|e| e.to_string())
}

#[derive(Default)]
struct DefaultConfigState {
    path: Option<PathBuf>,
    error: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct Pointer {
    version: u32,
    path: PathBuf,
}
#[derive(Serialize)]
pub struct GlobalConfigInfo {
    pub path: Option<PathBuf>,
    pub error: Option<String>,
}

pub struct GlobalConfigStore {
    root: PathBuf,
    state: Mutex<DefaultConfigState>,
}
impl GlobalConfigStore {
    pub fn load(root: PathBuf) -> Self {
        let loaded = match fs::read(root.join("global-config.json")) {
            Ok(bytes) => serde_json::from_slice::<Pointer>(&bytes)
                .map_err(|e| e.to_string())
                .and_then(|p| {
                    if p.version == 1 {
                        Ok(p.path)
                    } else {
                        Err("Unsupported global config version".into())
                    }
                })
                .and_then(|path| Self::validate_path(&root, &path).map(|_| path))
                .map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.to_string()),
        };
        let state = match loaded {
            Ok(path) => DefaultConfigState { path, error: None },
            Err(error) => DefaultConfigState {
                path: None,
                error: Some(error),
            },
        };
        Self {
            root,
            state: Mutex::new(state),
        }
    }
    fn validate_path(root: &Path, path: &Path) -> Result<()> {
        if !path.is_absolute() || !path.starts_with(root.join("global-configs")) {
            return Err("Global config pointer is outside app-owned snapshots".into());
        }
        let directory = fs::canonicalize(root.join("global-configs")).map_err(|e| e.to_string())?;
        let owner = fs::canonicalize(root).map_err(|e| e.to_string())?;
        if !directory.starts_with(&owner) {
            return Err("Global config snapshots escape the app-owned directory".into());
        }
        let file =
            fs::canonicalize(path).map_err(|e| format!("Global config is unavailable: {e}"))?;
        if !file.starts_with(directory) || !file.is_file() {
            return Err("Global config pointer escapes app-owned snapshots".into());
        }
        Ok(())
    }
    pub fn info(&self) -> GlobalConfigInfo {
        match self.state.lock() {
            Ok(state) => {
                let error = state.error.clone().or_else(|| {
                    state
                        .path
                        .as_ref()
                        .and_then(|path| Self::validate_path(&self.root, path).err())
                });
                GlobalConfigInfo {
                    path: state.path.clone(),
                    error,
                }
            }
            Err(e) => GlobalConfigInfo {
                path: None,
                error: Some(e.to_string()),
            },
        }
    }
    pub fn current_path(&self) -> Result<Option<PathBuf>> {
        let info = self.info();
        if let Some(error) = info.error {
            return Err(error);
        }
        Ok(info.path)
    }
    pub fn publish(&self, yaml: &str) -> Result<PathBuf> {
        let mut state = self.state.lock().map_err(|e| e.to_string())?;
        let snapshots = self.root.join("global-configs");
        fs::create_dir_all(&snapshots).map_err(|e| e.to_string())?;
        let owner = fs::canonicalize(&self.root).map_err(|e| e.to_string())?;
        if !fs::canonicalize(&snapshots)
            .map_err(|e| e.to_string())?
            .starts_with(owner)
        {
            return Err("Global config snapshots escape the app-owned directory".into());
        }
        let folder = snapshots.join(uuid::Uuid::new_v4().to_string());
        fs::create_dir(&folder).map_err(|e| e.to_string())?;
        let path = folder.join("config.yml");
        fs::write(&path, yaml).map_err(|e| e.to_string())?;
        let path = fs::canonicalize(path).map_err(|e| e.to_string())?;
        let path = PathBuf::from(path.to_string_lossy().trim_start_matches("\\\\?\\"));
        Self::validate_path(&self.root, &path)?;
        let temporary = self.root.join("global-config.json.tmp");
        let data = serde_json::to_vec_pretty(&Pointer {
            version: 1,
            path: path.clone(),
        })
        .map_err(|e| e.to_string())?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|e| e.to_string())?;
        if let Err(error) = file.write_all(&data).and_then(|_| file.sync_all()) {
            drop(file);
            let _ = fs::remove_file(&temporary);
            return Err(error.to_string());
        }
        drop(file);
        if let Err(error) = fs::rename(&temporary, self.root.join("global-config.json")) {
            let _ = fs::remove_file(&temporary);
            return Err(error.to_string());
        }
        state.path = Some(path.clone());
        state.error = None;
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn snapshots_are_immutable_and_bad_pointers_do_not_hide_errors() {
        let root = std::env::temp_dir().join(format!("omp-config-{}", uuid::Uuid::new_v4()));
        let store = GlobalConfigStore::load(root.clone());
        assert!(store.current_path().unwrap().is_none());
        let a = store.publish("modelRoles:\n  default: first\n").unwrap();
        let b = store.publish("modelRoles:\n  default: second\n").unwrap();
        assert_ne!(a, b);
        assert_eq!(
            fs::read_to_string(&a).unwrap(),
            "modelRoles:\n  default: first\n"
        );
        assert_eq!(
            GlobalConfigStore::load(root.clone())
                .current_path()
                .unwrap(),
            Some(b)
        );
        fs::write(
            root.join("global-config.json"),
            serde_json::to_vec(&Pointer {
                version: 1,
                path: root.join("outside.yml"),
            })
            .unwrap(),
        )
        .unwrap();
        let bad = GlobalConfigStore::load(root.clone());
        assert!(bad.current_path().is_err());
        assert!(bad.info().error.unwrap().contains("outside"));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn failed_pointer_publish_keeps_previous_default() {
        let root =
            std::env::temp_dir().join(format!("omp-config-failure-{}", uuid::Uuid::new_v4()));
        let store = GlobalConfigStore::load(root.clone());
        let original = store.publish("plan:\n  enabled: true\n").unwrap();
        fs::create_dir(root.join("global-config.json.tmp")).unwrap();
        assert!(store.publish("plan:\n  enabled: false\n").is_err());
        assert_eq!(store.current_path().unwrap(), Some(original));
        fs::remove_dir_all(root).unwrap();
    }
}
