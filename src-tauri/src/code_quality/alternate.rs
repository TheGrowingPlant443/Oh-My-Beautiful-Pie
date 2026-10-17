//! Frozen, offline analysis pipeline. License admission precedes every network/cache mutation.
use super::types::*;
use super::runtime::*;
use super::{job, reports, snapshot};
use crate::dependency_security::{atomic_json, bounded_json};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

const MANIFEST: &str = include_str!("../../../bridge/code-quality/rules/selected-manifest.json");
const LICENSE: &str = include_str!("../../../bridge/code-quality/rules/LICENSE-opengrep-rules.txt");
const LOCK: &str = include_str!("../../../bridge/code-quality/tools.lock.json");
const PROFILE: &str = include_str!("../../../bridge/code-quality/megalinter.yml");
const OWN_RULES: &str = include_str!("../../../bridge/code-quality/rules/omp-quality.yml");
const MAX_REPORT: u64 = 64 * 1024 * 1024;
const MAX_DEPENDENCIES: u64 = 4 * 1024 * 1024 * 1024;
const ASSETS: &[(&str, &str)] = &[
    ("Dockerfile", include_str!("../../../bridge/code-quality/Dockerfile.alternate")),
    ("tools.lock.json", LOCK), ("megalinter.yml", PROFILE),
    ("run-alternate.py", include_str!("../../../bridge/code-quality/run-alternate.py")),
    ("validate-rules.mjs", include_str!("../../../bridge/code-quality/validate-rules.mjs")),
    ("rules/omp-quality.yml", OWN_RULES), ("rules/selected-manifest.json", MANIFEST),
    ("rules/LICENSE-opengrep-rules.txt", LICENSE),
    ("rules/fixtures/quality.js", include_str!("../../../bridge/code-quality/rules/fixtures/quality.js")),
    ("rules/fixtures/quality.ts", include_str!("../../../bridge/code-quality/rules/fixtures/quality.ts")),
    ("rules/fixtures/quality.py", include_str!("../../../bridge/code-quality/rules/fixtures/quality.py")),
    ("fallback/eslint.config.cjs", include_str!("../../../bridge/code-quality/fallback/eslint.config.cjs")),
    ("fallback/.ruff.toml", include_str!("../../../bridge/code-quality/fallback/.ruff.toml")),
    ("fallback/java-pmd-ruleset.xml", include_str!("../../../bridge/code-quality/fallback/java-pmd-ruleset.xml")),
    ("fallback/.hadolint.yaml", include_str!("../../../bridge/code-quality/fallback/.hadolint.yaml")),
    ("fallback/.jscpd.json", include_str!("../../../bridge/code-quality/fallback/.jscpd.json")),
    ("fallback/.bandit.yml", include_str!("../../../bridge/code-quality/fallback/.bandit.yml")),
    ("fallback/.shellcheckrc", include_str!("../../../bridge/code-quality/fallback/.shellcheckrc")),
    ("fallback/.checkov.yml", include_str!("../../../bridge/code-quality/fallback/.checkov.yml")),
];

pub struct AlternateRun<'a> {
    pub owner: &'a str,
    pub job_id: &'a str,
    pub snapshot_tar: &'a Path,
    pub project: &'a QualityProject,
    pub job_dir: &'a Path,
    pub config_root: &'a Path,
}

pub struct AlternateResult {
    pub tools: Vec<QualityToolOutcome>,
    pub findings: Vec<QualityFinding>,
    pub warnings: Vec<String>,
    pub rules_hash: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Receipt { version: u32, manifest_hash: String, license_hash: String, acknowledged_at: u64 }

fn hash(bytes: &[u8]) -> String { format!("{:x}", Sha256::digest(bytes)) }
fn io_error(action: &str, error: impl std::fmt::Display) -> String { format!("Code quality {action}: {error}") }
fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() { fs::create_dir_all(parent).map_err(|e| io_error("create asset directory", e))?; }
    fs::write(path, bytes).map_err(|e| io_error("write app-owned asset", e))
}
fn read_bounded(path: &Path, max: u64) -> Result<Vec<u8>> {
    let file = fs::File::open(path).map_err(|e| io_error("open report/cache", e))?;
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes).map_err(|e| io_error("read report/cache", e))?;
    if bytes.len() as u64 > max { return Err(format!("Code quality report/cache exceeds {max} bytes; narrow the project's exclusions.")); }
    Ok(bytes)
}
fn json(text: &str) -> Result<Value> { serde_json::from_str(text).map_err(|e| io_error("parse shipped metadata", e)) }
fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value.get(key).and_then(Value::as_str).ok_or_else(|| format!("Shipped code quality metadata is missing {key}."))
}
fn cancelled(ctl: &Ctl) -> Result<()> { if ctl.cancelled() { Err("Code quality scan cancelled.".into()) } else { Ok(()) } }

pub fn rules_state(config_root: &Path) -> RulesState {
    let manifest_hash = hash(MANIFEST.as_bytes());
    let license_hash = hash(LICENSE.as_bytes());
    let receipt = bounded_json::<Receipt>(&config_root.join("rules-consent.json"), 1024 * 1024).ok().flatten();
    let acknowledged = receipt.is_some_and(|r| r.version == 1 && r.manifest_hash == manifest_hash && r.license_hash == license_hash);
    RulesState { manifest_hash, license_hash, acknowledged, license_text: format!("opengrep/opengrep-rules — LGPL-2.1 + Commons Clause\nPinned provenance and per-language counts are in selected-manifest.json.\nNo commercial redistribution rights are asserted.\n\n{LICENSE}") }
}

pub fn acknowledge_rules(config_root: &Path, manifest_hash: &str, license_hash: &str) -> Result<()> {
    let state = rules_state(config_root);
    if state.manifest_hash != manifest_hash || state.license_hash != license_hash {
        return Err("Code quality rules or license changed; review the current rules license and acknowledge both current hashes.".into());
    }
    fs::create_dir_all(config_root).map_err(|e| io_error("create rules receipt directory", e))?;
    atomic_json(&config_root.join("rules-consent.json"), &Receipt { version: 1, manifest_hash: manifest_hash.into(), license_hash: license_hash.into(), acknowledged_at: now_millis() }, 1024 * 1024)
}

pub fn require_acknowledged(config_root: &Path) -> Result<()> {
    if rules_state(config_root).acknowledged { Ok(()) } else { Err(RULES_MESSAGE.into()) }
}

/// A changed managed asset changes the derived image identity, including fallback/config changes.
pub fn image_name() -> String {
    let mut digest = Sha256::new();
    for (name, contents) in ASSETS { digest.update(name.as_bytes()); digest.update([0]); digest.update(contents.as_bytes()); digest.update([0]); }
    let digest = format!("{:x}", digest.finalize());
    format!("localhost/omp-quality-alternate:{}", &digest[..24])
}

