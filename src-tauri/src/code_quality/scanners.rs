//! Pinned scanner recipes. Secrets enter the engine only as process environment, never host argv.
use super::job::{run_job, JobStep};
use super::runtime::*;
use super::snapshot::{self, Snapshot};
use super::types::*;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

const WORKSPACE: &str = "/tmp/omp-workspace";
const MANAGED: &str = "/tmp/omp-quality";
const CONFIG_LIMIT: u64 = 1024 * 1024;
const RUN_HELPER: &str = include_str!("../../../bridge/code-quality/scanner/run.sh");
const GRADLE_INIT: &str = include_str!("../../../bridge/code-quality/scanner/sonar.init.gradle");
const IMAGE_MANIFEST: &str = include_str!("../../../bridge/code-quality/images.lock.json");

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageLock {
    pub version: u32,
    pub sonarqube_community: String,
    pub postgres: String,
    pub scanner_cli: String,
    pub maven: String,
    pub gradle: String,
    pub dotnet_sdk: String,
    pub dotnet_sdks: BTreeMap<String, String>,
    pub dotnet_scanner_version: String,
}
fn immutable_image(image: &str) -> bool {
    image.rsplit_once("@sha256:").map(|(name, digest)| !name.is_empty() && name.contains(':') && digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()) && !name.chars().any(|c| c.is_whitespace() || c.is_control())).unwrap_or(false)
}
pub fn images() -> Result<&'static ImageLock> {
    static LOCK: LazyLock<Result<ImageLock>> = LazyLock::new(|| {
        let lock: ImageLock = serde_json::from_str(IMAGE_MANIFEST).map_err(|e| format!("Invalid shipped scanner image lock: {e}"))?;
        if lock.version != 1 { return Err("Unsupported scanner image lock version".into()); }
        for image in [&lock.sonarqube_community, &lock.postgres, &lock.scanner_cli, &lock.maven, &lock.gradle, &lock.dotnet_sdk].into_iter().chain(lock.dotnet_sdks.values()) {
            if !immutable_image(image) { return Err("Shipped analysis image is not pinned to a SHA-256 registry digest".into()); }
        }
        Ok(lock)
    });
    LOCK.as_ref().map_err(Clone::clone)
}
pub fn default_project_key(folder: &Path) -> String {
    let canonical = fs::canonicalize(folder).unwrap_or_else(|_| folder.to_path_buf());
    let text = canonical.to_string_lossy();
    // Windows folders compare case-insensitively with either separator; Unix folders are exact (`\` is a filename character).
    let path = if cfg!(windows) { text.replace('\\', "/").to_lowercase() } else { text.into_owned() };
    let hash = format!("{:x}", Sha256::digest(path.as_bytes())); format!("omp-{}", &hash[..20])
}
pub(crate) fn plain_path(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path).map_err(|e| format!("Cannot inspect {}: {e}", path.display()))?;
    if snapshot::is_link(&meta) { return Err(format!("Refusing linked/junction/reparse-point configuration: {}", path.display())); }
    Ok(())
}
pub(crate) fn read_config(path: &Path) -> Result<Option<String>> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("Cannot inspect build configuration {}: {e}", path.display())),
        Ok(meta) if !meta.is_file() || snapshot::is_link(&meta) => return Err(format!("Build configuration is not a regular file: {}", path.display())),
        _ => (),
    }
    plain_path(path)?;
    let mut file = File::open(path).map_err(|e| format!("Cannot read build configuration {}: {e}", path.display()))?;
    let mut text = String::new(); (&mut file).take(CONFIG_LIMIT + 1).read_to_string(&mut text).map_err(|e| format!("Cannot read UTF-8 build configuration {}: {e}", path.display()))?;
    if text.len() as u64 > CONFIG_LIMIT { return Err(format!("Build configuration exceeds 1 MiB: {}", path.display())); }
    Ok(Some(text))
}
fn unescape_property(raw: &str) -> Result<String> {
    let mut output = String::new(); let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' { output.push(c); continue; }
        match chars.next() {
            Some('u') => { let digits: String = chars.by_ref().take(4).collect(); let value = u32::from_str_radix(&digits, 16).ok().and_then(char::from_u32).ok_or("Invalid Unicode escape in sonar-project.properties")?; output.push(value); },
            Some('t') => output.push('\t'), Some('n') => output.push('\n'), Some('r') => output.push('\r'), Some(c) => output.push(c), None => (),
        }
    } Ok(output)
}
fn explicit_project_key(folder: &Path) -> Result<Option<String>> {
    let Some(text) = read_config(&folder.join("sonar-project.properties"))? else { return Ok(None); };
    let mut logical = String::new(); let mut key = None;
    for line in text.lines() {
        let trimmed = line.trim(); if logical.is_empty() && (trimmed.starts_with('#') || trimmed.starts_with('!')) { continue; }
        logical.push_str(trimmed);
        if logical.chars().rev().take_while(|c| *c == '\\').count() % 2 == 1 { logical.pop(); continue; }
        let mut escaped = false; let mut split = None;
        for (i, c) in logical.char_indices() {
            if !escaped && (c == '=' || c == ':' || c.is_whitespace()) { split = Some(i); break; }
            escaped = !escaped && c == '\\';
        }
        if let Some(i) = split {
            let name = unescape_property(logical[..i].trim())?;
            if name == "sonar.projectKey" {
                let value = logical[i..].trim_start().trim_start_matches(['=', ':']).trim_start();
                key = Some(unescape_property(value)?);
            }
        }
        logical.clear();
    }
    Ok(key.filter(|k| !k.is_empty()))
}
fn valid_key(key: &str) -> bool { !key.is_empty() && key.len() <= 400 && key.bytes().all(|c| c.is_ascii_alphanumeric() || b"-_.:".contains(&c)) && key.bytes().any(|c| !c.is_ascii_digit()) }
fn entry_name(path: &str) -> Result<String> {
    if path.contains('\\') || path.contains(':') || path.chars().any(|c| c.is_control()) || path.is_empty() || Path::new(path).components().any(|p| !matches!(p, Component::Normal(_))) { return Err("Select a relative project entry file inside the source folder".into()); }
    Ok(path.to_string())
}
/// Folder text without Windows' verbatim `\\?\` prefix, so it equals the folder the rest of the service stores.
pub(crate) fn display_folder(path: &Path) -> String {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") { format!(r"\\{rest}") }
    else { text.strip_prefix(r"\\?\").unwrap_or(&text).to_string() }
}
pub fn detect(folder: &Path, saved: Option<QualityProject>) -> Result<QualityProjectDetection> {
    plain_path(folder)?;
    let folder = fs::canonicalize(folder).map_err(|e| format!("Cannot resolve project folder: {e}"))?;
    if !folder.is_dir() { return Err("Project folder is not a directory".into()); }
    let key = explicit_project_key(&folder)?;
    let mut names = Vec::new();
    for child in fs::read_dir(&folder).map_err(|e| format!("Cannot inspect root build manifests: {e}"))? {
        let child = child.map_err(|e| format!("Cannot enumerate root build manifests: {e}"))?;
        let meta = fs::symlink_metadata(child.path()).map_err(|e| format!("Cannot inspect root build manifest: {e}"))?;
        if meta.is_file() && !snapshot::is_link(&meta) { names.push(child.file_name().to_string_lossy().into_owned()); }
    }
    names.sort();
    let mut families = Vec::new();
    if names.iter().any(|n| n == "pom.xml") { families.push(Scanner::Maven); }
    if names.iter().any(|n| matches!(n.as_str(), "build.gradle" | "build.gradle.kts" | "settings.gradle" | "settings.gradle.kts")) { families.push(Scanner::Gradle); }
    let entry_files: Vec<String> = names.iter().filter(|n| [".sln", ".slnx", ".csproj", ".vbproj"].iter().any(|ext| n.ends_with(ext))).cloned().collect();
    let has_dotnet_source = names.iter().any(|n| n.ends_with(".cs") || n.ends_with(".vb"));
    if !entry_files.is_empty() || has_dotnet_source { families.push(Scanner::Dotnet); }
    let ambiguous = families.len() > 1 || (families.contains(&Scanner::Dotnet) && entry_files.len() != 1);
    let had_saved = saved.is_some();
    let has = |name: &str| names.iter().any(|n| n == name);
    let mut project = saved.unwrap_or_else(|| {
        let mut exclusions = Vec::new();
        if has("nx.json") { exclusions.push(".nx/**".to_string()); }
        if has("angular.json") { exclusions.push(".angular/**".to_string()); }
        QualityProject { folder: display_folder(&folder), project_key: key.clone().unwrap_or_else(|| default_project_key(&folder)), scanner: families.first().copied().unwrap_or(Scanner::Cli), entry_file: if entry_files.len() == 1 { Some(entry_files[0].clone()) } else { None }, image: None, build_args: Vec::new(), prepare_npm_dependencies: false, exclusions }
    });
    project.folder = display_folder(&folder);
    let selection_valid = if project.scanner == Scanner::Dotnet { project.entry_file.as_ref().map(|e| entry_files.contains(e)).unwrap_or(false) } else { project.scanner != Scanner::Cli || !families.contains(&Scanner::Dotnet) };
    let needs_selection = (ambiguous && !had_saved) || !selection_valid;
    let mut warnings = Vec::new();
    if needs_selection { warnings.push("Select a scanner/build family and, for .NET, one entry file before preparing analysis.".into()); }
    if has_dotnet_source && entry_files.is_empty() { warnings.push("C#/VB requires a compatible .NET project; the CLI scanner does not analyze C#/VB.".into()); }
    let mut labels: Vec<String> = Vec::new();
    if has("package.json") { labels.push("Node.js / TypeScript (package.json)".into()); }
    if has("angular.json") { labels.push("Angular (angular.json)".into()); }
    if has("nx.json") { labels.push("Nx workspace (nx.json)".into()); }
    if has("pyproject.toml") || has("requirements.txt") { labels.push("Python".into()); }
    if has("go.mod") { labels.push("Go".into()); }
    if has("Cargo.toml") { labels.push("Rust".into()); }
    if has("pom.xml") { labels.push("Maven (pom.xml)".into()); }
    if has("build.gradle") || has("build.gradle.kts") { labels.push("Gradle".into()); }
    if !entry_files.is_empty() { labels.push(".NET".into()); }
    if families.is_empty() { labels.push("No build tool found — analyzed directly".into()); }
    Ok(QualityProjectDetection { project, families, entry_files, explicit_key: key.is_some(), needs_selection, warnings, labels })
}
fn gradle_declares_plugin(folder: &Path) -> Result<bool> {
    for name in ["build.gradle", "build.gradle.kts", "settings.gradle", "settings.gradle.kts", "gradle/libs.versions.toml"] {
        if let Some(text) = read_config(&folder.join(name))? {
            if text.lines().filter(|l| !l.trim_start().starts_with("//") && !l.trim_start().starts_with('#')).any(|l| l.contains("org.sonarqube") || l.contains("sonarqube-gradle-plugin") || l.contains("libs.plugins.sonarqube") || l.contains("libs.plugins.sonar")) { return Ok(true); }
        }
    } Ok(false)
}
fn sdk_image(folder: &Path, lock: &ImageLock) -> Result<String> {
    let Some(text) = read_config(&folder.join("global.json"))? else { return Ok(lock.dotnet_sdk.clone()); };
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("Invalid global.json: {e}"))?;
    let Some(version) = value.pointer("/sdk/version").and_then(|v| v.as_str()) else { return Ok(lock.dotnet_sdk.clone()); };
    let major = version.split('.').next().ok_or("Invalid SDK version in global.json")?;
    lock.dotnet_sdks.get(version).or_else(|| lock.dotnet_sdks.get(major)).cloned().ok_or_else(|| format!("No shipped pinned Linux SDK for global.json {version}; select an explicitly approved compatible image pinned by digest. Windows-only .NET Framework builds are not supported by Linux SDK containers."))
}
fn image_for(project: &QualityProject, folder: &Path) -> Result<String> {
    if let Some(image) = &project.image {
        if !immutable_image(image) { return Err("Custom analysis images must use a published tag@sha256:digest so the approved recipe cannot change silently".into()); }
        return Ok(image.clone());
    }
    let lock = images()?;
    Ok(match project.scanner { Scanner::Cli => lock.scanner_cli.clone(), Scanner::Maven => lock.maven.clone(), Scanner::Gradle => lock.gradle.clone(), Scanner::Dotnet => sdk_image(folder, lock)? })
}
fn config_hashes(folder: &Path) -> Result<Vec<(String, String)>> {
    let mut hashes = Vec::new();
    let mut walk = ignore::WalkBuilder::new(folder); walk.hidden(false).follow_links(false).git_global(false).require_git(false);
    walk.filter_entry(|e| e.depth() == 0 || !matches!(e.file_name().to_str().unwrap_or(""), ".git" | "node_modules" | "target" | "build" | "dist" | ".venv" | "venv") && fs::symlink_metadata(e.path()).map(|m| !snapshot::is_link(&m)).unwrap_or(true));
    for entry in walk.build() {
        let entry = entry.map_err(|e| format!("Cannot enumerate build configuration for recipe consent: {e}"))?;
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) { continue; }
        let name = entry.file_name().to_string_lossy();
        let relative = entry.path().strip_prefix(folder).map_err(|_| "Build configuration escaped project")?.to_string_lossy().replace('\\', "/");
        let relevant = matches!(&*name, "pom.xml" | "global.json" | "sonar-project.properties" | "gradlew" | "gradlew.bat" | "gradle-wrapper.jar" | "gradle-wrapper.properties" | "package.json" | "package-lock.json" | "yarn.lock" | "pnpm-lock.yaml" | "nuget.config" | "NuGet.Config") || name.ends_with(".gradle") || name.ends_with(".gradle.kts") || name.ends_with(".csproj") || name.ends_with(".vbproj") || name.ends_with(".sln") || name.ends_with(".slnx") || name.ends_with(".props") || name.ends_with(".targets") || relative.starts_with(".mvn/") || relative.starts_with("gradle/");
        if relevant {
            plain_path(entry.path())?;
            let file = File::open(entry.path()).map_err(|e| format!("Cannot read build file {relative}: {e}"))?;
            if file.metadata().map_err(|e| e.to_string())?.len() > 16 * CONFIG_LIMIT { return Err(format!("Build/configuration file exceeds 16 MiB: {relative}")); }
            let mut hash = Sha256::new(); let mut reader = file.take(16 * CONFIG_LIMIT + 1); let mut buf = [0u8; 65536]; let mut bytes = 0;
            loop { let n = reader.read(&mut buf).map_err(|e| format!("Cannot hash build file {relative}: {e}"))?; if n == 0 { break; } bytes += n as u64; if bytes > 16 * CONFIG_LIMIT { return Err(format!("Build file changed beyond size limit: {relative}")); } hash.update(&buf[..n]); }
            hashes.push((relative, format!("{:x}", hash.finalize())));
        }
    }
    hashes.sort(); Ok(hashes)
}
fn validate_project(project: &QualityProject, folder: &Path) -> Result<()> {
    plain_path(folder)?;
    if !valid_key(&project.project_key) { return Err("Sonar project key must use letters, digits, '-', '_', '.', ':' and cannot be all digits".into()); }
    for arg in &project.build_args {
        let lower = arg.to_ascii_lowercase();
        if arg.contains(['\n', '\r', '\0']) || ["sonar.token", "sonar.login", "sonar.password", "sonar.host.url", "sonar.projectkey", "sonar.projectname", "sonar.projectbasedir", "sonar.working.directory", "sonar.scm.disabled"].iter().any(|reserved| lower.contains(reserved)) { return Err("Build arguments cannot contain credentials or override managed Sonar connection/project/workspace settings".into()); }
    }
    if project.scanner == Scanner::Cli {
        let mut walk = ignore::WalkBuilder::new(folder); walk.hidden(false).follow_links(false).require_git(false);
        walk.filter_entry(|e| e.depth() == 0 || !matches!(e.file_name().to_str().unwrap_or(""), ".git" | "node_modules" | "target" | "dist" | "build" | ".venv" | "venv") && fs::symlink_metadata(e.path()).map(|m| !snapshot::is_link(&m)).unwrap_or(true));
        for e in walk.build() { let e = e.map_err(|e| e.to_string())?; if e.file_type().map(|t| t.is_file()).unwrap_or(false) && matches!(e.path().extension().and_then(|s| s.to_str()), Some("cs" | "vb" | "csproj" | "vbproj")) { return Err("C#/VB analysis requires the .NET scanner and an explicitly selected build entry point; CLI cannot analyze these languages".into()); } }
    }
    if project.scanner == Scanner::Dotnet {
        let entry = entry_name(project.entry_file.as_deref().ok_or("Choose a .NET solution/project entry file before preparing analysis")?)?;
        if ![".sln", ".slnx", ".csproj", ".vbproj"].iter().any(|ext| entry.ends_with(ext)) { return Err("Choose a .sln, .slnx, .csproj or .vbproj entry file".into()); }
        plain_path(&folder.join(entry))?;
    }
    if project.prepare_npm_dependencies && read_config(&folder.join("package-lock.json"))?.is_none() { return Err("Prepare npm dependencies requires package-lock.json; for another package manager select an approved custom analysis image with the needed dependencies".into()); }
    Ok(())
}
pub fn prepare_scan(project: &QualityProject, folder: &Path, server_origin: &str) -> Result<QualityScanPreparation> {
    validate_project(project, folder)?;
    let server = reqwest::Url::parse(server_origin).map_err(|_| "Invalid configured Sonar server URL")?;
    if !server.username().is_empty() || server.password().is_some() || server.query().is_some() || server.fragment().is_some() || !matches!(server.scheme(), "http" | "https") { return Err("Sonar server must be a credential-free configured HTTP(S) URL without a query/fragment".into()); }
    let image = image_for(project, folder)?;
    let mut selected = project.clone(); selected.image = Some(image.clone());
    let args = serde_json::to_string(&project.build_args).map_err(|e| e.to_string())?;
    let mut recipe = vec![format!("Image: {image}"), format!("Copy ignored-filtered snapshot into stopped container at {WORKSPACE}; never mount the working tree. uid/gid 1000:1000; --cpus 2 --memory 4g; no engine socket or host home mounts."), format!("Source analysis is sent to {server_origin}. Scanner uses the configured Community private network alias or Enterprise HTTPS connection."), format!("Managed settings: projectKey={}; host=<configured server>; projectBaseDir={WORKSPACE}; SCM disabled; SONAR_TOKEN=<backend secret environment, never host argv/stored recipe>.", project.project_key)];
    if project.prepare_npm_dependencies { recipe.push("In the disposable snapshot only: npm ci --ignore-scripts (network access for dependency downloads; npm must exist in the selected image).".into()); }
    match project.scanner {
        Scanner::Cli => recipe.push(format!("sonar-scanner; argument array {args}; preserve sonar-project.properties scope; force managed host/project/working directory; no project build. report: {WORKSPACE}/.scannerwork/report-task.txt")),
        Scanner::Maven => recipe.push(format!("mvn -B -DskipTests -DskipITs clean verify org.sonarsource.scanner.maven:sonar-maven-plugin:5.8.0.7211:sonar; additional build argument array {args}; managed properties appended last. Compile source/tests but run neither Surefire nor Failsafe tests. report: {WORKSPACE}/.scannerwork/report-task.txt")),
        Scanner::Gradle => {
            let wrapper = read_config(&folder.join("gradlew"))?.is_some(); let existing = gradle_declares_plugin(folder)?;
            recipe.push(format!("{} --no-daemon {}classes testClasses sonar; additional build argument array {args}; {}. No test tasks by default. report: {WORKSPACE}/.scannerwork/report-task.txt", if wrapper { "./gradlew (execute project wrapper)" } else { "gradle" }, if existing { "" } else { "--init-script /tmp/omp-quality/sonar.init.gradle " }, if existing { "use project-declared Sonar plugin version" } else { "snapshot-only managed init applies Sonar plugin 7.5.0.8588" }));
        },
        Scanner::Dotnet => {
            recipe.push(format!("Prepare derivative of selected pinned SDK: dotnet tool install --tool-path /opt/omp-sonarscanner --version {} dotnet-sonarscanner; fixed build context only, no source/credentials.", images()?.dotnet_scanner_version));
            recipe.push(format!("In ONE disposable container/workspace: dotnet-sonarscanner begin /k:<managed-key> /d:sonar.host.url=<managed-host> /d:sonar.token=<in-container SONAR_TOKEN>; dotnet build {} --no-incremental --disable-build-servers; argument array {args}; dotnet-sonarscanner end /d:sonar.token=<in-container SONAR_TOKEN>. Token expanded only by fixed in-container helper, never host argv. report: {WORKSPACE}/.sonarqube/out/.sonar/report-task.txt", project.entry_file.as_deref().unwrap_or("")));
        },
    }
    let mut warnings = vec!["Coverage is unavailable unless the project supplies a supported coverage report; missing coverage is not 0% or 100%. Server profiles/gate and supported languages determine analysis coverage.".into()];
    let requires_consent = project.scanner != Scanner::Cli || project.prepare_npm_dependencies;
    if requires_consent { warnings.push("This recipe downloads dependencies/plugins and executes project build/wrapper/lint configuration in a disposable copy with network access. Approved Sonar builds receive this connection's analysis credential. No host source/home/engine socket is mounted; no fixes are applied to the original project.".into()); }
    if project.scanner == Scanner::Dotnet { warnings.push("Linux SDK containers cannot build Windows-only .NET Framework targets. The pinned SDK follows global.json's major; an exact SDK/rollForward requirement not present in that image will fail visibly—select an approved compatible digest-pinned image rather than falling back to CLI.".into()); }
    let hashes = config_hashes(folder)?;
    let serialized = serde_json::to_vec(&(&selected, &recipe, &hashes, RUN_HELPER, GRADLE_INIT, IMAGE_MANIFEST)).map_err(|e| e.to_string())?;
    let recipe_hash = format!("{:x}", Sha256::digest(serialized));
    Ok(QualityScanPreparation { project: selected, recipe, recipe_hash, requires_consent, warnings })
}

