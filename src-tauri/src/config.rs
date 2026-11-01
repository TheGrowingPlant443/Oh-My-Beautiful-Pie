use std::fs;
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub type Result<T, E = String> = std::result::Result<T, E>;

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigPreview {
    pub yaml: String,
    pub model_roles: serde_json::Map<String, Value>,
    pub omitted: Vec<String>,
    #[serde(default = "default_setup_name")]
    pub name: String,
    #[serde(default)]
    pub assets: Vec<ConfigAsset>,
    #[serde(default)]
    pub requires_execution_review: bool,
}

fn default_setup_name() -> String {
    "Saved setup".into()
}

#[derive(Clone, Deserialize, Serialize)]
pub struct ConfigAsset {
    pub path: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<ConfigAssetFile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigAssetFile {
    pub source_path: PathBuf,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct AssetMetadata {
    pub path: String,
    pub size: u64,
    pub sha256: String,
    pub mode: u32,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelConfigAssignment {
    pub model: String,
    pub config_path: PathBuf,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupMetadata {
    pub version: u32,
    pub name: String,
    pub requires_execution_review: bool,
    pub executable_review: bool,
    pub assets: Vec<String>,
    #[serde(default)]
    pub content_sha256: Option<String>,
    #[serde(default)]
    pub asset_metadata: Vec<AssetMetadata>,
}

fn validate_asset_path(value: &str) -> Result<()> {
    let parts: Vec<&str> = value.split('/').collect();
    if value
        .chars()
        .any(|c| c.is_control() || c == '\\' || c == ':')
        || parts
            .iter()
            .any(|p| p.is_empty() || *p == "." || *p == "..")
        || !(matches!(value, "AGENTS.md" | "RULES.md" | "mcp.json")
            || (parts.len() > 1 && matches!(parts[0], "skills" | "rules")))
    {
        return Err("Invalid setup asset path".into());
    }
    Ok(())
}

fn no_symlinks(path: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Err("Setup paths must not contain traversal".into());
        }
        current.push(component);
        if fs::symlink_metadata(&current)
            .map_err(|e| e.to_string())?
            .file_type()
            .is_symlink()
        {
            return Err("Setup paths must not contain symlinks".into());
        }
    }
    Ok(())
}

fn execution_sensitive(assets: &[ConfigAsset]) -> bool {
    assets.iter().any(|asset| {
        asset.mode.is_some_and(|mode| mode & 0o111 != 0)
            || asset.path == "mcp.json"
            || asset.path.starts_with("skills/")
            || !matches!(
                Path::new(&asset.path)
                    .extension()
                    .and_then(|s| s.to_str())
                    .map(str::to_ascii_lowercase)
                    .as_deref(),
                Some("md" | "mdc" | "txt" | "json" | "yml" | "yaml")
            )
    })
}

fn credential_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase().replace(['_', '-'], "");
    matches!(
        normalized.as_str(),
        "apikey"
            | "apitoken"
            | "password"
            | "passwd"
            | "secret"
            | "secretkey"
            | "token"
            | "accesstoken"
            | "auth"
            | "auths"
            | "refreshtoken"
            | "authorization"
            | "cookie"
            | "credential"
            | "credentials"
            | "header"
            | "headers"
            | "env"
            | "privatekey"
            | "clientsecret"
    ) || [
        "token",
        "privatekey",
        "secretkey",
        "clientsecret",
        "accesskey",
        "accesskeyid",
        "secretaccesskey",
    ]
    .into_iter()
    .any(|suffix| normalized.ends_with(suffix))
        || key.rsplit(['_', '-']).next().is_some_and(|part| {
            matches!(
                part.to_ascii_lowercase().as_str(),
                "token" | "password" | "secret"
            )
        })
}

fn token_word(word: &str) -> bool {
    word.starts_with("sk-") && word.len() >= 15
        || ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"]
            .into_iter()
            .any(|prefix| word.starts_with(prefix) && word.len() >= 16)
        || word.starts_with("akia") && word.len() == 20
        || word.starts_with("eyj") && word.split('.').count() == 3
}

fn url_with_userinfo(lower: &str) -> bool {
    ["http://", "https://"].into_iter().any(|scheme| {
        lower.match_indices(scheme).any(|(start, _)| {
            lower[start + scheme.len()..]
                .split(['/', ' ', '\n', '\r'])
                .next()
                .is_some_and(|authority| authority.contains('@'))
        })
    })
}

fn credential_flag(lower: &str) -> bool {
    lower
        .split(|c: char| c.is_whitespace() || c == '"' || c == '\'')
        .filter_map(|word| word.strip_prefix("--"))
        .any(|flag| credential_key(flag.split('=').next().unwrap_or("")))
}

fn credential_assignment_at(lower: &str, index: usize, ch: char) -> bool {
    if lower[index + 1..].trim().is_empty() {
        return false;
    }
    let key = lower[..index]
        .rsplit(|c: char| {
            !(c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '"' || c == '\'')
        })
        .next()
        .unwrap_or("")
        .trim_matches(['"', '\''])
        .trim_start_matches('-');
    if credential_key(key) {
        return true;
    }
    let query_key = lower[..index].rsplit(['?', '&']).next().unwrap_or("");
    ch == '=' && matches!(query_key, "signature" | "sig")
}

fn credential_text(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if lower.contains("private key-----") || lower.contains("bearer ") || lower.contains("basic ") {
        return true;
    }
    lower
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.'))
        .any(token_word)
        || url_with_userinfo(&lower)
        || credential_flag(&lower)
        || lower
            .char_indices()
            .filter(|&(_, ch)| ch == '=' || ch == ':')
            .any(|(index, ch)| credential_assignment_at(&lower, index, ch))
}

fn credential_value(value: &Value) -> bool {
    match value {
        Value::Object(fields) => fields
            .iter()
            .any(|(key, value)| credential_key(key) || credential_value(value)),
        Value::Array(items) => items.iter().any(credential_value),
        Value::String(value) => credential_text(value),
        _ => false,
    }
}

fn credential_store(path: &Path) -> bool {
    path.components().any(|part| {
        let name = part.as_os_str().to_string_lossy().to_ascii_lowercase();
        matches!(
            name.as_str(),
            ".ssh"
                | ".gnupg"
                | ".docker"
                | ".npmrc"
                | ".pypirc"
                | ".netrc"
                | ".git-credentials"
                | ".yarnrc"
                | ".yarnrc.yml"
                | "auth.json"
                | "auth.yml"
                | "auth.yaml"
                | "credentials"
                | ".credentials"
                | "credentials.json"
                | "credentials.yml"
                | "credentials.yaml"
                | "id_rsa"
                | "id_dsa"
                | "id_ecdsa"
                | "id_ed25519"
                | "token"
                | "hosts.yml"
        ) || name == ".env"
            || name.starts_with(".env.")
            || ["pem", "key", "p12", "pfx", "kdbx"]
                .into_iter()
                .any(|extension| {
                    Path::new(name.as_str())
                        .extension()
                        .is_some_and(|value| value == extension)
                })
    })
}

#[cfg(unix)]
fn safe_file_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn safe_file_mode(metadata: &fs::Metadata) -> u32 {
    if metadata.permissions().readonly() {
        0o444
    } else {
        0o666
    }
}

fn apply_asset_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o777))
            .map_err(|e| e.to_string())
    }
    #[cfg(not(unix))]
    {
        let mut permissions = fs::metadata(path).map_err(|e| e.to_string())?.permissions();
        permissions.set_readonly(mode & 0o222 == 0);
        fs::set_permissions(path, permissions).map_err(|e| e.to_string())
    }
}