fn download_verified(url: &str, expected: &str, path: &Path, max: u64, ctl: &Ctl) -> Result<()> {
    cancelled(ctl)?;
    if path.exists() && hash(&read_bounded(path, max)?) == expected { return Ok(()); }
    if !url.starts_with("https://") { return Err("Pinned analyzer download must use HTTPS.".into()); }
    let client = reqwest::blocking::Client::builder().connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(20 * 60))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.url().scheme() != "https" || attempt.previous().len() >= 5 { attempt.stop() } else { attempt.follow() }
        })).build().map_err(|e| io_error("create download client", e))?;
    let mut response = client.get(url).header(reqwest::header::USER_AGENT, "OMP-Manager-code-quality").send()
        .and_then(reqwest::blocking::Response::error_for_status).map_err(|e| io_error("download pinned analyzer asset", e))?;
    if response.content_length().is_some_and(|size| size > max) { return Err("Pinned analyzer download exceeds its size limit.".into()); }
    let temp = path.with_extension(format!("download-{}", uuid::Uuid::new_v4()));
    if let Some(parent) = temp.parent() { fs::create_dir_all(parent).map_err(|e| io_error("create analyzer cache", e))?; }
    let result = (|| -> Result<()> {
        let mut output = fs::File::create(&temp).map_err(|e| io_error("create download", e))?;
        let mut total = 0u64;
        let mut digest = Sha256::new();
        let mut buffer = [0u8; 65536];
        loop {
            cancelled(ctl)?;
            let count = response.read(&mut buffer).map_err(|e| io_error("read analyzer download", e))?;
            if count == 0 { break; }
            total += count as u64;
            if total > max { return Err("Pinned analyzer download exceeds its size limit.".into()); }
            digest.update(&buffer[..count]);
            output.write_all(&buffer[..count]).map_err(|e| io_error("save analyzer download", e))?;
        }
        if format!("{:x}", digest.finalize()) != expected { return Err("Pinned analyzer SHA-256 mismatch; no analyzer image was built. Retry setup or review the shipped lock file.".into()); }
        output.sync_all().map_err(|e| io_error("flush analyzer download", e))?;
        drop(output);
        if path.exists() { fs::remove_file(path).map_err(|e| io_error("replace invalid cached download", e))?; }
        fs::rename(&temp, path).map_err(|e| io_error("commit verified download", e))
    })();
    if result.is_err() { let _ = fs::remove_file(&temp); }
    result
}

fn safe_asset_path(text: &str) -> Result<PathBuf> {
    let path = Path::new(text);
    if text.contains('\\') || text.contains(':') || path.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err("Unsafe path in frozen rules manifest/archive.".into());
    }
    Ok(path.to_path_buf())
}

fn validate_rule(bytes: &[u8], entry: &Value) -> Result<()> {
    if hash(bytes) != string(entry, "sha256")? { return Err(format!("Pinned rule content hash mismatch: {}.", string(entry, "path")?)); }
    let document: Value = serde_yaml_ng::from_slice(bytes).map_err(|e| io_error("parse pinned rule YAML", e))?;
    let rules = document.get("rules").and_then(Value::as_array).ok_or("Pinned rule file has no rules array.")?;
    if rules.len() != 1 { return Err("Pinned selected rule file must contain exactly its one selected rule.".into()); }
    let rule = &rules[0];
    let metadata = &rule["metadata"];
    if string(rule, "id")? != string(entry, "id")? || metadata["category"] != "security"
        || !metadata["confidence"].as_str().is_some_and(|v| v.eq_ignore_ascii_case("HIGH"))
        || !matches!(rule["severity"].as_str(), Some("ERROR" | "WARNING")) {
        return Err("Pinned rule violates the frozen high-confidence security selection.".into());
    }
    fn forbidden(value: &Value) -> bool {
        match value {
            Value::Object(fields) => fields.iter().any(|(key, value)| matches!(key.as_str(), "validators" | "validator" | "pattern-where-python" | "pro-required" | "pro-intrafile" | "pro-interfile" | "semgrep-internal" | "join" | "extract" | "extends") || forbidden(value)),
            Value::Array(items) => items.iter().any(forbidden),
            _ => false,
        }
    }
    if forbidden(rule) || matches!(rule["mode"].as_str(), Some("join" | "extract")) { return Err("Selected rule requires unsupported Pro, validator or remote/executable capabilities.".into()); }
    Ok(())
}

fn materialize_rules(archive: &Path, context: &Path, ctl: &Ctl) -> Result<()> {
    let manifest = json(MANIFEST)?;
    let entries = manifest["rules"].as_array().ok_or("Frozen rules manifest has no rule entries.")?;
    if entries.is_empty() { return Err("Frozen rules manifest is empty; cannot claim analysis coverage.".into()); }
    let wanted: BTreeMap<&str, &Value> = entries.iter().map(|e| Ok((string(e, "path")?, e))).collect::<Result<_>>()?;
    let file = fs::File::open(archive).map_err(|e| io_error("open frozen rules archive", e))?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let prefix = format!("opengrep-rules-{}/", string(&manifest, "commit")?);
    let mut found = HashSet::new();
    let mut total = 0u64;
    let mut license_found = false;
    for member in archive.entries().map_err(|e| io_error("read pinned rules archive", e))? {
        cancelled(ctl)?;
        let member = member.map_err(|e| io_error("read pinned rule member", e))?;
        let raw = member.path().map_err(|e| io_error("read pinned rule path", e))?.to_string_lossy().into_owned();
        let Some(relative) = raw.strip_prefix(&prefix) else { continue; };
        if relative.is_empty() { continue; }
        safe_asset_path(relative)?;
        if !member.header().entry_type().is_file() { continue; }
        total = total.checked_add(member.size()).ok_or("Rules archive size overflow.")?;
        if total > 128 * 1024 * 1024 { return Err("Pinned rules archive exceeds 128 MiB expanded.".into()); }
        if relative == "LICENSE" {
            let mut bytes = Vec::new(); member.take(1024 * 1024).read_to_end(&mut bytes).map_err(|e| io_error("read rule license", e))?;
            if bytes != LICENSE.as_bytes() { return Err("Pinned rules license differs from acknowledged license.".into()); }
            license_found = true;
        } else if let Some(entry) = wanted.get(relative) {
            if !found.insert(relative.to_owned()) { return Err("Duplicate selected rule in pinned archive.".into()); }
            let mut bytes = Vec::new();
            member.take(1024 * 1024 + 1).read_to_end(&mut bytes).map_err(|e| io_error("read selected rule", e))?;
            if bytes.len() > 1024 * 1024 { return Err("Selected rule exceeds 1 MiB.".into()); }
            validate_rule(&bytes, entry)?;
            write_file(&context.join("rules/selected").join(safe_asset_path(relative)?), &bytes)?;
        }
    }
    if found.len() != wanted.len() || !license_found { return Err("Pinned rules archive is incomplete; missing selected rules or license. No empty-pack fallback is allowed.".into()); }
    Ok(())
}

pub fn prepare_alternate(engine: &dyn Engine, config_root: &Path, ctl: &Ctl) -> Result<()> {
    require_acknowledged(config_root)?; // MUST precede even mkdir, probe, image pull, download or build.
    cancelled(ctl)?;
    validate_profile()?;
    let image = image_name();
    if engine.image_exists(&image, ctl)? { return Ok(()); }
    let lock = json(LOCK)?;
    let manifest = json(MANIFEST)?;
    let cache = config_root.join("alternate-cache").join(hash(MANIFEST.as_bytes()));
    let rules_archive = cache.join("rules.tar.gz");
    download_verified(string(&manifest, "archiveUrl")?, string(&manifest, "archiveSha256")?, &rules_archive, 64 * 1024 * 1024, ctl)?;
    let binary = cache.join("opengrep");
    download_verified(string(&lock["opengrep"], "assetUrl")?, string(&lock["opengrep"], "sha256")?, &binary, 128 * 1024 * 1024, ctl)?;
    write_file(&cache.join("LICENSE-opengrep-rules.txt"), LICENSE.as_bytes())?;
    write_file(&cache.join("selected-manifest.json"), MANIFEST.as_bytes())?;
    let context = cache.join(format!("build-{}", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        for (name, contents) in ASSETS { write_file(&context.join(name), contents.as_bytes())?; }
        write_file(&context.join("opengrep"), &read_bounded(&binary, 128 * 1024 * 1024)?)?;
        materialize_rules(&rules_archive, &context, ctl)?;
        ctl.say("Building pinned alternate analysis image; both grep engines must validate the exact rule pack and safe/unsafe fixtures.");
        engine.build_image(&image, &context, ctl)?;
        if !engine.image_exists(&image, ctl)? { return Err("Alternate image build returned without the pinned image; setup remains incomplete.".into()); }
        Ok(())
    })();
    let _ = fs::remove_dir_all(&context);
    result
}

