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

#[cfg(unix)]
const SDK_PACKAGE: &str = "node_modules/@oh-my-pi/pi-coding-agent";

/// Where the installed OMP SDK (`@oh-my-pi/pi-coding-agent`) may be, most specific first. `OMP_SDK_DIR` is exclusive.
fn sdk_candidates(
    home: &Path,
    env: &dyn Fn(&str) -> Option<std::ffi::OsString>,
    omp: Option<&Path>,
) -> Vec<PathBuf> {
    if let Some(path) = env("OMP_SDK_DIR") {
        return vec![PathBuf::from(path)];
    }
    let mut candidates = vec![];
    if let Some(root) = env("BUN_INSTALL") {
        candidates.push(
            PathBuf::from(root).join("install/global/node_modules/@oh-my-pi/pi-coding-agent"),
        );
    }
    candidates.push(home.join(".bun/install/global/node_modules/@oh-my-pi/pi-coding-agent"));
    #[cfg(unix)]
    {
        // The app's user-owned Nix SDK directory; see the README "Linux and macOS" setup.
        for data in app_data_dirs(home, env) {
            candidates.push(data.join("omp-manager/sdk").join(SDK_PACKAGE));
        }
        // A package-manager install: `omp` is (a link into) `<prefix>/node_modules/@oh-my-pi/pi-coding-agent/...`.
        if let Some(real) = omp
            .filter(|path| path.is_absolute())
            .and_then(|path| fs::canonicalize(path).ok())
        {
            let ancestors = real.ancestors().skip(1).filter(|a| a.parent().is_some());
            for ancestor in ancestors {
                if ancestor.ends_with("@oh-my-pi/pi-coding-agent") {
                    candidates.push(ancestor.to_path_buf());
                }
                candidates.push(ancestor.join(SDK_PACKAGE));
            }
        }
    }
    #[cfg(not(unix))]
    let _ = omp;
    candidates
}

#[cfg(unix)]
fn app_data_dirs(home: &Path, env: &dyn Fn(&str) -> Option<std::ffi::OsString>) -> Vec<PathBuf> {
    let mut dirs = vec![];
    if let Some(data) = env("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    {
        dirs.push(data);
    }
    dirs.push(home.join(".local/share"));
    #[cfg(target_os = "macos")]
    dirs.push(home.join("Library/Application Support"));
    dirs
}

pub(crate) fn sdk_dir(home: &Path) -> Result<PathBuf> {
    let omp = crate::toolchain::omp_program();
    let candidates = sdk_candidates(home, &|name| std::env::var_os(name), Some(Path::new(&omp)));
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
    let hint = if cfg!(unix) {
        " On Linux/macOS build the matching SDK with Nix as described in the README section \"Linux and macOS\", then set OMP_SDK_DIR to its @oh-my-pi/pi-coding-agent directory."
    } else {
        ""
    };
    Err(format!("Portable config requires Bun and the installed OMP SDK. Set BUN_BIN / OMP_SDK_DIR if they are not discoverable.{hint}"))
}