fn inspect_file(path: &Path) -> Result<(u64, String, u32)> {
    no_symlinks(path)?;
    if credential_store(path) || credential_text(&path.to_string_lossy()) {
        return Err("Credential-store files cannot be included in setups".into());
    }
    let mut file = fs::File::open(path).map_err(|e| e.to_string())?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() {
        return Err("Setup source must be a regular file".into());
    }
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    let mut carry = Vec::new();
    let mut size = 0;
    loop {
        let count = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
        size += count as u64;
        carry.extend_from_slice(&buffer[..count]);
        if credential_text(&String::from_utf8_lossy(&carry)) {
            return Err("Credential-bearing setup file cannot be saved".into());
        }
        let keep = carry.len().saturating_sub(8192);
        carry.drain(..keep);
    }
    Ok((
        size,
        format!("{:x}", hash.finalize()),
        safe_file_mode(&metadata),
    ))
}

fn asset_json(asset: &ConfigAsset) -> Result<Value> {
    match &asset.file {
        Some(file) => {
            let mut reader = std::io::BufReader::new(
                fs::File::open(&file.source_path).map_err(|e| e.to_string())?,
            );
            if reader
                .fill_buf()
                .map_err(|e| e.to_string())?
                .starts_with(&[0xef, 0xbb, 0xbf])
            {
                reader.consume(3);
            }
            serde_json::from_reader(reader).map_err(|_| "Invalid setup JSON asset".into())
        }
        None => serde_json::from_str(
            asset
                .content
                .strip_prefix('\u{feff}')
                .unwrap_or(&asset.content),
        )
        .map_err(|_| "Invalid setup JSON asset".into()),
    }
}

const MCP_DEPENDENCY_ERROR: &str = "MCP relative dependencies must exist inside preserved setup assets; move support files under skills/ or rules/ before saving";

fn normalize_mcp_path(base: &str, value: &str) -> Result<String> {
    let mut parts = Vec::new();
    for part in base.split('/').chain(value.split('/')) {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Err(MCP_DEPENDENCY_ERROR.into());
                }
            }
            part => parts.push(part),
        }
    }
    Ok(parts.join("/"))
}

fn absolute_mcp_path(value: &str) -> bool {
    value.starts_with('/') || value.starts_with('\\') || value.as_bytes().get(1) == Some(&b':')
}

fn check_mcp_dependency(cwd: &str, value: &str, assets: &[ConfigAsset]) -> Result<()> {
    let value = value.replace('\\', "/");
    let target = normalize_mcp_path(cwd, &value)?;
    if !assets.iter().any(|asset| asset.path == target) {
        return Err(MCP_DEPENDENCY_ERROR.into());
    }
    Ok(())
}

fn mcp_server_cwd(server: &Value, assets: &[ConfigAsset]) -> Result<String> {
    let cwd = match server.get("cwd") {
        None => ".",
        Some(value) => value.as_str().ok_or(MCP_DEPENDENCY_ERROR)?,
    }
    .replace('\\', "/");
    if absolute_mcp_path(&cwd) || cwd.chars().any(char::is_control) {
        return Err(MCP_DEPENDENCY_ERROR.into());
    }
    let cwd = normalize_mcp_path("", &cwd)?;
    if !cwd.is_empty()
        && !assets
            .iter()
            .any(|asset| asset.path.starts_with(&format!("{cwd}/")))
    {
        return Err(MCP_DEPENDENCY_ERROR.into());
    }
    Ok(cwd)
}

fn validate_mcp_args(server: &Value, cwd: &str, assets: &[ConfigAsset]) -> Result<()> {
    let Some(args) = server.get("args").and_then(Value::as_array) else {
        return Ok(());
    };
    if args.iter().any(|arg| !arg.is_string()) {
        return Err("MCP arguments must be strings".into());
    }
    for arg in args.iter().filter_map(Value::as_str) {
        if [
            "./", "../", ".\\", "..\\", "skills/", "skills\\", "rules/", "rules\\",
        ]
        .into_iter()
        .any(|prefix| arg.starts_with(prefix))
        {
            check_mcp_dependency(cwd, arg, assets)?;
        }
    }
    Ok(())
}

fn validate_mcp_server(server: &Value, assets: &[ConfigAsset]) -> Result<()> {
    if !server.is_object()
        || !server.get("command").is_some_and(Value::is_string)
            && !server.get("url").is_some_and(Value::is_string)
    {
        return Err("MCP server requires a command or URL".into());
    }
    if server
        .get("command")
        .is_some_and(|command| command.as_str().is_none_or(|value| value.trim().is_empty()))
    {
        return Err("Invalid MCP command".into());
    }
    if server.get("args").is_some_and(|args| !args.is_array()) {
        return Err("MCP arguments must be a list".into());
    }
    let cwd = mcp_server_cwd(server, assets)?;
    if let Some(command) = server.get("command").and_then(Value::as_str) {
        if !absolute_mcp_path(command) && command.contains(['/', '\\']) {
            check_mcp_dependency(&cwd, command, assets)?;
        }
    }
    validate_mcp_args(server, &cwd, assets)
}