fn validate_profile() -> Result<()> {
    validate_profile_data(PROFILE, LOCK)?;
    let manifest = json(MANIFEST)?;
    if manifest["originalRules"]["sha256"].as_str() != Some(hash(OWN_RULES.as_bytes()).as_str()) {
        return Err("Original review rules changed without updating the frozen manifest; rebuild the shipped rule metadata.".into());
    }
    Ok(())
}

fn validate_profile_data(profile_text: &str, lock_text: &str) -> Result<()> {
    let profile: Value = serde_yaml_ng::from_str(profile_text).map_err(|e| io_error("parse managed lint profile", e))?;
    let lock = json(lock_text)?;
    let allow: HashSet<&str> = ["eslint", "ruff", "bandit", "pmd", "cppcheck", "shellcheck", "hadolint", "checkov", "devskim", "betterleaks", "jscpd"].into_iter().collect();
    let descriptors = lock["megalinter"]["linters"].as_array().ok_or("Frozen descriptor list is missing.")?;
    let mut names = HashSet::new(); let mut found = HashSet::new();
    for item in descriptors {
        let linter = string(item, "linterName")?;
        if !allow.contains(linter) { return Err("Managed lint descriptor enables a non-allowlisted tool.".into()); }
        found.insert(linter); names.insert(string(item, "name")?);
    }
    if found != allow { return Err("Pinned MegaLinter descriptors are missing an expected high-signal tool.".into()); }
    let configured: HashSet<&str> = profile["ENABLE_LINTERS"].as_array().ok_or("Managed lint profile is missing ENABLE_LINTERS.")?.iter()
        .map(|v| v.as_str().ok_or_else(|| "Invalid managed lint descriptor key.".to_string())).collect::<Result<_>>()?;
    if configured != names { return Err("Managed lint allowlist differs from frozen descriptor name keys.".into()); }
    for key in ["PRE_COMMANDS", "POST_COMMANDS", "PLUGINS", "EXTENDS"] {
        if !profile[key].as_array().is_some_and(Vec::is_empty) { return Err(format!("Managed lint profile must disable {key}.")); }
    }
    if profile["APPLY_FIXES"] != "none" || profile["PARALLEL"] != false || profile["VALIDATE_ALL_CODEBASE"] != true {
        return Err("Managed lint profile must disable fixes, serialize linters and scan the full snapshot.".into());
    }
    for (key, value) in profile.as_object().ok_or("Managed lint profile is not a mapping.")? {
        if key.ends_with("_REPORTER") && !matches!(key.as_str(), "JSON_REPORTER" | "SARIF_REPORTER") && value != false {
            return Err(format!("Managed lint profile must disable reporter {key}."));
        }
    }
    Ok(())
}

const BUILD_FILES: &[&str] = &["package.json", "package-lock.json", "npm-shrinkwrap.json", "yarn.lock", "pnpm-lock.yaml", "eslint.config.js", "eslint.config.cjs", "eslint.config.mjs", "eslint.config.ts", ".eslintrc", ".eslintrc.json", ".eslintrc.js", ".eslintrc.cjs", ".eslintrc.yml", ".eslintrc.yaml", "pyproject.toml", "ruff.toml", ".ruff.toml"];

pub fn prepare_scan(project: &QualityProject, folder: &Path) -> Result<QualityScanPreparation> {
    validate_profile()?;
    let mut project = project.clone();
    let image = project.image.clone().unwrap_or_else(image_name);
    if image.trim().is_empty() || image.chars().any(char::is_whitespace) { return Err("Select a non-empty analysis image reference without whitespace.".into()); }
    project.image = Some(image.clone());
    let mut recipe = vec![format!("Image: {image}; source copied via validated tar to /workspace; originals are never mounted."),
        "Repository lint configurations execute only inside disposable isolated analysis containers; no automatic fixes or uploads. Existing individual lint configs win over fallback policies; repository .mega-linter.yml is not loaded.".into()];
    let mut warnings = vec!["Coverage depends on selected rule languages, actual applicable files and individual linter support; a skipped tool is not proof of clean code.".into()];
    let manifest = json(MANIFEST)?;
    let lock = json(LOCK)?;
    let counts = manifest["languageCounts"].as_object().ok_or("Shipped language coverage metadata is missing.")?;
    warnings.push(format!("Pinned selected pack: {} high-confidence security rules at {}; language/rule counts: {}. Languages not listed have no upstream grep coverage. Original review rules cover JavaScript/TypeScript and Python.",
        manifest["rules"].as_array().map_or(0, Vec::len), string(&manifest, "commit")?,
        counts.iter().map(|(language, count)| format!("{language}: {count}")).collect::<Vec<_>>().join(", ")));
    recipe.push(format!("Pinned tools: MegaLinter {}, Opengrep {}, Semgrep CE {}; rules manifest SHA-256 {}.",
        string(&lock["megalinter"], "version")?, string(&lock["opengrep"], "version")?, string(&lock["semgrep"], "version")?, hash(MANIFEST.as_bytes())));
    let mut digest = Sha256::new();
    digest.update(serde_json::to_vec(&project).map_err(|e| io_error("encode scan recipe", e))?);
    digest.update(MANIFEST.as_bytes()); digest.update(image_name().as_bytes());
    for name in BUILD_FILES {
        let path = folder.join(name);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(io_error(&format!("inspect configuration {name}"), error)),
        };
        {
            if metadata.file_type().is_symlink() || !metadata.is_file() { return Err(format!("Analysis preparation refuses linked/non-file configuration: {name}.")); }
            #[cfg(windows)] {
                use std::os::windows::fs::MetadataExt;
                if metadata.file_attributes() & 0x400 != 0 { return Err(format!("Analysis preparation refuses reparse-point configuration: {name}.")); }
            }
            let bytes = read_bounded(&path, 1024 * 1024)?;
            digest.update(name.as_bytes()); digest.update(&bytes);
            if name.starts_with("eslint.config") || name.starts_with(".eslintrc") || *name == "package.json" {
                let text = String::from_utf8_lossy(&bytes);
                if text.contains("plugin") || text.contains("eslint-config") || text.contains("extends") || text.contains("import ") || text.contains("require(") {
                    warnings.push(format!("{name} references lint plugins/config packages. Missing or incompatible dependencies are reported as tool failures, never replaced. If needed opt into Prepare npm dependencies; non-npm projects require an approved custom analysis image."));
                }
            }
        }
    }
    if project.prepare_npm_dependencies {
        if !folder.join("package-lock.json").is_file() { return Err("Prepare npm dependencies requires package-lock.json. Other package managers need an explicitly approved custom analysis image; no package scripts are run.".into()); }
        recipe.push("Separate temporary networked container: npm ci --ignore-scripts --no-audit --no-fund in /workspace; dependency downloads allowed, scripts disabled; validate node_modules tar (4 GiB maximum), copy dependencies only into MegaLinter, then delete prepared dependencies.".into());
    }
    recipe.extend([
        "MegaLinter: managed /opt/omp/megalinter.yml; --network none; APPLY_FIXES=none VALIDATE_ALL_CODEBASE=true PARALLEL=false JSON/SARIF reporters; hooks/plugins/remote reporters disabled.".to_string(),
        "Opengrep: scan --jobs=2 --quiet --config /opt/omp/rules/selected --config /opt/omp/rules/omp-quality.yml --json /workspace; --network none; --taint-intrafile only if validated scan --help exposes it.".to_string(),
        "Semgrep CE: scan --oss --metrics=off --disable-version-check --jobs=2 --quiet --config /opt/omp/rules/selected --config /opt/omp/rules/omp-quality.yml --json /workspace; --network none.".to_string(),
        "Each scanner: 2 CPUs / 4 GiB RAM / 20 minute wall-clock; local rule packs only, no Semgrep account or cloud submission.".to_string(),
    ]);
    for line in &recipe { digest.update(line.as_bytes()); digest.update([0]); }
    Ok(QualityScanPreparation { requires_consent: project.prepare_npm_dependencies, project, recipe, recipe_hash: format!("{:x}", digest.finalize()), warnings })
}