pub fn native_request(root: &Path, home: &Path, mut request: Value) -> Result<Value> {
    let sdk = sdk_dir(home)?;
    let package: Value =
        serde_json::from_slice(&fs::read(sdk.join("package.json")).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let sdk_version = package["version"]
        .as_str()
        .ok_or("OMP SDK has no version")?;
    let omp = crate::toolchain::omp_program();
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
    let bun = crate::toolchain::bun_program();
    let mut child = command(bun)
        .arg(root.join("omp-config.mjs"))
        .current_dir(cwd)
        .env_remove("PI_COMPILED")
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
    let mut response: Value = serde_json::from_slice(&output.stdout).map_err(|_| {
        "OMP config helper returned invalid output. Check Bun and OMP SDK compatibility."
            .to_string()
    })?;
    if !output.status.success() || response["ok"] != true {
        return Err(response["error"]
            .as_str()
            .unwrap_or("OMP config helper failed")
            .into());
    }
    Ok(response["data"].take())
}

pub fn preview(root: &Path, home: &Path, request: Value) -> Result<ConfigPreview> {
    serde_json::from_value(native_request(root, home, request)?).map_err(|e| e.to_string())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedConfig {
    pub path: PathBuf,
    pub modified_ms: u64,
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
    #[must_use]
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
    #[must_use]
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
        self.write_pointer(&mut state, &path)?;
        Ok(path)
    }
    fn write_pointer(&self, state: &mut DefaultConfigState, path: &Path) -> Result<()> {
        let temporary = self.root.join("global-config.json.tmp");
        let data = serde_json::to_vec_pretty(&Pointer {
            version: 1,
            path: path.to_path_buf(),
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
        state.path = Some(path.to_path_buf());
        state.error = None;
        Ok(())
    }
    fn normalize(path: &Path) -> Result<PathBuf> {
        let path =
            fs::canonicalize(path).map_err(|e| format!("Global config is unavailable: {e}"))?;
        Ok(PathBuf::from(
            path.to_string_lossy().trim_start_matches("\\\\?\\"),
        ))
    }
    #[must_use]
    pub fn list(&self) -> Vec<SavedConfig> {
        let Ok(entries) = fs::read_dir(self.root.join("global-configs")) else {
            return Vec::new();
        };
        let mut found: Vec<SavedConfig> = entries
            .flatten()
            .filter_map(|entry| {
                let path = Self::normalize(&entry.path().join("config.yml")).ok()?;
                Self::validate_path(&self.root, &path).ok()?;
                let modified = fs::metadata(&path).ok()?.modified().ok()?;
                let modified_ms = modified
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                Some(SavedConfig { path, modified_ms })
            })
            .collect();
        found.sort_by(|a, b| {
            b.modified_ms
                .cmp(&a.modified_ms)
                .then_with(|| a.path.cmp(&b.path))
        });
        found
    }
    pub fn activate(&self, path: &Path) -> Result<PathBuf> {
        let mut state = self.state.lock().map_err(|e| e.to_string())?;
        let path = Self::normalize(path)?;
        Self::validate_path(&self.root, &path)?;
        self.write_pointer(&mut state, &path)?;
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
    #[test]
    fn lists_snapshots_newest_first_and_activation_reuses_them() {
        let root = std::env::temp_dir().join(format!("omp-config-list-{}", uuid::Uuid::new_v4()));
        let store = GlobalConfigStore::load(root.clone());
        let a = store.publish("modelRoles:\n  default: first\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        let b = store.publish("modelRoles:\n  default: second\n").unwrap();
        fs::create_dir(root.join("global-configs").join("stray")).unwrap();
        let listed: Vec<PathBuf> = store.list().into_iter().map(|c| c.path).collect();
        assert_eq!(listed, vec![b.clone(), a.clone()]);
        let count = || fs::read_dir(root.join("global-configs")).unwrap().count();
        let before = count();
        assert_eq!(store.activate(&a).unwrap(), a);
        assert_eq!(store.current_path().unwrap(), Some(a.clone()));
        assert_eq!(count(), before);
        assert_eq!(
            GlobalConfigStore::load(root.clone())
                .current_path()
                .unwrap(),
            Some(a.clone())
        );
        let outside = root.join("outside.yml");
        fs::write(&outside, "x: 1\n").unwrap();
        assert!(store.activate(&outside).is_err());
        assert_eq!(store.current_path().unwrap(), Some(a));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn explicit_sdk_dir_is_exclusive_and_defaults_cover_bun_global() {
        let env =
            |name: &str| (name == "OMP_SDK_DIR").then(|| std::ffi::OsString::from("/explicit/sdk"));
        assert_eq!(
            sdk_candidates(Path::new("/home/u"), &env, None),
            vec![PathBuf::from("/explicit/sdk")]
        );
        let none = |_: &str| None;
        let defaults = sdk_candidates(Path::new("/home/u"), &none, None);
        assert_eq!(
            defaults[0],
            PathBuf::from("/home/u/.bun/install/global/node_modules/@oh-my-pi/pi-coding-agent")
        );
    }
    #[cfg(unix)]
    #[test]
    fn unix_defaults_include_the_app_sdk_dir_and_the_package_around_omp() {
        let env = |name: &str| (name == "XDG_DATA_HOME").then(|| std::ffi::OsString::from("/xdg"));
        let home = Path::new("/home/u");
        let candidates = sdk_candidates(home, &env, None);
        assert!(candidates.contains(&PathBuf::from(
            "/xdg/omp-manager/sdk/node_modules/@oh-my-pi/pi-coding-agent"
        )));
        assert!(candidates.contains(&PathBuf::from(
            "/home/u/.local/share/omp-manager/sdk/node_modules/@oh-my-pi/pi-coding-agent"
        )));
        // A relative XDG_DATA_HOME is ignored per the XDG specification.
        let relative =
            |name: &str| (name == "XDG_DATA_HOME").then(|| std::ffi::OsString::from("relative"));
        assert!(sdk_candidates(home, &relative, None)
            .iter()
            .all(|c| c.is_absolute()));

        let prefix = std::env::temp_dir().join(format!("omp-sdk-prefix-{}", uuid::Uuid::new_v4()));
        let package = prefix.join("lib/node_modules/@oh-my-pi/pi-coding-agent");
        fs::create_dir_all(package.join("src")).unwrap();
        fs::create_dir_all(prefix.join("bin")).unwrap();
        fs::write(package.join("src/cli.ts"), "").unwrap();
        std::os::unix::fs::symlink(package.join("src/cli.ts"), prefix.join("bin/omp")).unwrap();
        let canonical = fs::canonicalize(&package).unwrap();
        assert!(
            sdk_candidates(home, &|_| None, Some(&prefix.join("bin/omp"))).contains(&canonical)
        );
        assert!(sdk_candidates(home, &|_| None, Some(Path::new("omp")))
            .iter()
            .all(|c| c.is_absolute()));
        fs::remove_dir_all(prefix).unwrap();
    }
}