fn validate_mcp_dependencies(value: &Value, assets: &[ConfigAsset]) -> Result<()> {
    let servers = value
        .get("mcpServers")
        .and_then(Value::as_object)
        .ok_or("MCP asset requires a mcpServers mapping")?;
    for server in servers.values() {
        validate_mcp_server(server, assets)?;
    }
    Ok(())
}

fn validate_preview_settings(preview: &ConfigPreview, name: &str) -> Result<()> {
    if name.trim().is_empty()
        || name.chars().count() > 120
        || name.chars().any(char::is_control)
        || credential_text(name)
    {
        return Err("Enter a setup name (1–120 characters)".into());
    }
    let settings: Value =
        serde_yaml_ng::from_str(&preview.yaml).map_err(|_| "Invalid setup YAML".to_string())?;
    if !settings.is_object() || credential_value(&settings) {
        return Err("Setup settings must be a credential-free mapping".into());
    }
    if settings
        .get("skills")
        .and_then(|skills| skills.get("customDirectories"))
        .is_some()
    {
        return Err("External skill directories must be copied into reviewed setup assets".into());
    }
    if settings.as_object().is_some_and(serde_json::Map::is_empty) && preview.assets.is_empty() {
        return Err("Setup needs portable settings or assets".into());
    }
    Ok(())
}

fn asset_integrity(asset: &ConfigAsset) -> Result<(u64, String, u32)> {
    let Some(file) = &asset.file else {
        if asset.content.contains('\0') || credential_text(&asset.content) {
            return Err("Credential-bearing or invalid inline setup asset".into());
        }
        return Ok((
            asset.content.len() as u64,
            format!("{:x}", Sha256::digest(asset.content.as_bytes())),
            asset.mode.unwrap_or(0o644) & 0o777,
        ));
    };
    if !asset.content.is_empty() || !file.source_path.is_absolute() {
        return Err("Invalid setup file reference".into());
    }
    let actual = inspect_file(&file.source_path)?;
    let mode = asset.mode.map(|mode| mode & 0o777).unwrap_or(actual.2);
    if actual.0 != file.size || actual.1 != file.sha256 || cfg!(unix) && actual.2 != mode {
        return Err("Setup file changed since preview".into());
    }
    Ok((actual.0, actual.1, mode))
}

fn validate_json_asset(asset: &ConfigAsset, assets: &[ConfigAsset]) -> Result<()> {
    let value = asset_json(asset)?;
    if credential_value(&value) {
        return Err("Credential-bearing setup asset fields cannot be saved".into());
    }
    if asset.path == "mcp.json" {
        validate_mcp_dependencies(&value, assets)?;
    }
    Ok(())
}

fn validate_preview(preview: &ConfigPreview, name: &str) -> Result<Vec<AssetMetadata>> {
    validate_preview_settings(preview, name)?;
    let mut seen = std::collections::HashSet::new();
    let mut metadata = Vec::with_capacity(preview.assets.len());
    for asset in &preview.assets {
        validate_asset_path(&asset.path)?;
        if !seen.insert(asset.path.to_lowercase())
            || credential_store(Path::new(&asset.path))
            || credential_text(&asset.path)
        {
            return Err("Duplicate or credential-bearing setup asset path".into());
        }
        let (size, sha256, mode) = asset_integrity(asset)?;
        if asset.path.to_ascii_lowercase().ends_with(".json") {
            validate_json_asset(asset, &preview.assets)?;
        }
        metadata.push(AssetMetadata {
            path: asset.path.clone(),
            size,
            sha256,
            mode,
        });
    }
    Ok(metadata)
}

fn content_digest(yaml: &str, assets: &[AssetMetadata]) -> Result<String> {
    let assets: Vec<(&str, u64, &str, u32)> = assets
        .iter()
        .map(|a| (a.path.as_str(), a.size, a.sha256.as_str(), a.mode))
        .collect();
    let mut hash = Sha256::new();
    serde_json::to_writer(&mut hash, &(yaml, assets)).map_err(|e| e.to_string())?;
    Ok(format!("{:x}", hash.finalize()))
}