fn spec(run: &AlternateRun<'_>, action: &str, image: &str, network: Option<&str>) -> ContainerSpec {
    let owner: String = run.owner.chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect();
    let id: String = run.job_id.chars().filter(|c| c.is_ascii_alphanumeric()).take(12).collect();
    ContainerSpec { name: format!("omp-q-{owner}-scan-{id}-{action}"), image: image.into(),
        labels: vec![(OWNER_LABEL.into(), run.owner.into()), (ROLE_LABEL.into(), ROLE_SCAN.into())],
        network: network.map(str::to_owned), cpus: Some(2.0), memory: Some("4g".into()),
        user: Some("1000:1000".into()), workdir: Some("/workspace".into()),
        entrypoint: Some(vec!["python".into(), "/opt/omp/run-alternate.py".into()]), command: vec![action.into()],
        env: vec![("SEMGREP_SEND_METRICS".into(), "off".into()), ("SEMGREP_ENABLE_VERSION_CHECK".into(), "0".into()), ("DO_NOT_TRACK".into(), "1".into())],
        ..ContainerSpec::default() }
}

fn npm_dependencies(engine: &dyn Engine, run: &AlternateRun<'_>, image: &str, on_container: &dyn Fn(&str), ctl: &Ctl) -> Result<PathBuf> {
    cancelled(ctl)?;
    let tar = run.job_dir.join("prepared-dependencies.tar");
    let step = job::JobStep { spec: spec(run, "npm", image, None), copy_in: vec![("/workspace".into(), run.snapshot_tar.into())],
        copy_out: vec![("/workspace/node_modules".into(), tar.clone())], timeout_secs: 20 * 60 };
    let result = job::run_job(engine, &step, on_container, ctl)?;
    if result.cancelled || ctl.cancelled() { return Err("Code quality scan cancelled.".into()); }
    if result.timed_out || result.exit_code != 0 { return Err("npm ci --ignore-scripts did not complete. Review dependency compatibility or use an approved custom image; no analysis was claimed.".into()); }
    snapshot::validate_tar(&tar, MAX_DEPENDENCIES)?;
    // Validate every member's destination as well as the archive traversal/link/size protections.
    let mut archive = tar::Archive::new(fs::File::open(&tar).map_err(|e| io_error("open prepared dependencies", e))?);
    for member in archive.entries().map_err(|e| io_error("read prepared dependencies", e))? {
        let member = member.map_err(|e| io_error("read prepared dependency member", e))?;
        let path = member.path().map_err(|e| io_error("read prepared dependency path", e))?;
        if path.components().next() != Some(Component::Normal(std::ffi::OsStr::new("node_modules"))) {
            return Err("Prepared dependencies contain a member outside node_modules; analysis was not started.".into());
        }
    }
    Ok(tar)
}

fn sanitize_json(bytes: &[u8]) -> Result<String> {
    let mut value: Value = serde_json::from_slice(bytes).map_err(|e| io_error("parse analyzer JSON", e))?;
    reports::strip_secret_fields(&mut value);
    serde_json::to_string_pretty(&value).map_err(|e| io_error("sanitize analyzer JSON", e))
}
fn persisted_json(source: &Path, dest: &Path) -> Result<String> {
    let text = sanitize_json(&read_bounded(source, MAX_REPORT)?)?;
    write_file(dest, text.as_bytes())?;
    Ok(text)
}
fn locate_report<'a>(files: &'a [PathBuf], name: &str) -> Result<&'a Path> {
    let matches: Vec<&Path> = files.iter().filter(|p| p.file_name().is_some_and(|n| n == name)).map(PathBuf::as_path).collect();
    if matches.len() != 1 { return Err(format!("Analyzer did not produce exactly one {name}; analysis coverage is incomplete.")); }
    Ok(matches[0])
}