pub struct SonarRunTarget { pub scanner_url: String, pub network: Option<String>, pub project_key: String, pub project_name: String }
pub struct ScannerRun { pub exit_code: i64, pub report_task: Option<String>, pub log_tail: String, pub timed_out: bool, pub cancelled: bool }
fn write_asset(path: &Path, content: &str) -> Result<()> { fs::write(path, content).map_err(|e| format!("Cannot materialize managed scanner asset: {e}")) }
fn bootstrap_tar(path: &Path) -> Result<()> {
    let mut tar = tar::Builder::new(File::create(path).map_err(|e| e.to_string())?);
    for name in ["omp-workspace", "omp-quality", "omp-home"] {
        let mut h = tar::Header::new_gnu(); h.set_entry_type(tar::EntryType::Directory); h.set_size(0); h.set_uid(1000); h.set_gid(1000); h.set_mode(0o700); h.set_mtime(0); h.set_cksum();
        tar.append_data(&mut h, name, std::io::empty()).map_err(|e| e.to_string())?;
    }
    tar.finish().map_err(|e| e.to_string())
}
fn scanner_spec(image: String, project: &QualityProject, target: &SonarRunTarget, token: &str, owner: &str, job_id: &str, wrapper: bool, init: bool) -> ContainerSpec {
    ContainerSpec {
        name: format!("omp-q-{}-scan-{}", owner.chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect::<String>(), job_id.chars().filter(|c| c.is_ascii_alphanumeric()).take(12).collect::<String>()),
        image,
        labels: vec![(OWNER_LABEL.into(), owner.into()), (ROLE_LABEL.into(), ROLE_SCAN.into())],
        env: vec![("SONAR_HOST_URL".into(), target.scanner_url.clone()), ("OMP_PROJECT_KEY".into(), target.project_key.clone()), ("OMP_PROJECT_NAME".into(), target.project_name.chars().filter(|c| !c.is_control()).take(200).collect()), ("OMP_SCANNER".into(), match project.scanner { Scanner::Cli => "cli", Scanner::Maven => "maven", Scanner::Gradle => "gradle", Scanner::Dotnet => "dotnet" }.into()), ("OMP_ENTRY_FILE".into(), project.entry_file.clone().unwrap_or_default()), ("OMP_GRADLE_WRAPPER".into(), wrapper.to_string()), ("OMP_GRADLE_INIT".into(), init.to_string()), ("OMP_NPM_PREPARE".into(), project.prepare_npm_dependencies.to_string())],
        secret_env: vec![("SONAR_TOKEN".into(), token.into())],
        network: target.network.clone(), cpus: Some(2.0), memory: Some("4g".into()), user: Some("1000:1000".into()), workdir: Some("/tmp".into()), entrypoint: Some(vec!["/bin/sh".into(), format!("{MANAGED}/run.sh")]), command: project.build_args.clone(), ..ContainerSpec::default()
    }
}
struct DisposablePath { path: PathBuf, directory: bool }
impl Drop for DisposablePath {
    fn drop(&mut self) {
        if self.directory { let _ = fs::remove_dir_all(&self.path); } else { let _ = fs::remove_file(&self.path); }
    }
}
fn snapshot_metadata(source: &Snapshot, stage: &Path, project: &QualityProject, ctl: &Ctl) -> Result<()> {
    let mut archive = tar::Archive::new(File::open(&source.tar_path).map_err(|e| e.to_string())?);
    for entry in archive.entries().map_err(|e| e.to_string())? {
        if ctl.cancelled() { return Err("Scan cancelled while reading captured build metadata".into()); }
        let mut entry = entry.map_err(|e| e.to_string())?;
        if !entry.header().entry_type().is_file() { continue; }
        let path = entry.path().map_err(|e| e.to_string())?.into_owned();
        if project.scanner == Scanner::Cli && matches!(path.extension().and_then(|s| s.to_str()), Some("cs" | "vb" | "csproj" | "vbproj")) {
            return Err("C#/VB requires the .NET scanner, not CLI".into());
        }
        let name = path.to_string_lossy().replace('\\', "/");
        let needed = matches!(name.as_str(), "global.json" | "gradlew" | "build.gradle" | "build.gradle.kts" | "settings.gradle" | "settings.gradle.kts" | "gradle/libs.versions.toml")
            || (name == "package-lock.json" && project.prepare_npm_dependencies)
            || project.entry_file.as_deref() == Some(name.as_str());
        if needed {
            if entry.size() > CONFIG_LIMIT { return Err(format!("Captured scanner metadata exceeds 1 MiB: {name}")); }
            let destination = stage.join(&path);
            if let Some(parent) = destination.parent() { fs::create_dir_all(parent).map_err(|e| e.to_string())?; }
            let mut file = fs::OpenOptions::new().create_new(true).write(true).open(&destination).map_err(|e| e.to_string())?;
            std::io::copy(&mut entry, &mut file).map_err(|e| e.to_string())?;
            file.flush().map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}
pub fn run_scanner(engine: &dyn Engine, owner: &str, job_id: &str, source: &Snapshot, project: &QualityProject, target: &SonarRunTarget, token: &str, job_dir: &Path, on_container: &dyn Fn(&str), ctl: &Ctl) -> Result<ScannerRun> {
    if ctl.cancelled() { return Err("Scan cancelled before scanner preparation".into()); }
    if token.is_empty() { return Err("Save a Sonar user token before scanning".into()); }
    if target.project_key != project.project_key || !valid_key(&target.project_key) { return Err("Scanner target does not match the approved project key".into()); }
    snapshot::validate_tar(&source.tar_path, 1024 * 1024 * 1024)?;
    // Approved build metadata is read from the captured snapshot, never from a later working-tree edit.
    let stage = job_dir.join("scanner-source-metadata");
    fs::create_dir_all(&stage).map_err(|e| e.to_string())?;
    let _stage_cleanup = DisposablePath { path: stage.clone(), directory: true };
    snapshot_metadata(source, &stage, project, ctl)?;
    validate_project(project, &stage)?;
    let wrapper = project.scanner == Scanner::Gradle && read_config(&stage.join("gradlew"))?.is_some();
    let init = project.scanner == Scanner::Gradle && !gradle_declares_plugin(&stage)?;
    let mut image = image_for(project, &stage)?;
    if !engine.image_exists(&image, ctl)? { engine.pull(&image, ctl)?; }
    if project.scanner == Scanner::Dotnet {
        let context = job_dir.join("dotnet-image-context"); fs::create_dir_all(&context).map_err(|e| e.to_string())?;
        let dockerfile = format!("FROM {image}\nENV DOTNET_CLI_TELEMETRY_OPTOUT=1 DOTNET_NOLOGO=1\nRUN dotnet tool install --tool-path /opt/omp-sonarscanner --version {} dotnet-sonarscanner && /opt/omp-sonarscanner/dotnet-sonarscanner --version\n", images()?.dotnet_scanner_version);
        let digest = format!("{:x}", Sha256::digest(dockerfile.as_bytes()));
        image = format!("localhost/omp-quality-dotnet:{}", &digest[..24]);
        if !engine.image_exists(&image, ctl)? { write_asset(&context.join("Dockerfile"), &dockerfile)?; engine.build_image(&image, &context, ctl)?; }
    }
    if ctl.cancelled() { return Err("Scan cancelled during scanner preparation".into()); }
    let assets = job_dir.join("scanner-assets"); fs::create_dir_all(&assets).map_err(|e| e.to_string())?;
    // Git autocrlf must not turn the POSIX helper into CRLF.
    write_asset(&assets.join("run.sh"), &RUN_HELPER.replace("\r\n", "\n"))?; write_asset(&assets.join("sonar.init.gradle"), &GRADLE_INIT.replace("\r\n", "\n"))?;
    let asset_tar = job_dir.join("scanner-assets.tar"); snapshot::pack_dir_tar(&assets, &asset_tar)?;
    let bootstrap = job_dir.join("scanner-bootstrap.tar"); bootstrap_tar(&bootstrap)?;
    let report_tar = job_dir.join("scanner-report-task.tar");
    let report_source = match project.scanner { Scanner::Dotnet => format!("{WORKSPACE}/.sonarqube/out/.sonar/report-task.txt"), _ => format!("{WORKSPACE}/.scannerwork/report-task.txt") };
    let spec = scanner_spec(image, project, target, token, owner, job_id, wrapper, init);
    let safe_ctl = Ctl { cancel: ctl.cancel.clone(), log: { let sink = ctl.log.clone(); let secret = token.to_owned(); std::sync::Arc::new(move |line: &str| sink(&super::reports::redact(line, &[secret.as_str()]))) } };
    let result = run_job(engine, &JobStep { spec, copy_in: vec![("/tmp".into(), bootstrap), (WORKSPACE.into(), source.tar_path.clone()), (MANAGED.into(), asset_tar)], copy_out: vec![(report_source, report_tar.clone())], timeout_secs: 60 * 60 }, on_container, &safe_ctl).map_err(|e| super::reports::redact(&e, &[token]))?;
    let mut log_tail = super::reports::redact(&result.log_tail, &[token]);
    let report_task = if !result.cancelled && report_tar.is_file() {
        let report_dir = job_dir.join("scanner-report-task");
        let paths = snapshot::extract_tar_safely(&report_tar, &report_dir, CONFIG_LIMIT)?;
        let candidates: Vec<_> = paths.iter().filter(|p| p.file_name().and_then(|s| s.to_str()) == Some("report-task.txt") && report_dir.join(p).is_file()).collect();
        if candidates.len() != 1 { return Err("Scanner report archive must contain exactly one report-task.txt".into()); }
        let text = read_config(&report_dir.join(candidates[0]))?.ok_or("Scanner report-task.txt is missing")?;
        let redacted = super::reports::redact(&text, &[token]);
        // Even previews/raw report-tasks may not persist the managed credential.
        write_asset(&report_dir.join(candidates[0]), &redacted)?;
        fs::remove_file(&report_tar).map_err(|e| format!("Cannot remove unredacted scanner transfer archive: {e}"))?;
        Some(redacted)
    } else { if !result.cancelled { log_tail.push_str("\nScanner did not produce report-task.txt; submission is not a completed analysis."); } None };
    // The captured source archive + manifest remain the provenance; the metadata copy is removed on return.
    Ok(ScannerRun { exit_code: result.exit_code, report_task, log_tail, timed_out: result.timed_out, cancelled: result.cancelled })
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Temp(PathBuf);
    impl Temp { fn new() -> Self { let p = std::env::temp_dir().join(format!("omp scanners ø {}", uuid::Uuid::new_v4())); fs::create_dir_all(&p).unwrap(); Self(p) } }
    impl Drop for Temp { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }
    #[test]
    fn detects_manifest_families_ambiguity_and_explicit_property_key() {
        for (names, expected, needs) in [ (vec!["pom.xml"], vec![Scanner::Maven], false), (vec!["build.gradle.kts"], vec![Scanner::Gradle], false), (vec!["a.csproj"], vec![Scanner::Dotnet], false), (vec!["a.sln", "b.sln"], vec![Scanner::Dotnet], true), (vec!["pom.xml", "settings.gradle"], vec![Scanner::Maven, Scanner::Gradle], true), (vec!["plain.py"], vec![], false), (vec!["source.cs"], vec![Scanner::Dotnet], true) ] {
            let t = Temp::new(); for name in names { fs::write(t.0.join(name), "").unwrap(); }
            fs::write(t.0.join("sonar-project.properties"), "# comment\nsonar.projectKey = team\\:explicit\n").unwrap();
            let detection = detect(&t.0, None).unwrap(); assert_eq!(detection.families, expected); assert_eq!(detection.needs_selection, needs); assert_eq!(detection.project.project_key, "team:explicit"); assert!(detection.explicit_key);
        }
    }
    #[test]
    fn recipe_is_deterministic_and_config_changes_revoke_approval() {
        let t = Temp::new(); fs::write(t.0.join("pom.xml"), "<project/>").unwrap(); let p = detect(&t.0, None).unwrap().project;
        let a = prepare_scan(&p, &t.0, "https://sonar.example/team").unwrap(); let b = prepare_scan(&p, &t.0, "https://sonar.example/team").unwrap(); assert_eq!(a.recipe_hash, b.recipe_hash); assert!(a.requires_consent);
        assert!(a.recipe.iter().any(|r| r.contains("-DskipTests -DskipITs") && r.contains("5.8.0.7211")));
        fs::write(t.0.join("pom.xml"), "<project><properties/></project>").unwrap(); assert_ne!(a.recipe_hash, prepare_scan(&p, &t.0, "https://sonar.example/team").unwrap().recipe_hash);
    }
    #[test]
    fn pinned_images_and_secret_env_never_host_argv_or_recipe() {
        let lock = images().unwrap(); assert!(lock.scanner_cli.contains("12.2.0.4256_8.1.0@sha256:"));
        let t = Temp::new(); let p = detect(&t.0, None).unwrap().project; let token = "never-persist-token-abc123";
        let target = SonarRunTarget { scanner_url: "http://owned-sonar:9000".into(), network: Some("owned-network".into()), project_key: p.project_key.clone(), project_name: "test".into() };
        let spec = scanner_spec(lock.scanner_cli.clone(), &p, &target, token, "owner-12345", "job-12345", false, false);
        assert_eq!(spec.secret_env, vec![("SONAR_TOKEN".into(), token.into())]); assert!(!spec.command.join(" ").contains(token)); assert!(!format!("{:?}", spec.env).contains(token)); assert!(!spec.entrypoint.unwrap().join(" ").contains(token)); assert_eq!(spec.network, target.network);
        let prep = prepare_scan(&p, &t.0, "http://127.0.0.1:9000").unwrap(); assert!(!prep.recipe.join("\n").contains(token)); assert!(!prep.requires_consent);
    }
    #[test]
    fn cli_never_claims_csharp_coverage_and_gradle_respects_plugin() {
        let t = Temp::new(); fs::write(t.0.join("build.gradle"), "plugins { id 'org.sonarqube' version '6.0.1.5171' }").unwrap(); assert!(gradle_declares_plugin(&t.0).unwrap());
        let mut p = detect(&t.0, None).unwrap().project; p.scanner = Scanner::Cli; fs::write(t.0.join("code.cs"), "class X {}").unwrap(); assert!(prepare_scan(&p, &t.0, "https://sonar.example").is_err());
        fs::remove_file(t.0.join("code.cs")).unwrap(); assert!(prepare_scan(&p, &t.0, "https://sonar.example").is_ok());
        p.build_args = vec!["-Dsonar.token=forbidden".into()]; assert!(validate_project(&p, &t.0).is_err());
    }
    #[test]
    fn node_nx_angular_repo_uses_cli_without_consent_and_labels_what_was_found() {
        let t = Temp::new();
        for name in ["package.json", "nx.json", "angular.json"] { fs::write(t.0.join(name), "{}").unwrap(); }
        let detection = detect(&t.0, None).unwrap();
        assert_eq!(detection.project.scanner, Scanner::Cli);
        assert!(detection.families.is_empty());
        assert!(!detection.needs_selection);
        for label in ["Node.js / TypeScript (package.json)", "Angular (angular.json)", "Nx workspace (nx.json)", "No build tool found — analyzed directly"] {
            assert!(detection.labels.iter().any(|l| l == label), "missing label {label}: {:?}", detection.labels);
        }
        assert!(detection.project.exclusions.contains(&".nx/**".to_string()));
        assert!(detection.project.exclusions.contains(&".angular/**".to_string()));
        let prep = prepare_scan(&detection.project, &t.0, "https://sonar.example").unwrap();
        assert!(!prep.requires_consent);
        // A saved project keeps its own exclusions; defaults are only for fresh detection.
        let mut saved = detection.project.clone(); saved.exclusions.clear();
        assert!(detect(&t.0, Some(saved)).unwrap().project.exclusions.is_empty());
    }
    #[cfg(unix)]
    #[test]
    fn unix_folders_differing_only_in_case_or_backslash_get_distinct_default_keys() {
        let missing = |name: &str| Path::new("/omp-key-test-missing").join(name);
        let upper = default_project_key(&missing("A"));
        assert_eq!(upper, default_project_key(&missing("A")));
        assert_ne!(upper, default_project_key(&missing("a")));
        assert_ne!(default_project_key(&missing("a/b")), default_project_key(&missing("a\\b")));
    }
    #[cfg(windows)]
    #[test]
    fn windows_default_keys_ignore_case_and_separator_style() {
        assert_eq!(default_project_key(Path::new(r"C:\omp-missing\Repo")), default_project_key(Path::new("c:/omp-missing/repo")));
    }
}