pub fn materialize(root: &Path) -> Result<()> {
    fs::create_dir_all(root).map_err(|e| e.to_string())?;
    for (name, source) in [
        (
            "omp-config.mjs",
            include_str!("../../bridge/omp-config.mjs"),
        ),
        (
            "config-bundle.mjs",
            include_str!("../../bridge/config-bundle.mjs"),
        ),
        (
            "omp-config-policy.mjs",
            include_str!("../../bridge/omp-config-policy.mjs"),
        ),
        (
            "omp-utility.mjs",
            include_str!("../../bridge/omp-utility.mjs"),
        ),
        (
            "omp-visualization-guard.mjs",
            include_str!("../../bridge/omp-visualization-guard.mjs"),
        ),
        (
            "local-ai-models.mjs",
            include_str!("../../bridge/local-ai-models.mjs"),
        ),
    ] {
        let file = root.join(name);
        if fs::read_to_string(&file).ok().as_deref() != Some(source) {
            fs::write(file, source).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let command = Command::new(program);
    #[cfg(windows)]
    let mut command = command;
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

pub fn check_sdk_version(sdk: &Path) -> Result<()> {
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
    Ok(())
}

pub fn native_request(root: &Path, home: &Path, mut request: Value) -> Result<Value> {
    let sdk = sdk_dir(home)?;
    check_sdk_version(&sdk)?;
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

pub(crate) struct ConfigDirectory(pub(crate) PathBuf);
impl ConfigDirectory {
    pub(crate) fn create(root: &Path) -> Result<Self> {
        let directory = root.join(format!(".config-session-{}", uuid::Uuid::new_v4()));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&directory)
            .map_err(|error| error.to_string())?;
        Ok(Self(directory))
    }
}
impl Drop for ConfigDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub struct UtilityConfiguration {
    pub sdk_cli: PathBuf,
    pub generator: PathBuf,
    pub bun: std::ffi::OsString,
    pub configs: Vec<PathBuf>,
    pub model: Option<String>,
    pub settings_snapshot: Option<PathBuf>,
    pub(crate) _directory: Option<ConfigDirectory>,
}

pub enum UtilityMode {
    Render,
    Generate,
}

fn utility_same_context(
    parent: Option<&Path>,
    children: &[crate::session::Subagent],
    selected: Option<&Path>,
) -> bool {
    let Some(path) = selected else {
        return true;
    };
    if parent == Some(path) || children.iter().any(|child| child.path == path) {
        return true;
    }
    parent.is_some_and(|parent| {
        crate::session::same_path(path, parent)
            || crate::session::header(path).is_some_and(|(header, _)| {
                header
                    .get("parentSession")
                    .and_then(Value::as_str)
                    .is_some_and(|linked| crate::session::same_path(Path::new(linked), parent))
            })
    })
}

pub fn prepare_utility(
    registry: &crate::instance::Registry,
    id: &str,
    model: Option<&str>,
    session_path: Option<&Path>,
    mode: UtilityMode,
) -> Result<UtilityConfiguration> {
    let source = registry.inspect(id, crate::instance::Origin::Human)?;
    let requested = model.filter(|model| !model.trim().is_empty());
    let same_context = utility_same_context(
        source.session_path.as_deref(),
        &source.subagents,
        session_path,
    );
    let authority = source.config_state.as_ref().filter(|state| {
        same_context && requested.is_none_or(|model| state.model.as_deref() == Some(model))
    });
    if let Some(error) = authority.and_then(|state| state.error.as_deref()) {
        return Err(error.to_owned());
    }
    let sdk_cli = sdk_dir(&registry.home)?.join("src/cli.ts");
    if !sdk_cli.is_file() {
        return Err(
            "OMP compatibility: the installed SDK has no exported native source CLI".into(),
        );
    }
    materialize(registry.config_root())?;
    let directory = ConfigDirectory::create(registry.config_root())?;
    let selected_model = requested
        .or_else(|| authority.and_then(|state| state.model.as_deref()))
        .or_else(|| {
            session_path
                .is_none()
                .then_some(source.model.as_str())
                .filter(|model| !model.is_empty())
        });
    let saved_session = session_path.or(source.session_path.as_deref());
    let configs = authority.map_or(source.configs.as_slice(), |state| {
        state.config_files.as_slice()
    });
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct PreparedUtility {
        config_files: Vec<PathBuf>,
        model: Option<String>,
        slot_manifest: UtilityManifest,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct UtilityManifest {
        settings_snapshot: PathBuf,
    }
    let prepared: PreparedUtility = serde_json::from_value(native_request(
        registry.config_root(), &registry.home,
        json!({
            "action": "prepare_session", "directory": directory.0, "configRoot": registry.config_root(),
            "cwd": source.project_path, "profile": source.profile, "configFiles": configs,
            "configDefault": source.config_default, "configSelection": source.config_selection,
            "model": selected_model, "sessionPath": saved_session, "utility": true,
            "renderOnly": matches!(mode, UtilityMode::Render),
            "authoritativeConfigFiles": authority.is_some(),
        }),
    )?).map_err(|error| format!("Invalid native utility configuration: {error}"))?;
    let snapshot = prepared.slot_manifest.settings_snapshot;
    if !snapshot.is_absolute() {
        return Err("Native utility settings snapshot must be inside its owned capsule".into());
    }
    no_symlinks(&snapshot)?;
    let owner = fs::canonicalize(&directory.0).map_err(|error| error.to_string())?;
    let snapshot = fs::canonicalize(snapshot).map_err(|error| error.to_string())?;
    if !snapshot.starts_with(owner) || !snapshot.is_file() {
        return Err("Native utility settings snapshot escaped its owned capsule".into());
    }
    Ok(UtilityConfiguration {
        sdk_cli,
        generator: registry.config_root().join("omp-utility.mjs"),
        bun: crate::toolchain::bun_program(),
        configs: prepared.config_files,
        model: prepared.model,
        settings_snapshot: Some(snapshot),
        _directory: Some(directory),
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedConfig {
    pub path: PathBuf,
    pub modified_ms: u64,
    pub name: String,
    pub requires_execution_review: bool,
    pub executable_review: bool,
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
        let loaded = Self::secure_snapshot_root(&root).and_then(|()| {
            match fs::read(root.join("global-config.json")) {
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
            }
        });
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
    fn create_private_directory(path: &Path, recursive: bool) -> Result<()> {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(recursive);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(path).map_err(|e| e.to_string())
    }

    fn secure_snapshot_root(root: &Path) -> Result<()> {
        let snapshots = root.join("global-configs");
        match fs::symlink_metadata(&snapshots) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.to_string()),
            Ok(metadata) => {
                if !metadata.is_dir() {
                    return Err("App-owned snapshot root must be a real directory".into());
                }
                no_symlinks(&snapshots)?;
                apply_asset_mode(&snapshots, 0o700)
            }
        }
    }

    fn validate_path(root: &Path, path: &Path) -> Result<()> {
        if !path.is_absolute() || !path.starts_with(root.join("global-configs")) {
            return Err("Global config pointer is outside app-owned snapshots".into());
        }
        Self::secure_snapshot_root(root)?;
        no_symlinks(path)?;
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
                    state.path.as_ref().and_then(|path| {
                        self.metadata(path)
                            .and_then(|metadata| {
                                if metadata.requires_execution_review && !metadata.executable_review
                                {
                                    Err("Default setup contains unapproved executable assets"
                                        .into())
                                } else {
                                    Ok(())
                                }
                            })
                            .err()
                    })
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
        self.publish_setup(
            &ConfigPreview {
                yaml: yaml.into(),
                model_roles: serde_json::Map::new(),
                omitted: Vec::new(),
                name: default_setup_name(),
                assets: Vec::new(),
                requires_execution_review: false,
            },
            &default_setup_name(),
            true,
            false,
        )
    }

    pub fn publish_setup(
        &self,
        preview: &ConfigPreview,
        name: &str,
        activate: bool,
        approve_execution: bool,
    ) -> Result<PathBuf> {
        let asset_metadata = validate_preview(preview, name)?;
        let requires_execution_review = preview.requires_execution_review
            || execution_sensitive(&preview.assets)
            || asset_metadata.iter().any(|asset| asset.mode & 0o111 != 0);
        if activate && requires_execution_review && !approve_execution {
            return Err("Review and approve executable setup assets before activation".into());
        }
        let mut state = self.state.lock().map_err(|e| e.to_string())?;
        let snapshots = self.root.join("global-configs");
        Self::create_private_directory(&snapshots, true)?;
        Self::secure_snapshot_root(&self.root)?;
        let folder = snapshots.join(uuid::Uuid::new_v4().to_string());
        Self::create_private_directory(&folder, false)?;
        let result = (|| {
            let metadata = SetupMetadata {
                version: 1,
                name: name.trim().into(),
                requires_execution_review,
                executable_review: approve_execution,
                assets: preview.assets.iter().map(|a| a.path.clone()).collect(),
                content_sha256: Some(content_digest(&preview.yaml, &asset_metadata)?),
                asset_metadata,
            };
            Self::write_bundle(&folder, &preview.yaml, &preview.assets, &metadata)?;
            let path = Self::normalize(&folder.join("config.yml"))?;
            Self::validate_path(&self.root, &path)?;
            if activate {
                self.write_pointer(&mut state, &path)?;
            }
            Ok(path)
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(folder);
        }
        result
    }

    fn write_bundle(
        folder: &Path,
        yaml: &str,
        assets: &[ConfigAsset],
        metadata: &SetupMetadata,
    ) -> Result<()> {
        fs::create_dir(folder.join("assets")).map_err(|e| e.to_string())?;
        for (asset, expected) in assets.iter().zip(&metadata.asset_metadata) {
            let file = folder.join("assets").join(&asset.path);
            fs::create_dir_all(file.parent().ok_or("Invalid asset parent")?)
                .map_err(|e| e.to_string())?;
            let mut output = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&file)
                .map_err(|e| e.to_string())?;
            if let Some(source) = &asset.file {
                no_symlinks(&source.source_path)?;
                drop(output);
                fs::copy(&source.source_path, &file).map_err(|e| e.to_string())?;
            } else {
                output
                    .write_all(asset.content.as_bytes())
                    .map_err(|e| e.to_string())?;
            }
            apply_asset_mode(&file, expected.mode)?;
            let (size, sha256, mode) = inspect_file(&file)?;
            if size != expected.size
                || sha256 != expected.sha256
                || cfg!(unix) && mode != expected.mode
            {
                return Err("Setup source changed while copying".into());
            }
        }
        fs::write(
            folder.join("setup.json"),
            serde_json::to_vec_pretty(metadata).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        fs::write(folder.join("config.yml"), yaml).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Validated app-owned metadata; legacy settings-only snapshots have no executable assets.
    pub fn metadata(&self, path: &Path) -> Result<SetupMetadata> {
        Self::validate_path(&self.root, path)?;
        if path.file_name().and_then(|p| p.to_str()) != Some("config.yml") {
            return Err("Setup path must select its config.yml".into());
        }
        let folder = path.parent().ok_or("Missing setup directory")?;
        let file = folder.join("setup.json");
        let metadata = match fs::symlink_metadata(&file) {
            Ok(_) => {
                no_symlinks(&file)?;
                let bytes = fs::read(file).map_err(|e| e.to_string())?;
                serde_json::from_slice::<SetupMetadata>(
                    bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes),
                )
                .map_err(|_| "Invalid setup metadata".to_string())?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if folder.join("assets").exists() {
                    return Err("Setup metadata is missing".into());
                }
                SetupMetadata {
                    version: 1,
                    name: default_setup_name(),
                    requires_execution_review: false,
                    executable_review: false,
                    assets: Vec::new(),
                    content_sha256: None,
                    asset_metadata: Vec::new(),
                }
            }
            Err(error) => return Err(error.to_string()),
        };
        if metadata.version != 1 {
            return Err("Unsupported setup version".into());
        }
        let preview = self.read_snapshot(path, &metadata)?;
        let actual = validate_preview(&preview, &metadata.name)?;
        // Named snapshots bind review consent to their exact settings and asset bytes.
        if actual != metadata.asset_metadata
            || metadata.content_sha256.as_deref().is_some_and(|expected| {
                content_digest(&preview.yaml, &actual).as_deref() != Ok(expected)
            })
            || (metadata.content_sha256.is_none() && folder.join("setup.json").exists())
        {
            return Err("Setup content changed since it was saved and reviewed".into());
        }
        if execution_sensitive(&preview.assets) && !metadata.requires_execution_review {
            return Err("Setup executable metadata is inconsistent".into());
        }
        Ok(metadata)
    }

    fn read_snapshot(&self, path: &Path, metadata: &SetupMetadata) -> Result<ConfigPreview> {
        let folder = path.parent().ok_or("Missing setup directory")?;
        if metadata.assets.len() != metadata.asset_metadata.len() {
            return Err("Invalid setup asset integrity metadata".into());
        }
        let mut assets = Vec::with_capacity(metadata.assets.len());
        for (relative, expected) in metadata.assets.iter().zip(&metadata.asset_metadata) {
            validate_asset_path(relative)?;
            if relative != &expected.path || expected.mode > 0o777 {
                return Err("Invalid setup asset integrity metadata".into());
            }
            assets.push(ConfigAsset {
                path: relative.clone(),
                content: String::new(),
                mode: Some(expected.mode),
                file: Some(ConfigAssetFile {
                    source_path: folder.join("assets").join(relative),
                    size: expected.size,
                    sha256: expected.sha256.clone(),
                }),
            });
        }
        no_symlinks(path)?;
        let yaml =
            fs::read_to_string(path).map_err(|_| "Setup config must be UTF-8 text".to_string())?;
        Ok(ConfigPreview {
            yaml,
            model_roles: serde_json::Map::new(),
            omitted: Vec::new(),
            name: metadata.name.clone(),
            assets,
            requires_execution_review: metadata.requires_execution_review,
        })
    }

    pub fn export_setup(&self, path: &Path, destination: &Path) -> Result<()> {
        let _guard = self.state.lock().map_err(|e| e.to_string())?;
        let metadata = self.metadata(path)?;
        let preview = self.read_snapshot(path, &metadata)?;
        let parent = destination
            .parent()
            .ok_or("Choose an export parent directory")?;
        no_symlinks(parent)?;
        fs::create_dir(destination)
            .map_err(|e| format!("Export needs a new destination folder: {e}"))?;
        // Consent is local to a saved snapshot, never portable across an import.
        let exported = SetupMetadata {
            executable_review: false,
            content_sha256: Some(content_digest(&preview.yaml, &metadata.asset_metadata)?),
            ..metadata
        };
        let result = Self::write_bundle(destination, &preview.yaml, &preview.assets, &exported);
        if result.is_err() {
            let _ = fs::remove_dir_all(destination);
        }
        result
    }

    pub fn assignments(&self) -> Result<Vec<ModelConfigAssignment>> {
        let file = self.root.join("model-configs.json");
        match fs::symlink_metadata(&file) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.to_string()),
            Ok(_) => no_symlinks(&file)?,
        }
        let bytes = fs::read(&file).map_err(|e| e.to_string())?;
        let rows: Vec<ModelConfigAssignment> =
            serde_json::from_slice(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes))
                .map_err(|_| "Invalid model assignments".to_string())?;
        self.validate_assignments(&rows)?;
        Ok(rows)
    }

    fn validate_assignments(&self, rows: &[ModelConfigAssignment]) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        let mut validated_paths = std::collections::HashSet::new();
        for row in rows {
            let (provider, model) = row
                .model
                .split_once('/')
                .ok_or("Use a resolved provider/model selector")?;
            if provider.is_empty()
                || model.is_empty()
                || provider.starts_with('@')
                || row
                    .model
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control())
                || !seen.insert(&row.model)
            {
                return Err("Invalid or duplicate resolved model assignment".into());
            }
            if validated_paths.insert(&row.config_path) {
                let metadata = self.metadata(&row.config_path)?;
                if metadata.requires_execution_review && !metadata.executable_review {
                    return Err(
                        "Review and approve executable setup assets before assigning models".into(),
                    );
                }
            }
        }
        Ok(())
    }

    pub fn save_assignments(&self, rows: Vec<ModelConfigAssignment>) -> Result<()> {
        let _guard = self.state.lock().map_err(|e| e.to_string())?;
        self.validate_assignments(&rows)?;
        fs::create_dir_all(&self.root).map_err(|e| e.to_string())?;
        no_symlinks(&self.root)?;
        let temporary = self
            .root
            .join(format!("model-configs-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(|e| e.to_string())?;
            file.write_all(&serde_json::to_vec_pretty(&rows).map_err(|e| e.to_string())?)
                .and_then(|_| file.sync_all())
                .map_err(|e| e.to_string())?;
            fs::rename(&temporary, self.root.join("model-configs.json")).map_err(|e| e.to_string())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
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
                no_symlinks(&entry.path().join("config.yml")).ok()?;
                let path = Self::normalize(&entry.path().join("config.yml")).ok()?;
                let metadata = self.metadata(&path).ok()?;
                Self::validate_path(&self.root, &path).ok()?;
                let modified = fs::metadata(&path).ok()?.modified().ok()?;
                let modified_ms = modified
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                Some(SavedConfig {
                    path,
                    modified_ms,
                    name: metadata.name,
                    requires_execution_review: metadata.requires_execution_review,
                    executable_review: metadata.executable_review,
                })
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
        let metadata = self.metadata(path)?;
        if metadata.requires_execution_review && !metadata.executable_review {
            return Err("Review and approve executable setup assets before activation".into());
        }
        let path = Self::normalize(path)?;
        Self::validate_path(&self.root, &path)?;
        self.write_pointer(&mut state, &path)?;
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preview_with(assets: Vec<ConfigAsset>) -> ConfigPreview {
        ConfigPreview {
            yaml: "modelRoles:\n  default: ollama/local\n".into(),
            model_roles: serde_json::Map::new(),
            omitted: Vec::new(),
            name: "Daily local coding".into(),
            assets,
            requires_execution_review: false,
        }
    }

    #[test]
    fn utility_context_matches_current_and_owned_children_but_not_unrelated_archives() {
        let root =
            std::env::temp_dir().join(format!("omp-utility-context-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let parent = root.join("parent.jsonl");
        fs::write(&parent, "{\"type\":\"session\"}\n").unwrap();
        let child = root.join("child.jsonl");
        fs::write(
            &child,
            json!({"type":"session","parentSession":parent}).to_string(),
        )
        .unwrap();
        let unrelated = root.join("archive.jsonl");
        fs::write(
            &unrelated,
            json!({"type":"session","parentSession":root.join("other.jsonl")}).to_string(),
        )
        .unwrap();
        let children = [crate::session::Subagent {
            id: "child".into(),
            name: "Child".into(),
            task: None,
            status: "completed".into(),
            agent: None,
            path: child.clone(),
        }];
        assert!(utility_same_context(Some(&parent), &children, None));
        assert!(utility_same_context(
            Some(&parent),
            &children,
            Some(&parent)
        ));
        assert!(utility_same_context(Some(&parent), &children, Some(&child)));
        assert!(utility_same_context(Some(&parent), &[], Some(&child)));
        assert!(!utility_same_context(
            Some(&parent),
            &children,
            Some(&unrelated)
        ));
        assert!(!utility_same_context(None, &[], Some(&unrelated)));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn utility_capsule_lives_until_its_configuration_owner_is_dropped() {
        let root = std::env::temp_dir().join(format!("omp-utility-owner-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let directory = ConfigDirectory::create(&root).unwrap();
        let path = directory.0.clone();
        let utility = UtilityConfiguration {
            sdk_cli: PathBuf::new(),
            generator: PathBuf::new(),
            bun: std::ffi::OsString::new(),
            configs: Vec::new(),
            model: None,
            settings_snapshot: None,
            _directory: Some(directory),
        };
        assert!(path.is_dir());
        #[cfg(unix)]
        assert_eq!(safe_file_mode(&fs::metadata(&path).unwrap()), 0o700);
        drop(utility);
        assert!(!path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_and_staging_capsules_are_private_even_with_world_writable_assets() {
        let root = std::env::temp_dir().join(format!("omp-private-{}", uuid::Uuid::new_v4()));
        let snapshots = root.join("global-configs");
        fs::create_dir_all(&snapshots).unwrap();
        apply_asset_mode(&snapshots, 0o777).unwrap();
        let store = GlobalConfigStore::load(root.clone());
        assert_eq!(safe_file_mode(&fs::metadata(&snapshots).unwrap()), 0o700);
        let preview = preview_with(vec![ConfigAsset {
            path: "skills/demo/data.txt".into(),
            content: "safe data\n".into(),
            file: None,
            mode: Some(0o777),
        }]);
        let saved = store
            .publish_setup(&preview, "Private capsule", false, true)
            .unwrap();
        assert_eq!(
            safe_file_mode(&fs::metadata(saved.parent().unwrap()).unwrap()),
            0o700
        );
        assert_eq!(
            safe_file_mode(
                &fs::metadata(saved.parent().unwrap().join("assets/skills/demo/data.txt")).unwrap()
            ),
            0o777
        );
        let staging = snapshots.join("private-staging");
        GlobalConfigStore::create_private_directory(&staging, false).unwrap();
        assert_eq!(safe_file_mode(&fs::metadata(staging).unwrap()), 0o700);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn binary_and_large_support_files_are_copied_losslessly_and_stale_sources_never_activate() {
        let root = std::env::temp_dir().join(format!("omp-binary-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let source = root.join("model.bin");
        fs::write(&source, vec![0x80; 9 * 1024 * 1024]).unwrap();
        let (size, sha256, mode) = inspect_file(&source).unwrap();
        let preview = preview_with(vec![ConfigAsset {
            path: "skills/demo/model.bin".into(),
            content: String::new(),
            mode: Some(mode),
            file: Some(ConfigAssetFile {
                source_path: source.clone(),
                size,
                sha256: sha256.clone(),
            }),
        }]);
        let store = GlobalConfigStore::load(root.clone());
        let saved = store
            .publish_setup(&preview, "Binary skill", false, true)
            .unwrap();
        assert_eq!(
            inspect_file(&saved.parent().unwrap().join("assets/skills/demo/model.bin")).unwrap(),
            (size, sha256.clone(), mode)
        );
        let destination = root.join("portable");
        store.export_setup(&saved, &destination).unwrap();
        assert_eq!(
            inspect_file(&destination.join("assets/skills/demo/model.bin")).unwrap(),
            (size, sha256, mode)
        );
        fs::write(&source, b"changed").unwrap();
        assert!(store
            .publish_setup(&preview, "Stale source", true, true)
            .is_err());
        assert!(store.current_path().unwrap().is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mcp_relative_support_files_must_be_present_inside_the_bundle() {
        let root = std::env::temp_dir().join(format!("omp-mcp-paths-{}", uuid::Uuid::new_v4()));
        let store = GlobalConfigStore::load(root.clone());
        let mcp = |command: &str| ConfigAsset {
            path: "mcp.json".into(),
            content: json!({"mcpServers":{"demo":{"command":command}}}).to_string(),
            file: None,
            mode: None,
        };
        assert!(store
            .publish_setup(
                &preview_with(vec![mcp("./server.sh")]),
                "Missing support",
                false,
                false
            )
            .is_err());
        assert!(store
            .publish_setup(
                &preview_with(vec![mcp("../outside.sh")]),
                "Outside support",
                false,
                false
            )
            .is_err());
        let preview = preview_with(vec![
            mcp("./skills/demo/server.sh"),
            ConfigAsset {
                path: "skills/demo/server.sh".into(),
                content: "#!/bin/sh\nexit 0\n".into(),
                file: None,
                mode: Some(0o4755),
            },
        ]);
        let saved = store
            .publish_setup(&preview, "Portable MCP", false, true)
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let script = saved.parent().unwrap().join("assets/skills/demo/server.sh");
            assert_eq!(safe_file_mode(&fs::metadata(&script).unwrap()), 0o755);
            assert_eq!(
                fs::metadata(&script).unwrap().permissions().mode() & 0o7000,
                0
            );
            let destination = root.join("portable");
            store.export_setup(&saved, &destination).unwrap();
            assert_eq!(
                safe_file_mode(
                    &fs::metadata(destination.join("assets/skills/demo/server.sh")).unwrap()
                ),
                0o755
            );
            apply_asset_mode(&script, 0o644).unwrap();
            assert!(store.metadata(&saved).is_err());
            assert!(store
                .save_assignments(vec![ModelConfigAssignment {
                    model: "ollama/local".into(),
                    config_path: saved
                }])
                .is_err());
        }
        let executable_doc = preview_with(vec![ConfigAsset {
            path: "rules/helper.md".into(),
            content: "#!/bin/sh\nexit 0\n".into(),
            file: None,
            mode: Some(0o755),
        }]);
        assert!(store
            .publish_setup(&executable_doc, "Executable text", true, false)
            .is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn named_bundles_preserve_assets_and_assignments_without_changing_default() {
        let root = std::env::temp_dir().join(format!("omp-bundles-{}", uuid::Uuid::new_v4()));
        let store = GlobalConfigStore::load(root.clone());
        let original = store.publish("theme: dark\n").unwrap();
        let preview = preview_with(vec![
            ConfigAsset {
                path: "AGENTS.md".into(),
                content: "\u{feff}# Æble 🍎\r\nUse local models.\r\n".into(),
                file: None,
                mode: None,
            },
            ConfigAsset {
                path: "rules/style.md".into(),
                content: "# Style\nKeep it small.\n".into(),
                file: None,
                mode: None,
            },
        ]);
        let saved = store
            .publish_setup(&preview, "Daily local coding", false, false)
            .unwrap();
        assert_eq!(store.current_path().unwrap(), Some(original.clone()));
        assert_eq!(store.metadata(&saved).unwrap().name, "Daily local coding");
        let destination = root.join("exported");
        store.export_setup(&saved, &destination).unwrap();
        assert_eq!(
            fs::read_to_string(destination.join("config.yml")).unwrap(),
            preview.yaml
        );
        assert_eq!(
            fs::read_to_string(destination.join("assets/AGENTS.md")).unwrap(),
            preview.assets[0].content
        );
        let metadata: SetupMetadata =
            serde_json::from_slice(&fs::read(destination.join("setup.json")).unwrap()).unwrap();
        assert_eq!(metadata.name, "Daily local coding");
        assert!(!metadata.executable_review);
        assert!(store.export_setup(&saved, &destination).is_err());
        store
            .save_assignments(vec![ModelConfigAssignment {
                model: "ollama/local".into(),
                config_path: saved.clone(),
            }])
            .unwrap();
        let restarted = GlobalConfigStore::load(root.clone());
        assert_eq!(restarted.assignments().unwrap()[0].config_path, saved);
        assert_eq!(restarted.current_path().unwrap(), Some(original));
        assert!(restarted
            .list()
            .iter()
            .any(|item| item.name == "Daily local coding"));
        store.save_assignments(Vec::new()).unwrap();
        assert!(GlobalConfigStore::load(root.clone())
            .assignments()
            .unwrap()
            .is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn execution_consent_is_explicit_immutable_and_not_exported() {
        let root = std::env::temp_dir().join(format!("omp-review-{}", uuid::Uuid::new_v4()));
        let store = GlobalConfigStore::load(root.clone());
        let preview = preview_with(vec![ConfigAsset {
            path: "mcp.json".into(),
            content: format!(
                "\u{feff}{}\r\n",
                r#"{"mcpServers":{"demo":{"command":"node","args":["server.mjs"]}}}"#
            ),
            file: None,
            mode: None,
        }]);
        assert!(store
            .publish_setup(&preview, "MCP setup", true, false)
            .is_err());
        assert!(store.current_path().unwrap().is_none());
        let unapproved = store
            .publish_setup(&preview, "Review later", false, false)
            .unwrap();
        assert!(
            store
                .metadata(&unapproved)
                .unwrap()
                .requires_execution_review
        );
        assert!(store.activate(&unapproved).is_err());
        assert!(store
            .save_assignments(vec![ModelConfigAssignment {
                model: "ollama/local".into(),
                config_path: unapproved
            }])
            .is_err());
        let approved = store
            .publish_setup(&preview, "Reviewed MCP", true, true)
            .unwrap();
        assert!(store.metadata(&approved).unwrap().executable_review);
        assert_eq!(
            GlobalConfigStore::load(root.clone())
                .current_path()
                .unwrap(),
            Some(approved.clone())
        );
        let destination = root.join("portable");
        store.export_setup(&approved, &destination).unwrap();
        let exported: SetupMetadata =
            serde_json::from_slice(&fs::read(destination.join("setup.json")).unwrap()).unwrap();
        assert!(exported.requires_execution_review);
        assert!(!exported.executable_review);
        fs::write(
            approved.parent().unwrap().join("assets/mcp.json"),
            r#"{"mcpServers":{"demo":{"command":"changed-command"}}}"#,
        )
        .unwrap();
        assert!(store.activate(&approved).is_err());
        assert!(store
            .save_assignments(vec![ModelConfigAssignment {
                model: "ollama/local".into(),
                config_path: approved
            }])
            .is_err());
        assert!(GlobalConfigStore::load(root.clone())
            .current_path()
            .is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn hostile_assets_credentials_and_duplicate_assignments_are_rejected() {
        let root = std::env::temp_dir().join(format!("omp-hostile-{}", uuid::Uuid::new_v4()));
        let store = GlobalConfigStore::load(root.clone());
        for path in [
            "../AGENTS.md",
            "skills/../../secret",
            "/AGENTS.md",
            "skills/a\\b",
            "skills//a",
            "auth.json",
            "skills/a:stream",
        ] {
            assert!(store
                .publish_setup(
                    &preview_with(vec![ConfigAsset {
                        path: path.into(),
                        content: "safe".into(),
                        file: None,
                        mode: None
                    }]),
                    "Bad",
                    false,
                    false
                )
                .is_err());
        }
        for content in [
            "API_TOKEN=DO_NOT_EXPORT",
            "Authorization: Bearer DO_NOT_EXPORT",
            "-----BEGIN PRIVATE KEY-----",
            "https://user:DO_NOT_EXPORT@example.test",
            "https://example.test?auth=DO_NOT_EXPORT",
            "https://example.test?signature=DO_NOT_EXPORT",
            "https://example.test?sig=DO_NOT_EXPORT",
            "https://example.test?client_secret=DO_NOT_EXPORT",
        ] {
            assert!(store
                .publish_setup(
                    &preview_with(vec![ConfigAsset {
                        path: "skills/demo/run.sh".into(),
                        content: content.into(),
                        file: None,
                        mode: None
                    }]),
                    "Bad",
                    false,
                    false
                )
                .is_err());
        }
        let mut preview = preview_with(Vec::new());
        preview.yaml = "modelPresets:\n  demo:\n    apiKey: DO_NOT_EXPORT\n".into();
        assert!(store.publish_setup(&preview, "Bad", false, false).is_err());
        preview.yaml = "skills:\n  customDirectories: [/outside/unreviewed]\n".into();
        assert!(store
            .publish_setup(&preview, "Unreviewed paths", false, false)
            .is_err());
        let saved = store.publish("theme: dark\n").unwrap();
        let row = || ModelConfigAssignment {
            model: "ollama/local".into(),
            config_path: saved.clone(),
        };
        assert!(store.save_assignments(vec![row(), row()]).is_err());
        fs::write(root.join("model-configs.json"), b"not json").unwrap();
        assert!(GlobalConfigStore::load(root.clone()).assignments().is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_symlinks_and_manifest_traversal_are_not_exportable_or_activatable() {
        let root = std::env::temp_dir().join(format!("omp-links-{}", uuid::Uuid::new_v4()));
        let store = GlobalConfigStore::load(root.clone());
        let preview = preview_with(vec![ConfigAsset {
            path: "AGENTS.md".into(),
            content: "safe".into(),
            file: None,
            mode: None,
        }]);
        let saved = store.publish_setup(&preview, "Safe", false, false).unwrap();
        let asset = saved.parent().unwrap().join("assets/AGENTS.md");
        fs::remove_file(&asset).unwrap();
        fs::write(root.join("outside"), "DO_NOT_EXPORT").unwrap();
        std::os::unix::fs::symlink(root.join("outside"), &asset).unwrap();
        assert!(store.activate(&saved).is_err());
        assert!(store.export_setup(&saved, &root.join("portable")).is_err());
        let metadata = SetupMetadata {
            version: 1,
            name: "Bad".into(),
            requires_execution_review: false,
            executable_review: false,
            assets: vec!["../outside".into()],
            content_sha256: None,
            asset_metadata: Vec::new(),
        };
        fs::write(
            saved.parent().unwrap().join("setup.json"),
            serde_json::to_vec(&metadata).unwrap(),
        )
        .unwrap();
        assert!(store.metadata(&saved).is_err());
        let alias = root.join("global-configs/alias.yml");
        std::os::unix::fs::symlink(&saved, &alias).unwrap();
        assert!(store.activate(&alias).is_err());
        fs::remove_dir_all(root).unwrap();
    }
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