fn canonicalize_grep_findings(findings: &mut [QualityFinding]) -> Result<()> {
    let manifest = json(MANIFEST)?;
    let entries = manifest["rules"].as_array().ok_or("Frozen rule identities are missing.")?;
    let mut counts = BTreeMap::new();
    for entry in entries { *counts.entry(string(entry, "id")?).or_insert(0u32) += 1; }
    let mut identities = Vec::new();
    for entry in entries {
        let id = string(entry, "id")?;
        let path = string(entry, "path")?;
        let stem = path.strip_suffix(".yaml").or_else(|| path.strip_suffix(".yml")).unwrap_or(path);
        let suffix = format!("{}.{}", stem.replace('/', "."), id);
        // Upstream reuses several IDs across different source rules. Preserve the original leaf
        // ID, but keep its canonical source identity when needed to avoid merging unrelated rules.
        let canonical = if counts[id] > 1 { suffix.clone() } else { id.to_string() };
        identities.push((suffix, canonical));
    }
    let own: Value = serde_yaml_ng::from_str(OWN_RULES).map_err(|e| io_error("parse own rule identities", e))?;
    let own: Vec<(&str, String)> = own["rules"].as_array().ok_or("Original review rules are missing.")?.iter()
        .map(|r| { let id = string(r, "id")?; Ok((id, format!(".{id}"))) }).collect::<Result<_>>()?;
    for finding in findings {
        if let Some((id, _)) = own.iter().find(|(id, suffix)| finding.rule_id == *id || finding.rule_id.ends_with(suffix)) {
            finding.rule_id = (*id).to_string();
        } else if let Some((_, canonical)) = identities.iter().find(|(suffix, _)| finding.rule_id == *suffix || finding.rule_id.ends_with(&format!(".{suffix}"))) {
            finding.rule_id.clone_from(canonical);
        }
    }
    Ok(())
}
fn parse_step(action: &str, files: &[PathBuf], staging: &Path, report_dir: &Path, exit: i64) -> Result<(Vec<QualityFinding>, Vec<String>, Vec<String>, Option<u64>, ToolStatus)> {
    let mut warnings = Vec::new();
    let mut names = Vec::new();
    let bases = ["/workspace", "/tmp/lint", "/github/workspace"];
    if action == "megalinter" {
        // Parse the two reports independently: SARIF covers only capable linters.
        let json_report = locate_report(files, "mega-linter-report.json")
            .and_then(|path| persisted_json(&staging.join(path), &report_dir.join("megalinter-report.json")));
        let mut statuses = Vec::new();
        let mut report_failures = 0;
        let mut report_successes = 0;
        let mut warning_counts = BTreeMap::new();
        let mut config_failures = HashSet::new();
        match json_report {
            Ok(text) => {
                names.push("megalinter-report.json".into());
                let root: Value = serde_json::from_str(&text).map_err(|e| io_error("parse linter coverage", e))?;
                if let Some(linters) = root["linters"].as_array() {
                    for linter in linters {
                        if let Some(name) = linter["name"].as_str() {
                            warning_counts.insert(name.to_string(), linter["total_number_warnings"].as_u64().unwrap_or(0));
                            let diagnostic = linter.to_string().to_ascii_lowercase();
                            if ["cannot find module", "cannot find package", "couldn't find plugin", "failed to load config", "eslint v10 migration required", "invalid configuration", "error parsing your config"].iter().any(|hint| diagnostic.contains(hint)) {
                                config_failures.insert(name.to_string());
                            }
                        }
                    }
                }
                match reports::parse_megalinter_json(&text) {
                    Ok(value) => { statuses = value; report_successes += 1; }
                    Err(error) => { warnings.push(error); report_failures += 1; }
                }
            }
            Err(error) => { warnings.push(error); report_failures += 1; }
        }
        let mut findings = Vec::new();
        let sarif = locate_report(files, "megalinter.sarif")
            .and_then(|path| persisted_json(&staging.join(path), &report_dir.join("megalinter.sarif")));
        match sarif {
            Ok(text) => {
                names.push("megalinter.sarif".into());
                match reports::parse_sarif("MegaLinter", &text, &bases) {
                    Ok(parsed) => {
                        if !parsed.warnings.is_empty() { report_failures += 1; }
                        findings = parsed.findings; warnings.extend(parsed.warnings); report_successes += 1;
                    }
                    Err(error) => { warnings.push(error); report_failures += 1; }
                }
            }
            Err(error) => { warnings.push(error); report_failures += 1; }
        }
        if report_successes == 0 { return Err(warnings.join(" ")); }
        let mut failures = 0; let mut applicable = 0; let mut analyzed = 0;
        let known_counts = !statuses.is_empty() && statuses.iter().all(|s| s.file_count.is_some());
        for linter in statuses {
            let status = linter.status.to_ascii_lowercase();
            let count = linter.found_errors + warning_counts.get(&linter.name).copied().unwrap_or(0);
            let findings_exit = matches!(linter.return_code, Some(0 | 1))
                || linter.name == "JAVA_PMD" && linter.return_code == Some(4);
            let failed = config_failures.contains(&linter.name) || linter.error.is_some() && (count == 0 || !findings_exit);
            if !failed && (matches!(status.as_str(), "skipped" | "disabled") || linter.file_count == Some(0)) {
                warnings.push(format!("MegaLinter {}: skipped/no applicable files.", linter.name)); continue;
            }
            applicable += 1; analyzed += linter.file_count.unwrap_or(0);
            if failed {
                failures += 1;
                warnings.push(linter.error.clone().unwrap_or_else(|| format!("MegaLinter {} execution/configuration failed.", linter.name)));
            }
            if count > 0 && !failed {
                warnings.push(format!("MegaLinter {} reports {count} diagnostic(s); JSON counts may include diagnostics not represented by SARIF.", linter.name));
                if !findings.iter().any(|f| f.tools.iter().any(|t| t.eq_ignore_ascii_case(&linter.name)) || f.rule_id.to_ascii_lowercase().contains(&linter.name.to_ascii_lowercase())) {
                    let message = format!("{} reported {count} diagnostic(s). Review its sanitized JSON report; no source location is available for this tool-level summary.", linter.name);
                    findings.push(QualityFinding { id: hash(message.as_bytes())[..16].into(), tools: vec!["MegaLinter".into()], rule_id: format!("megalinter.summary.{}", linter.name), category: "Tool summary".into(), severity: Severity::Medium, message, path: None, line: None, column: None, end_line: None, help_url: None, hotspot: false });
                }
            }
        }
        if !known_counts { warnings.push("MegaLinter did not report all per-linter file counts; coverage is unavailable for those linters.".into()); }
        let status = if applicable > 0 && failures == applicable && findings.is_empty() { ToolStatus::Error }
            else if failures > 0 || report_failures > 0 || exit > 1 || !known_counts { ToolStatus::Partial }
            else if applicable == 0 { ToolStatus::Skipped }
            else { ToolStatus::Complete };
        Ok((findings, warnings, names, if known_counts { Some(analyzed) } else { None }, status))
    } else {
        let name = format!("{action}.json");
        let source = staging.join(locate_report(files, &name)?);
        let text = persisted_json(&source, &report_dir.join(&name))?;
        names.push(name);
        let mut parsed = reports::parse_grep_json(if action == "opengrep" { "Opengrep" } else { "Semgrep CE" }, &text, &bases)?;
        canonicalize_grep_findings(&mut parsed.findings)?;
        // The parser's zero-result note is informational; actual coverage is judged from scanned paths below.
        parsed.warnings.retain(|warning| !warning.contains("no findings were reported"));
        let value: Value = serde_json::from_str(&text).map_err(|e| io_error("parse grep coverage", e))?;
        let scanned = value["paths"]["scanned"].as_array().map(|p| p.len() as u64);
        let mut status = if scanned == Some(0) { ToolStatus::Skipped } else { ToolStatus::Complete };
        if !parsed.warnings.is_empty() || exit > 1 { status = ToolStatus::Partial; }
        if scanned.is_none() { warnings.push(format!("{action} did not report its scanned-file count; coverage is unavailable.")); status = ToolStatus::Partial; }
        warnings.extend(parsed.warnings);
        Ok((parsed.findings, warnings, names, scanned, status))
    }
}

pub fn run_alternate(engine: &dyn Engine, run: &AlternateRun<'_>, on_container: &dyn Fn(&str), ctl: &Ctl) -> Result<AlternateResult> {
    require_acknowledged(run.config_root)?;
    let started = std::time::Instant::now();
    let image = run.project.image.clone().unwrap_or_else(image_name);
    let report_dir = run.job_dir.join("reports");
    fs::create_dir_all(&report_dir).map_err(|e| io_error("create report directory", e))?;
    let dependency_path = run.job_dir.join("prepared-dependencies.tar");
    let result = (|| -> Result<AlternateResult> {
        let lock = json(LOCK)?;
        let mut tools = Vec::new(); let mut findings = Vec::new(); let mut warnings = Vec::new();
        let mut dependency_error = None;
        let mut cleanup_blocked = false;
        let dependencies = if run.project.prepare_npm_dependencies && !ctl.cancelled() {
            match npm_dependencies(engine, run, &image, on_container, ctl) {
                Ok(path) => Some(path),
                Err(error) => {
                    if !ctl.cancelled() { warnings.push(format!("Dependency preparation failed: {error}. Independent analyzers will still run, but MegaLinter coverage is incomplete.")); }
                    cleanup_blocked = error.contains("needs reconciliation");
                    dependency_error = Some(error); None
                }
            }
        } else { None };
        for (action, tool, version) in [("megalinter", "MegaLinter", string(&lock["megalinter"], "version")?), ("opengrep", "Opengrep", string(&lock["opengrep"], "version")?), ("semgrep", "Semgrep CE", string(&lock["semgrep"], "version")?)] {
            let mut outcome = QualityToolOutcome { tool: tool.into(), version: version.into(), status: ToolStatus::Error, exit_code: None, analyzed_files: None, finding_count: 0, error: None, report_names: Vec::new() };
            if ctl.cancelled() {
                outcome.status = ToolStatus::Skipped; outcome.error = Some("Cancelled before it started".into()); tools.push(outcome); continue;
            }
            if cleanup_blocked {
                outcome.status = ToolStatus::Skipped;
                outcome.error = Some("Blocked by an unresolved prior scanner container; use Start / retry reconciliation.".into());
                tools.push(outcome); continue;
            }
            let remaining = (60 * 60u64).saturating_sub(started.elapsed().as_secs());
            if remaining == 0 {
                outcome.error = Some("The job's 60 minute wall-clock limit expired before this tool could start.".into());
                tools.push(outcome); continue;
            }
            ctl.say(&format!("Running {tool} in a disposable network-disabled container."));
            let tar = run.job_dir.join(format!("{action}-reports.tar"));
            let staging = run.job_dir.join(format!("{action}-untrusted-reports"));
            let mut copies = vec![("/workspace".into(), run.snapshot_tar.into())];
            if action == "megalinter" { if let Some(dependencies) = &dependencies { copies.push(("/workspace".into(), dependencies.clone())); } }
            let step = job::JobStep { spec: spec(run, action, &image, Some("none")), copy_in: copies, copy_out: vec![("/out/.".into(), tar.clone())], timeout_secs: (20 * 60).min(remaining) };
            let executed = job::run_job(engine, &step, on_container, ctl);
            if executed.as_ref().err().is_some_and(|error| error.contains("needs reconciliation")) { cleanup_blocked = true; }
            if ctl.cancelled() || executed.as_ref().is_ok_and(|r| r.cancelled) {
                let _ = fs::remove_file(&tar); let _ = fs::remove_dir_all(&staging);
                outcome.exit_code = executed.as_ref().ok().map(|r| r.exit_code);
                match executed {
                    Ok(_) => {
                        outcome.status = ToolStatus::Partial;
                        outcome.error = Some("Cancelled during analysis; its disposable container has been stopped and removed.".into());
                    }
                    Err(error) => {
                        outcome.status = ToolStatus::Error;
                        outcome.error = Some(format!("Cancellation cleanup could not be confirmed: {error}"));
                    }
                }
                tools.push(outcome); continue;
            }
            let parsed = match executed {
                Err(error) => Err(error),
                Ok(execution) => {
                    outcome.exit_code = Some(execution.exit_code);
                    if execution.timed_out { warnings.push(format!("{tool} exceeded its wall-clock limit; remaining independent tools will run only while the overall job deadline permits.")); }
                    // Raw output can include source/secret snippets. Only sanitized JSON and a
                    // fixed exit summary are persisted; the bounded raw tail is deliberately discarded.
                    let log_name = format!("{action}.log");
                    write_file(&report_dir.join(&log_name), format!("{tool} exited {}; timed out: {}. Raw stdout/stderr is not persisted because tools may include detected secrets.\n", execution.exit_code, execution.timed_out).as_bytes())?;
                    outcome.report_names.push(log_name);
                    snapshot::extract_tar_safely(&tar, &staging, MAX_REPORT)
                        .and_then(|files| parse_step(action, &files, &staging, &report_dir, execution.exit_code))
                        .map(|mut parsed| { if execution.timed_out && parsed.4 != ToolStatus::Error { parsed.4 = ToolStatus::Partial; } parsed })
                }
            };
            let _ = fs::remove_file(&tar); let _ = fs::remove_dir_all(&staging);
            match parsed {
                Ok((mut found, mut notes, names, analyzed, mut status)) => {
                    if action == "megalinter" && dependency_error.is_some() && status != ToolStatus::Error { status = ToolStatus::Partial; }
                    outcome.status = status; outcome.analyzed_files = analyzed; outcome.finding_count = found.len() as u64;
                    outcome.report_names.extend(names); findings.append(&mut found); warnings.append(&mut notes);
                    if matches!(status, ToolStatus::Partial | ToolStatus::Error) { outcome.error = Some("Analysis/report coverage is incomplete; review warnings and the sanitized reports.".into()); }
                }
                Err(error) => {
                    let diagnostic = reports::preview_text(&reports::redact(&error, &[]), 4096).text;
                    outcome.error = Some(format!("{tool}: {diagnostic}"));
                    warnings.push(format!("{tool} did not complete; remaining independent tools were still attempted."));
                    for name in if action == "megalinter" { vec!["megalinter-report.json".to_string(), "megalinter.sarif".to_string()] } else { vec![format!("{action}.json")] } {
                        if report_dir.join(&name).is_file() { outcome.report_names.push(name); }
                    }
                }
            }
            tools.push(outcome);
        }
        Ok(AlternateResult { tools, findings: reports::merge_findings(findings), warnings, rules_hash: hash(MANIFEST.as_bytes()) })
    })();
    let _ = fs::remove_file(dependency_path);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use parking_lot::Mutex;
    use std::sync::atomic::Ordering;
    use std::collections::HashMap;

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("omp-alternate-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap(); Self(path)
        }
    }
    impl Drop for Scratch { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }
    fn project(path: &Path) -> QualityProject {
        QualityProject { folder: path.to_string_lossy().into_owned(), project_key: "fixture".into(), scanner: Scanner::Cli,
            entry_file: None, image: None, build_args: Vec::new(), prepare_npm_dependencies: false, exclusions: Vec::new() }
    }
    fn ack(path: &Path) {
        let state = rules_state(path);
        acknowledge_rules(path, &state.manifest_hash, &state.license_hash).unwrap();
    }
    fn archive(path: &Path, files: &[(&str, String)]) {
        let mut builder = tar::Builder::new(fs::File::create(path).unwrap());
        for (name, text) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(text.len() as u64); header.set_mode(0o644); header.set_uid(1000); header.set_gid(1000); header.set_cksum();
            builder.append_data(&mut header, name, text.as_bytes()).unwrap();
        }
        builder.finish().unwrap();
    }
    struct Fake {
        runtime: QualityRuntime,
        calls: Mutex<Vec<String>>,
        containers: Mutex<HashMap<String, ContainerInfo>>,
        mode: String,
    }
    impl Fake {
        fn new(mode: &str) -> Self { Self { runtime: QualityRuntime::Podman { connection: "fixture-rootless".into() }, calls: Mutex::new(Vec::new()), containers: Mutex::new(HashMap::new()), mode: mode.into() } }
        fn action(id: &str) -> &str { id.strip_suffix("-id").unwrap() }
    }
    impl Engine for Fake {
        fn runtime(&self) -> &QualityRuntime { &self.runtime }
        fn probe(&self, _: &Ctl) -> Result<EngineProbe> { panic!("analysis cannot probe another daemon") }
        fn ensure_started(&self, _: &Ctl) -> Result<()> { panic!("analysis cannot start an engine") }
        fn image_exists(&self, _: &str, _: &Ctl) -> Result<bool> { self.calls.lock().push("image_exists".into()); Ok(true) }
        fn pull(&self, _: &str, _: &Ctl) -> Result<()> { panic!("fixtures never pull") }
        fn build_image(&self, _: &str, _: &Path, _: &Ctl) -> Result<()> { panic!("fixtures never build") }
        fn network_create(&self, _: &str, _: &[(String, String)], _: &Ctl) -> Result<()> { panic!("analysis never creates networks") }
        fn volume_create(&self, _: &str, _: &[(String, String)], _: &Ctl) -> Result<()> { panic!("analysis never creates volumes") }
        fn create(&self, spec: &ContainerSpec, _: &Ctl) -> Result<String> {
            let action = &spec.command[0]; self.calls.lock().push(action.clone());
            assert_eq!(spec.cpus, Some(2.0)); assert_eq!(spec.memory.as_deref(), Some("4g"));
            assert!(spec.volumes.is_empty()); assert!(spec.secret_env.is_empty());
            assert!(spec.labels.contains(&(OWNER_LABEL.into(), "owner-uuid".into())));
            assert!(spec.labels.contains(&(ROLE_LABEL.into(), ROLE_SCAN.into())));
            if action == "npm" { assert!(spec.network.is_none()); } else { assert_eq!(spec.network.as_deref(), Some("none")); }
            if self.mode == "infra-all" || self.mode == format!("infra-{action}") { return Err(format!("Fixture {action} engine failure.")); }
            let id = format!("{action}-id");
            self.containers.lock().insert(id.clone(), ContainerInfo { id: id.clone(), running: false, ..ContainerInfo::default() }); Ok(id)
        }
        fn inspect(&self, id: &str, _: &Ctl) -> Result<Option<ContainerInfo>> { Ok(self.containers.lock().get(id).cloned()) }
        fn list_owned(&self, _: &str, _: &Ctl) -> Result<Vec<ContainerInfo>> { panic!("job cleanup must never enumerate foreign resources") }
        fn start(&self, id: &str, _: &Ctl) -> Result<()> { self.containers.lock().get_mut(id).unwrap().running = true; Ok(()) }
        fn stop(&self, id: &str, _: u32, ctl: &Ctl) -> Result<()> { assert!(!ctl.cancelled()); self.containers.lock().get_mut(id).unwrap().running = false; Ok(()) }
        fn wait(&self, id: &str, _: u64, ctl: &Ctl) -> Result<i64> {
            if self.mode == format!("cancel-{}", Self::action(id)) || self.mode == "cancel-cleanup" && id == "opengrep-id" { ctl.cancel.store(true, Ordering::Relaxed); return Err("fixture cancellation".into()); }
            self.containers.lock().get_mut(id).unwrap().running = false;
            Ok(if self.mode == "findings" { 1 } else { 0 })
        }
        fn copy_in(&self, id: &str, dest: &str, _: &Path, _: &Ctl) -> Result<()> {
            assert_eq!(dest, "/workspace");
            assert!(!self.containers.lock().get(id).unwrap().running);
            self.calls.lock().push(format!("copy:{id}")); Ok(())
        }
        fn copy_out(&self, id: &str, _: &str, path: &Path, _: &Ctl) -> Result<()> {
            let action = Self::action(id);
            if action == "npm" { archive(path, &[("node_modules/fixture/index.js", "module.exports = 1;".into())]); return Ok(()); }
            if action == "megalinter" {
                let skipped = self.mode == "skipped";
                let failed = self.mode == "linter-error";
                let count = if self.mode == "findings" { 2 } else { 0 };
                let report = json!({"linters":[{"name":"PYTHON_RUFF","is_active":true,"status":if failed {"error"} else {"success"},"return_code":if failed {2} else if count>0 {1} else {0},"files_number":if skipped {0} else {1},"total_number_errors":count}]});
                let mut files = vec![("mega-linter-report.json", report.to_string())];
                if self.mode != "missing-sarif" { files.push(("megalinter.sarif", json!({"version":"2.1.0","runs":[{"tool":{"driver":{"name":"fixture"}},"results":[]}]}).to_string())); }
                archive(path, &files); return Ok(());
            }
            let findings = if self.mode == "findings" { vec![json!({"check_id":"omp-quality.javascript.eval","path":"/workspace/quality.js","start":{"line":2,"col":1},"end":{"line":2,"col":12},"extra":{"message":"Review dynamic eval.","severity":"WARNING","lines":"never-persisted-secret","metavars":{"$X":{"abstract_content":"never-persisted-secret"}}}})] } else { Vec::new() };
            let scanned = if self.mode == "skipped" { Vec::new() } else { vec!["/workspace/quality.js"] };
            let report = json!({"results":findings,"errors":[],"paths":{"scanned":scanned}});
            let text = if self.mode == "malformed-opengrep" && action == "opengrep" { "{".into() } else { report.to_string() };
            archive(path, &[(if action == "opengrep" {"opengrep.json"} else {"semgrep.json"}, text)]); Ok(())
        }
        fn remove(&self, id: &str, force: bool, ctl: &Ctl) -> Result<()> {
            assert!(force); assert!(!ctl.cancelled());
            if self.mode == "cleanup-megalinter" && id == "megalinter-id" || self.mode == "cancel-cleanup" && id == "opengrep-id" { return Err("fixture removal failed".into()); }
            self.containers.lock().remove(id); Ok(())
        }
        fn exec(&self, _: &str, _: &[String], _: &[(String, String)], _: u64, _: &Ctl) -> Result<ExecOutput> { panic!("analysis uses only run_job") }
        fn logs(&self, _: &str, _: u32, _: &Ctl) -> Result<String> { Ok("never-persisted-secret".into()) }
    }
    fn fixture(mode: &str, npm: bool) -> (Scratch, Fake, AlternateResult) {
        let scratch = Scratch::new(); let engine = Fake::new(mode); ack(&scratch.0);
        let source = scratch.0.join("source.tar"); archive(&source, &[]);
        let mut project = project(&scratch.0); project.prepare_npm_dependencies = npm;
        let run = AlternateRun { owner: "owner-uuid", job_id: "fixture-job-id", snapshot_tar: &source, project: &project, job_dir: &scratch.0, config_root: &scratch.0 };
        let result = run_alternate(&engine, &run, &|_| {}, &Ctl::silent()).unwrap();
        (scratch, engine, result)
    }
    fn actions(engine: &Fake) -> Vec<String> {
        engine.calls.lock().iter().filter(|value| !value.starts_with("copy:")).cloned().collect()
    }
    #[test]
    fn shipped_lock_manifest_and_original_yaml_are_frozen_and_well_formed() {
        validate_profile().unwrap();
        let lock = json(LOCK).unwrap();
        assert!(lock["megalinter"]["image"].as_str().unwrap().contains("@sha256:"));
        assert_eq!(lock["semgrep"]["version"], "1.175.0");
        assert_eq!(string(&lock["opengrep"], "sha256").unwrap().len(), 64);
        assert!(string(&lock["opengrep"], "assetUrl").unwrap().contains("/v1.31.0/opengrep_musllinux_x86"));
        let manifest = json(MANIFEST).unwrap();
        let rules = manifest["rules"].as_array().unwrap(); assert!(!rules.is_empty());
        let mut ids = HashSet::new();
        for entry in rules {
            assert!(ids.insert((string(entry, "path").unwrap(), string(entry, "id").unwrap())));
            assert_eq!(entry["category"], "security"); assert_eq!(entry["confidence"], "HIGH");
            assert!(matches!(entry["severity"].as_str(), Some("ERROR" | "WARNING")));
            assert_eq!(string(entry, "sha256").unwrap().len(), 64); safe_asset_path(string(entry, "path").unwrap()).unwrap();
            assert_eq!(entry["commit"], manifest["commit"]); assert_eq!(entry["license"], "LGPL-2.1 + Commons Clause");
        }
        let own: Value = serde_yaml_ng::from_str(OWN_RULES).unwrap();
        assert_eq!(own["rules"].as_array().unwrap().len(), 7);
        assert_eq!(manifest["originalRules"]["sha256"], hash(OWN_RULES.as_bytes()));
        assert_eq!(image_name().split(':').next_back().unwrap().len(), 24);
    }
    #[test]
    fn profile_rejects_hooks_extra_tools_and_missing_allowlisted_descriptor() {
        assert!(validate_profile_data(&PROFILE.replace("PRE_COMMANDS: []", "PRE_COMMANDS: [curl]"), LOCK).is_err());
        assert!(validate_profile_data(&PROFILE.replace("  - PYTHON_RUFF", "  - PYTHON_BLACK"), LOCK).is_err());
        let mut lock = json(LOCK).unwrap(); lock["megalinter"]["linters"].as_array_mut().unwrap().retain(|v| v["linterName"] != "bandit");
        assert!(validate_profile_data(PROFILE, &lock.to_string()).is_err());
        assert!(PROFILE.contains("--redact=100")); assert!(PROFILE.contains("--validation=false"));
    }
    #[test]
    fn license_admission_has_no_engine_network_or_filesystem_side_effects() {
        let scratch = Scratch::new(); let root = scratch.0.join("not-created"); let engine = Fake::new("");
        let state = rules_state(&root); assert!(!state.acknowledged); assert!(!root.exists());
        assert_eq!(prepare_alternate(&engine, &root, &Ctl::silent()).unwrap_err(), RULES_MESSAGE);
        assert!(acknowledge_rules(&root, "old-manifest", &state.license_hash).is_err());
        assert!(acknowledge_rules(&root, &state.manifest_hash, "old-license").is_err());
        assert!(engine.calls.lock().is_empty()); assert!(!root.exists());
        ack(&root); assert!(rules_state(&root).acknowledged);
        let mut receipt: Receipt = bounded_json(&root.join("rules-consent.json"), 1024 * 1024).unwrap().unwrap();
        receipt.license_hash = "stale".into(); atomic_json(&root.join("rules-consent.json"), &receipt, 1024 * 1024).unwrap();
        assert_eq!(require_acknowledged(&root).unwrap_err(), RULES_MESSAGE);
        assert!(engine.calls.lock().is_empty());
    }
    #[test]
    fn preparation_requires_only_explicit_npm_consent_and_hashes_lock_changes() {
        let scratch = Scratch::new(); let mut project = project(&scratch.0);
        let plain = prepare_scan(&project, &scratch.0).unwrap(); assert!(!plain.requires_consent);
        project.prepare_npm_dependencies = true; assert!(prepare_scan(&project, &scratch.0).is_err());
        write_file(&scratch.0.join("package-lock.json"), b"{\"lockfileVersion\":3}").unwrap();
        let first = prepare_scan(&project, &scratch.0).unwrap(); assert!(first.requires_consent);
        assert!(first.recipe.iter().any(|line| line.contains("npm ci --ignore-scripts")));
        write_file(&scratch.0.join("package-lock.json"), b"{\"lockfileVersion\":3,\"changed\":true}").unwrap();
        assert_ne!(first.recipe_hash, prepare_scan(&project, &scratch.0).unwrap().recipe_hash);
        write_file(&scratch.0.join("eslint.config.js"), b"import plugin from 'eslint-plugin-fixture'; export default [plugin.configs.recommended];").unwrap();
        assert!(prepare_scan(&project, &scratch.0).unwrap().warnings.iter().any(|warning| warning.contains("plugins/config packages")));
    }
    #[test]
    fn three_tools_run_in_order_without_network_and_findings_only_exits_complete() {
        let (scratch, engine, result) = fixture("findings", false);
        assert_eq!(actions(&engine), ["megalinter", "opengrep", "semgrep"]);
        assert!(engine.containers.lock().is_empty());
        assert!(result.tools.iter().all(|tool| tool.status == ToolStatus::Complete));
        let eval: Vec<_> = result.findings.iter().filter(|finding| finding.rule_id == "omp-quality.javascript.eval").collect();
        assert_eq!(eval.len(), 1); assert_eq!(eval[0].tools, ["Opengrep", "Semgrep CE"]);
        for tool in result.tools { for name in tool.report_names {
            assert!(!fs::read_to_string(scratch.0.join("reports").join(name)).unwrap().contains("never-persisted-secret"));
        }}
        assert!(!scratch.0.join("opengrep-reports.tar").exists());
        assert!(!scratch.0.join("opengrep-untrusted-reports").exists());
    }
    #[test]
    fn independent_execution_errors_do_not_skip_later_engines() {
        let (_, engine, result) = fixture("infra-megalinter", false);
        assert_eq!(actions(&engine), ["megalinter", "opengrep", "semgrep"]);
        assert_eq!(result.tools[0].status, ToolStatus::Error);
        assert_eq!(result.tools[1].status, ToolStatus::Complete); assert_eq!(result.tools[2].status, ToolStatus::Complete);
        let (_, _, result) = fixture("infra-all", false); assert!(result.tools.iter().all(|tool| tool.status == ToolStatus::Error));
    }
    #[test]
    fn missing_malformed_and_zero_coverage_reports_are_not_clean_results() {
        let (_, _, result) = fixture("missing-sarif", false); assert_eq!(result.tools[0].status, ToolStatus::Partial);
        let (_, _, result) = fixture("malformed-opengrep", false); assert_eq!(result.tools[1].status, ToolStatus::Error); assert_eq!(result.tools[2].status, ToolStatus::Complete);
        let (_, _, result) = fixture("linter-error", false); assert_eq!(result.tools[0].status, ToolStatus::Error);
        let (_, _, result) = fixture("skipped", false); assert!(result.tools.iter().all(|tool| tool.status == ToolStatus::Skipped));
    }
    #[test]
    fn cancellation_keeps_prior_results_removes_container_and_never_starts_next_tool() {
        let (_, engine, result) = fixture("cancel-opengrep", false);
        assert_eq!(actions(&engine), ["megalinter", "opengrep"]);
        assert!(engine.containers.lock().is_empty());
        assert_eq!(result.tools[0].status, ToolStatus::Complete);
        assert_eq!(result.tools[1].status, ToolStatus::Partial);
        assert_eq!(result.tools[2].status, ToolStatus::Skipped);
        assert_eq!(result.tools[2].error.as_deref(), Some("Cancelled before it started"));
    }
    #[test]
    fn unconfirmed_cleanup_blocks_later_containers_instead_of_claiming_success() {
        let (_, engine, result) = fixture("cleanup-megalinter", false);
        assert_eq!(actions(&engine), ["megalinter"]);
        assert_eq!(result.tools[0].status, ToolStatus::Error);
        assert!(result.tools[0].error.as_deref().unwrap().contains("needs reconciliation"));
        assert!(result.tools[1..].iter().all(|tool| tool.status == ToolStatus::Skipped));
        let (_, engine, result) = fixture("cancel-cleanup", false);
        assert_eq!(actions(&engine), ["megalinter", "opengrep"]);
        assert_eq!(result.tools[1].status, ToolStatus::Error);
        assert!(result.tools[1].error.as_deref().unwrap().contains("Cancellation cleanup could not be confirmed"));
        assert_eq!(result.tools[2].error.as_deref(), Some("Cancelled before it started"));
    }
    #[test]
    fn duplicate_upstream_ids_keep_source_identity_while_engines_still_merge() {
        let make = |rule: &str| QualityFinding { id: "id".into(), tools: vec!["Opengrep".into()], rule_id: rule.into(), category: "Security".into(), severity: Severity::High, message: "m".into(), path: None, line: None, column: None, end_line: None, help_url: None, hotspot: false };
        let manifest = json(MANIFEST).unwrap();
        let duplicate = manifest["rules"].as_array().unwrap().iter().find(|rule| rule["id"] == "use-of-md5").unwrap();
        let path = string(duplicate, "path").unwrap();
        let stem = path.strip_suffix(".yaml").or_else(|| path.strip_suffix(".yml")).unwrap_or(path).replace('/', ".");
        let mut findings = vec![make(&format!("opt.omp.rules.selected.{stem}.use-of-md5")), make("opt.omp.rules.omp-quality.omp-quality.python.eval")];
        canonicalize_grep_findings(&mut findings).unwrap();
        assert_eq!(findings[0].rule_id, format!("{stem}.use-of-md5"));
        assert_eq!(findings[1].rule_id, "omp-quality.python.eval");
    }
    #[test]
    fn npm_preparation_is_separate_and_only_megalinter_receives_dependencies() {
        let (scratch, engine, result) = fixture("", true);
        assert_eq!(actions(&engine), ["npm", "megalinter", "opengrep", "semgrep"]);
        let calls = engine.calls.lock();
        assert_eq!(calls.iter().filter(|call| call.as_str() == "copy:megalinter-id").count(), 2);
        assert_eq!(calls.iter().filter(|call| call.as_str() == "copy:opengrep-id").count(), 1);
        assert_eq!(calls.iter().filter(|call| call.as_str() == "copy:semgrep-id").count(), 1);
        assert!(!scratch.0.join("prepared-dependencies.tar").exists());
        assert!(result.tools.iter().all(|tool| tool.status == ToolStatus::Complete));
        let (_, engine, result) = fixture("infra-npm", true);
        assert_eq!(actions(&engine), ["npm", "megalinter", "opengrep", "semgrep"]);
        assert_eq!(result.tools[0].status, ToolStatus::Partial);
        assert_eq!(result.tools[1].status, ToolStatus::Complete);
    }
}
