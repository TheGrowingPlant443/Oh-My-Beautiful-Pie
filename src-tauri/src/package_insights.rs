//! Bounded public registry and repository metadata. This service never reads project files.
use parking_lot::{Condvar, Mutex};
use reqwest::blocking::Client;
use reqwest::{header, StatusCode, Url};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_BODY: usize = 5 * 1024 * 1024;
const DAY: u64 = 86_400_000;
const HOUR: u64 = 3_600_000;
const TRANSIENT: u64 = 300_000;
const USER_AGENT: &str = "omp-manager/0.1.0 (+https://github.com)";
const PROVIDER_BASES: [&str; 6] = [
    "https://registry.npmjs.org/", "https://api.npmjs.org/", "https://pypi.org/",
    "https://crates.io/", "https://repo.maven.apache.org/", "https://api.github.com/",
];
const PYPI_DOWNLOADS: &str = "PyPI does not expose public per-package download counts through its JSON API.";
const MAVEN_DOWNLOADS: &str = "Maven Central download statistics are publisher-only.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InsightStatus { Available, Unavailable, RateLimited, Error }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CountMetric {
    pub value: Option<u64>,
    pub status: InsightStatus,
    pub source: String,
    pub period: Option<String>,
    pub reason: Option<String>,
    pub retry_at: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OwnerType { User, Organization, Unknown }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryInsights {
    pub url: String,
    pub owner_login: String,
    pub owner_type: OwnerType,
    pub stars: CountMetric,
    pub contributors: CountMetric,
    pub checked_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageInsights {
    pub id: String,
    pub registry_url: String,
    pub newest_version: Option<String>,
    pub newest_status: InsightStatus,
    pub description: Option<String>,
    pub repository_url: Option<String>,
    pub github_pending: bool,
    pub repository: Option<RepositoryInsights>,
    pub repository_reason: Option<String>,
    pub downloads: CountMetric,
    pub checked_at: u64,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GithubMode { Skip, Demand, Bulk }

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageInsightsStatus {
    pub github_authenticated: bool,
    pub github_source: Option<String>,
    pub github_problem: Option<String>,
    pub github_limit: Option<u64>,
    pub github_remaining: Option<u64>,
    pub github_reset_at: Option<u64>,
    pub github_cooldown_until: Option<u64>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Entry<T> { schema: u32, key: String, saved_at: u64, retry_at: Option<u64>, value: T }

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacyRepositoryEntry { schema: u32, key: String, expires_at: u64, value: Value }

#[derive(Default)]
struct Cache {
    packages: HashMap<String, Entry<PackageInsights>>,
    repositories: HashMap<String, Entry<RepositoryInsights>>,
    cooldowns: HashMap<Provider, u64>,
    github: GithubBudget,
    /// Credential kind ("anonymous" | "authenticated") the GitHub budget and cooldown were measured with.
    credential: Option<String>,
    /// Time of the last credential-kind switch; unsuccessful repository entries older than this are refetched.
    github_since: u64,
}

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GithubBudget { limit: Option<u64>, remaining: Option<u64>, reset_at: Option<u64> }

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderState {
    schema: u32, cooldowns: HashMap<String, u64>, github: GithubBudget,
    #[serde(default)] credential: Option<String>,
    #[serde(default)] github_since: u64,
}

struct RegistryPermit<'a>(&'a PackageInsightsService);
impl Drop for RegistryPermit<'_> {
    fn drop(&mut self) {
        *self.0.registry_permits.lock() += 1;
        self.0.registry_ready.notify_one();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Provider { NpmRegistry, NpmDownloads, Pypi, Cargo, Maven, Github }
impl Provider {
    fn index(self) -> usize { self as usize }
    fn key(self) -> &'static str {
        ["npmRegistry", "npmDownloads", "pypi", "cargo", "maven", "github"][self.index()]
    }
}

#[derive(Debug)]
struct Failure { status: InsightStatus, reason: &'static str, retry_at: Option<u64>, ttl: u64 }
impl Failure {
    fn error(reason: &'static str) -> Self {
        Self { status: InsightStatus::Error, reason, retry_at: None, ttl: TRANSIENT }
    }
    fn unavailable(reason: &'static str) -> Self {
        Self { status: InsightStatus::Unavailable, reason, retry_at: None, ttl: DAY }
    }
    fn metric(&self, source: &str) -> CountMetric {
        CountMetric { value: None, status: self.status, source: source.into(), period: None,
            reason: Some(self.reason.into()), retry_at: self.retry_at }
    }
}

struct Response { status: StatusCode, headers: header::HeaderMap, body: Vec<u8> }

pub struct PackageInsightsService {
    root: PathBuf,
    client: Client,
    cache: Mutex<Cache>,
    registry_permits: Mutex<usize>,
    registry_ready: Condvar,
    github_gate: Mutex<()>,
    credential: Mutex<CredentialState>,
    env: Arc<dyn Fn(&str) -> Option<String> + Send + Sync>,
    gh: Arc<dyn Fn() -> GhOutcome + Send + Sync>,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    request_timeout: Duration,
    lookup_timeout: Duration,
    #[cfg(test)]
    test_base: Option<Url>,
}

impl PackageInsightsService {
    pub fn new(root: PathBuf) -> Result<Self, String> {
        Self::create(root, Arc::new(epoch_ms))
    }

    fn create(root: PathBuf, clock: Arc<dyn Fn() -> u64 + Send + Sync>) -> Result<Self, String> {
        let client = Client::builder().connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10)).redirect(reqwest::redirect::Policy::none())
            .user_agent(USER_AGENT).build().map_err(|_| "Cannot initialize package metadata HTTP client".to_string())?;
        let root = root.join("package-insights");
        let mut cache = Cache::default();
        if let Some(state) = read_json::<ProviderState>(&root.join("provider-state.json")).filter(|s| s.schema == 1) {
            let now = clock();
            for provider in [Provider::NpmRegistry, Provider::NpmDownloads, Provider::Pypi, Provider::Cargo, Provider::Maven, Provider::Github] {
                if let Some(at) = state.cooldowns.get(provider.key()).filter(|at| **at > now) {
                    cache.cooldowns.insert(provider, *at);
                }
            }
            cache.github = state.github;
            cache.credential = state.credential.filter(|kind| matches!(kind.as_str(), "anonymous" | "authenticated"));
            cache.github_since = state.github_since;
        }
        Ok(Self { root, client, cache: Mutex::new(cache),
            registry_permits: Mutex::new(4), registry_ready: Condvar::new(), github_gate: Mutex::new(()),
            credential: Mutex::new(CredentialState::default()), env: Arc::new(credential_env), gh: Arc::new(run_gh_token),
            clock, request_timeout: Duration::from_secs(10),
            lookup_timeout: Duration::from_secs(30), #[cfg(test)] test_base: None })
    }

    /// Loopback-only injection; not present in production binaries. Provider path prefixes keep fixtures independent.
    #[cfg(test)]
    pub(crate) fn with_test_providers(root: PathBuf, base: &str, token: Option<String>,
        now: Arc<std::sync::atomic::AtomicU64>, request_timeout: Duration, lookup_timeout: Duration) -> Result<Self, String> {
        let url = Url::parse(base).map_err(|_| "Invalid fixture base")?;
        if url.scheme() != "http" || !matches!(url.host_str(), Some("127.0.0.1" | "[::1]"))
            || !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() {
            return Err("Fixture base must be loopback HTTP".into());
        }
        let mut service = Self::create(root, Arc::new(move || now.load(std::sync::atomic::Ordering::SeqCst)))?;
        // Fixtures never read the workstation's environment or spawn the real `gh`.
        service.env = Arc::new(|_: &str| None::<String>);
        service.gh = Arc::new(|| GhOutcome::NotInstalled);
        let source = token.as_ref().map(|_| "GITHUB_TOKEN");
        // The kind is recorded only when a credential is resolved (not for this injected one), so fixtures can
        // persist a budget and still exercise a later switch through `with_credentials`.
        service.credential = Mutex::new(CredentialState { current: Some(Credential { token, source, problem: None }),
            probed_at: (service.clock)() });
        // Fixtures must not depend on the workstation's system or environment proxy configuration.
        service.client = Client::builder().no_proxy().connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10)).redirect(reqwest::redirect::Policy::none())
            .user_agent(USER_AGENT).build().map_err(|_| "Cannot initialize fixture HTTP client")?;
        service.test_base = Some(url);
        service.request_timeout = request_timeout;
        service.lookup_timeout = lookup_timeout;
        Ok(service)
    }

    /// Replaces the injected credential sources and forgets the installed credential so the next use resolves again.
    #[cfg(test)]
    fn with_credentials(mut self, env: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
        gh: impl Fn() -> GhOutcome + Send + Sync + 'static) -> Self {
        self.env = Arc::new(env);
        self.gh = Arc::new(gh);
        *self.credential.lock() = CredentialState::default();
        self
    }

    pub fn lookup(&self, id: &str, github: GithubMode, refresh: bool) -> Result<PackageInsights, String> {
        let package = Package::parse(id).ok_or("Invalid package id")?;
        let deadline = Instant::now() + self.lookup_timeout;
        let cached = self.cached_package(id);
        let mut result = if !refresh && cached.as_ref().is_some_and(|entry| !entry_due(entry, (self.clock)())) {
            cached.unwrap().value
        } else {
            let Some(_permit) = self.registry_permit(deadline) else {
                if let Some(old) = cached.filter(|entry| package_good(&entry.value)) {
                    let mut value = old.value;
                    value.notes.push(refresh_note("Package metadata lookup deadline exceeded.", old.saved_at));
                    if let Some(repo) = value.repository_url.as_deref().and_then(normalize_repository) {
                        value.repository = self.cached_repository(&repo.key()).map(|entry| entry.value);
                        value.github_pending = value.repository.is_none();
                    }
                    return Ok(value);
                }
                return Ok(package.deadline_result(id, (self.clock)()));
            };
            let cached = self.cached_package(id);
            if !refresh && cached.as_ref().is_some_and(|entry| !entry_due(entry, (self.clock)())) {
                cached.unwrap().value
            } else {
                let entry = self.registry(id, &package, deadline);
                if let Some(old) = cached.filter(|old| package_good(&old.value) && !package_good(&entry.value)) {
                    let mut value = old.value;
                    value.notes.push(refresh_note(entry.value.notes.first().map(String::as_str).unwrap_or("registry metadata unavailable"), old.saved_at));
                    value
                } else {
                    self.persist("packages", &entry);
                    let value = entry.value.clone();
                    self.cache.lock().packages.insert(id.into(), entry);
                    value
                }
            }
        };
        if let Some(repo) = result.repository_url.as_deref().and_then(normalize_repository) {
            let entry = if github == GithubMode::Skip { self.cached_repository(&repo.key()) }
                else { self.repository(&repo, github, refresh, deadline, &mut result.notes) };
            result.github_pending = self.cached_repository(&repo.key()).is_none();
            result.repository = entry.map(|entry| entry.value);
        }
        Ok(result)
    }

    pub fn status(&self) -> PackageInsightsStatus {
        let credential = self.credential(false);
        let now = (self.clock)();
        let cache = self.cache.lock();
        // A budget past its hourly reset says nothing about the current window.
        let current = cache.github.reset_at.is_none_or(|at| at > now);
        PackageInsightsStatus {
            github_authenticated: credential.token.is_some(),
            github_source: credential.token.as_ref().and(credential.source).map(str::to_string),
            github_problem: credential.problem.clone(),
            github_limit: cache.github.limit,
            github_remaining: cache.github.remaining.filter(|_| current),
            github_reset_at: cache.github.reset_at.filter(|_| current),
            github_cooldown_until: cache.cooldowns.get(&Provider::Github).copied().filter(|at| *at > now),
        }
    }

    /// Forces a fresh credential probe (environment, then `gh`) and reports the resulting state.
    pub fn recheck(&self) -> PackageInsightsStatus {
        self.credential(true);
        self.status()
    }

    /// Saved registry and repository metadata only: never resolves a token, runs `gh`, refreshes or sends a request.
    pub fn cached_only(&self, id: &str) -> Result<Option<PackageInsights>, String> {
        Package::parse(id).ok_or("Invalid package id")?;
        let Some(entry) = self.cached_package(id) else { return Ok(None); };
        let mut value = entry.value;
        if let Some(repo) = value.repository_url.as_deref().and_then(normalize_repository) {
            value.repository = self.cached_repository(&repo.key()).map(|entry| entry.value);
            value.github_pending = value.repository.is_none();
        }
        Ok(Some(value))
    }

    /// Current GitHub credential. A token is kept until GitHub rejects it or `force`; a missing one is re-probed
    /// at most every 30 s so a later `gh auth login` or PATH change is noticed without an app restart.
    fn credential(&self, force: bool) -> Credential {
        let mut state = self.credential.lock();
        let now = (self.clock)();
        if !force {
            if let Some(current) = &state.current {
                if current.token.is_some() || now.saturating_sub(state.probed_at) < CREDENTIAL_RETRY {
                    return current.clone();
                }
            }
        }
        let resolved = resolve_credential(|name| (self.env)(name), || (self.gh)());
        state.current = Some(resolved.clone());
        state.probed_at = now;
        self.apply_credential_kind(&resolved);
        resolved
    }

    /// GitHub answered 401 to `sent`: stop using it. Lock order is always credential, then cache.
    fn reject_credential(&self, sent: &str) {
        let mut state = self.credential.lock();
        if state.current.as_ref().and_then(|c| c.token.as_deref()) != Some(sent) { return; }
        let rejected = Credential { token: None, source: state.current.as_ref().and_then(|c| c.source),
            problem: Some(REJECTED_TOKEN.into()) };
        state.current = Some(rejected.clone());
        state.probed_at = (self.clock)();
        self.apply_credential_kind(&rejected);
    }

    /// Budget and cooldown belong to the credential that produced them; a switch invalidates both.
    fn apply_credential_kind(&self, credential: &Credential) {
        let kind = if credential.token.is_some() { "authenticated" } else { "anonymous" };
        let mut cache = self.cache.lock();
        if cache.credential.as_deref() == Some(kind) { return; }
        // A state saved before kinds were recorded is anonymous only in the sense that it predates any token.
        if cache.credential.is_some() || kind == "authenticated" {
            cache.github = GithubBudget::default();
            cache.cooldowns.remove(&Provider::Github);
            cache.github_since = (self.clock)();
        }
        cache.credential = Some(kind.into());
        self.persist_provider_state(&cache);
    }

    fn registry_permit(&self, deadline: Instant) -> Option<RegistryPermit<'_>> {
        let mut available = self.registry_permits.lock();
        while *available == 0 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() { return None; }
            self.registry_ready.wait_for(&mut available, remaining);
        }
        *available -= 1;
        Some(RegistryPermit(self))
    }

    fn registry(&self, id: &str, package: &Package, deadline: Instant) -> Entry<PackageInsights> {
        let mut ttl = DAY;
        let mut result = PackageInsights { id: id.into(), registry_url: package.page(), newest_version: None,
            newest_status: InsightStatus::Unavailable, description: None, repository_url: None, github_pending: false,
            repository: None, repository_reason: None,
            downloads: Failure::unavailable("Download count is not available.").metric(package.source()),
            checked_at: (self.clock)(), notes: vec![] };
        let repository = match &package {
            Package::Npm(name) => {
                let metadata = self.json(Provider::NpmRegistry, &[name, "latest"], &[], deadline);
                let repo = self.apply_metadata(metadata, &mut result, &mut ttl, |v| {
                    (clean_text(v["version"].as_str(), 256), repository_field(&v["repository"]), one_line(v["description"].as_str()))
                });
                result.downloads = match self.json(Provider::NpmDownloads,
                    &["downloads", "point", "last-month", name], &[], deadline) {
                    Ok(v) => {
                        let dates = v["start"].as_str().zip(v["end"].as_str()).filter(|(a,b)| valid_date(a) && valid_date(b) && a <= b);
                        match (v["downloads"].as_u64(), dates) {
                            (Some(value), Some((start,end))) => measured(value, "npm", Some(format!("Last 30 available days ({start}–{end}); package-wide"))),
                            _ => Failure::error("Invalid npm download response.").metric("npm")
                        }
                    }
                    Err(e) => e.metric("npm")
                };
                repo
            }
            Package::Pypi(name) => {
                result.downloads = Failure::unavailable(PYPI_DOWNLOADS).metric("PyPI");
                self.apply_metadata(self.json(Provider::Pypi, &["pypi", name, "json"], &[], deadline),
                    &mut result, &mut ttl, |v| (clean_text(v["info"]["version"].as_str(), 256), pypi_repository(&v["info"]), one_line(v["info"]["summary"].as_str())))
            }
            Package::Cargo(name) => {
                match self.json(Provider::Cargo, &["api", "v1", "crates", name], &[], deadline) {
                    Ok(v) => {
                        result.downloads = v["crate"]["recent_downloads"].as_u64()
                            .map(|n| measured(n, "crates.io", Some("Last 90 days; package-wide".into())))
                            .unwrap_or_else(|| Failure::unavailable("crates.io did not return a recent download count.").metric("crates.io"));
                        self.apply_metadata(Ok(v), &mut result, &mut ttl, |v|
                            (clean_text(v["crate"]["default_version"].as_str(), 256), repository_field(&v["crate"]["repository"]), one_line(v["crate"]["description"].as_str())))
                    }
                    Err(e) => {
                        result.downloads = e.metric("crates.io");
                        self.apply_metadata(Err(e), &mut result, &mut ttl, |_| (None,None,None))
                    }
                }
            }
            Package::Maven(group, artifact) => {
                result.downloads = Failure::unavailable(MAVEN_DOWNLOADS).metric("Maven Central");
                self.maven(group, artifact, &mut result, &mut ttl, deadline)
            }
        };
        result.repository_url = repository.map(|repo| repo.url());
        if result.repository_url.is_none() && result.repository_reason.is_none() {
            result.repository_reason = Some("No supported public GitHub repository was supplied by the registry.".into());
        }
        let saved_at = (self.clock)();
        result.checked_at = saved_at;
        let retry_at = (!package_good(&result)).then(|| saved_at.saturating_add(ttl));
        Entry { schema: 2, key: id.into(), saved_at, retry_at, value: result }
    }

    fn apply_metadata<F>(&self, metadata: Result<Value, Failure>, result: &mut PackageInsights, ttl: &mut u64, parse: F) -> Option<Repo>
        where F: FnOnce(&Value) -> (Option<String>, Option<Repo>, Option<String>) {
        match metadata {
            Ok(value) => {
                let (version, repo, description) = parse(&value);
                result.description = description;
                result.newest_status = if version.is_some() { InsightStatus::Available } else { InsightStatus::Error };
                if version.is_none() {
                    *ttl = (*ttl).min(TRANSIENT);
                    result.notes.push("Registry response did not contain a valid recommended release.".into());
                }
                result.newest_version = version;
                repo
            }
            Err(e) => {
                *ttl = e.ttl;
                result.newest_status = e.status;
                result.notes.push(e.reason.into());
                if let Some(retry_at) = e.retry_at { result.notes.push(format!("Registry retry after UTC epoch milliseconds: {retry_at}")); }
                result.repository_reason = Some("Registry metadata was unavailable; repository could not be determined.".into());
                None
            }
        }
    }

    fn endpoint(&self, provider: Provider, segments: &[&str], query: &[(&str,&str)]) -> Url {
        #[cfg(test)]
        let mut url = if let Some(base) = &self.test_base {
            base.join(&format!("{}/", provider.key())).expect("validated fixture URL")
        } else { Url::parse(PROVIDER_BASES[provider.index()]).expect("fixed URL") };
        #[cfg(not(test))]
        let mut url = Url::parse(PROVIDER_BASES[provider.index()]).expect("fixed URL");
        url.path_segments_mut().expect("provider URL is hierarchical").pop_if_empty().extend(segments.iter().copied());
        if !query.is_empty() { url.query_pairs_mut().extend_pairs(query.iter().copied()); }
        url
    }

    fn fetch(&self, provider: Provider, segments: &[&str], query: &[(&str,&str)], deadline: Instant) -> Result<Response, Failure> {
        let now = (self.clock)();
        if let Some(retry_at) = self.cache.lock().cooldowns.get(&provider).copied().filter(|at| *at > now) {
            return Err(Failure { status: InsightStatus::RateLimited, reason: "Provider rate limit; retry after the cooldown.", retry_at: Some(retry_at), ttl: retry_at - now });
        }
        let mut url = self.endpoint(provider, segments, query);
        let origin = url.clone();
        let mut redirects = 0;
        // Set once GitHub rejected the token: the same request is retried exactly once without credentials.
        let mut anonymous = false;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() { return Err(Failure::error("Package metadata lookup deadline exceeded.")); }
            let mut request = self.client.get(url.clone()).timeout(self.request_timeout.min(remaining));
            let mut sent_token = None;
            if provider == Provider::Github {
                request = request.header(header::ACCEPT, "application/vnd.github+json")
                    .header("X-GitHub-Api-Version", "2026-03-10");
                if !anonymous && (url.host_str() == Some("api.github.com") || self.fixture_credentials()) {
                    if let Some(token) = self.credential(false).token {
                        request = request.bearer_auth(&token);
                        sent_token = Some(token);
                    }
                }
            }
            let response = request.send().map_err(|e| if e.is_timeout() {
                Failure::error("Package metadata request timed out.")
            } else { Failure::error("Package metadata request failed.") })?;
            let status = response.status();
            let headers = response.headers().clone();
            if provider == Provider::Github { self.record_github_headers(&headers); }
            if status.is_redirection() {
                let next = headers.get(header::LOCATION).and_then(|v| v.to_str().ok()).and_then(|s| url.join(s).ok())
                    .ok_or_else(|| Failure::error("Invalid provider redirect."))?;
                if redirects == 3 || !self.allowed_redirect(&origin, &next) {
                    return Err(Failure::error("Provider redirect was blocked."));
                }
                redirects += 1;
                url = next;
                continue;
            }
            if status == StatusCode::UNAUTHORIZED {
                if let Some(token) = sent_token {
                    self.reject_credential(&token);
                    anonymous = true;
                    continue;
                }
            }
            if response.content_length().is_some_and(|n| n > MAX_BODY as u64) {
                return Err(Failure::error("Provider response exceeded 5 MiB."));
            }
            let mut body = Vec::with_capacity(response.content_length().unwrap_or(0).min(MAX_BODY as u64) as usize);
            response.take((MAX_BODY + 1) as u64).read_to_end(&mut body)
                .map_err(|_| Failure::error("Could not read provider response within its request deadline."))?;
            if body.len() > MAX_BODY { return Err(Failure::error("Provider response exceeded 5 MiB.")); }
            if Instant::now() >= deadline { return Err(Failure::error("Package metadata lookup deadline exceeded.")); }
            if status == StatusCode::TOO_MANY_REQUESTS || (status == StatusCode::FORBIDDEN && rate_limit_403(&headers, &body)) {
                let fallback = if provider == Provider::Github { HOUR } else { 60_000 };
                let retry_at = retry_at(&headers, now).filter(|at| *at > now).unwrap_or_else(|| now.saturating_add(fallback));
                let mut cache = self.cache.lock();
                cache.cooldowns.insert(provider, retry_at);
                self.persist_provider_state(&cache);
                drop(cache);
                return Err(Failure { status: InsightStatus::RateLimited, reason: "Provider rate limit; retry after the cooldown.", retry_at: Some(retry_at), ttl: retry_at.saturating_sub(now) });
            }
            if status == StatusCode::NOT_FOUND {
                return Err(Failure { status: InsightStatus::Unavailable, reason: "Provider has no public metadata for this package or repository (404).", retry_at: None, ttl: HOUR });
            }
            if !status.is_success() { return Err(Failure::error("Provider returned an unsuccessful HTTP status.")); }
            return Ok(Response { status, headers, body });
        }
    }

    fn fixture_credentials(&self) -> bool {
        #[cfg(test)]
        { self.test_base.is_some() }
        #[cfg(not(test))]
        { false }
    }

    fn record_github_headers(&self, headers: &header::HeaderMap) {
        if !["x-ratelimit-limit", "x-ratelimit-remaining", "x-ratelimit-reset"].iter().any(|key| headers.contains_key(*key)) { return; }
        let number = |key| headers.get(key).and_then(|v| v.to_str().ok()).and_then(|s| s.parse::<u64>().ok());
        let mut cache = self.cache.lock();
        if let Some(n) = number("x-ratelimit-limit") { cache.github.limit = Some(n); }
        if let Some(n) = number("x-ratelimit-remaining") { cache.github.remaining = Some(n); }
        if let Some(n) = number("x-ratelimit-reset") { cache.github.reset_at = Some(n.saturating_mul(1000)); }
        self.persist_provider_state(&cache);
    }

    fn persist_provider_state(&self, cache: &Cache) {
        let state = ProviderState { schema: 1, cooldowns: cache.cooldowns.iter().map(|(p,at)| (p.key().into(), *at)).collect(),
            github: cache.github.clone(), credential: cache.credential.clone(), github_since: cache.github_since };
        persist_json(&self.root.join("provider-state.json"), &state);
    }

    fn allowed_redirect(&self, origin: &Url, next: &Url) -> bool {
        let same_authority = origin.host_str() == next.host_str() && origin.port_or_known_default() == next.port_or_known_default()
            && next.username().is_empty() && next.password().is_none();
        #[cfg(test)]
        if self.test_base.is_some() { return same_authority && next.scheme() == "http"; }
        same_authority && next.scheme() == "https"
    }

    fn json(&self, provider: Provider, segments: &[&str], query: &[(&str,&str)], deadline: Instant) -> Result<Value, Failure> {
        let response = self.fetch(provider, segments, query, deadline)?;
        serde_json::from_slice(&response.body).map_err(|_| Failure::error("Provider returned invalid JSON."))
    }

    fn repository(&self, repo: &Repo, mode: GithubMode, refresh: bool, deadline: Instant, notes: &mut Vec<String>) -> Option<Entry<RepositoryInsights>> {
        // Notice a credential change before judging saved entries: a switch resets the budget and `github_since`.
        self.credential(false);
        let key = repo.key();
        let cached = self.cached_repository(&key);
        let since = self.cache.lock().github_since;
        if !refresh && cached.as_ref().is_some_and(|entry| !repository_due(entry, (self.clock)(), since)) { return cached; }
        let Some(_gate) = self.github_gate.try_lock_for(deadline.saturating_duration_since(Instant::now())) else {
            let error = Failure::error("Package metadata lookup deadline exceeded.");
            if let Some(old) = cached.filter(|entry| repository_good(&entry.value)) {
                notes.push(refresh_note(error.reason, old.saved_at));
                return Some(old);
            }
            return Some(repository_failure(repo, &error, (self.clock)()));
        };
        let cached = self.cached_repository(&key);
        let now = (self.clock)();
        let since = self.cache.lock().github_since;
        if !refresh && cached.as_ref().is_some_and(|entry| !repository_due(entry, now, since)) { return cached; }
        let blocked = {
            let cache = self.cache.lock();
            if let Some(at) = cache.cooldowns.get(&Provider::Github).copied().filter(|at| *at > now) {
                Some(Failure { status: InsightStatus::RateLimited, reason: "Provider rate limit; retry after the cooldown.",
                    retry_at: Some(at), ttl: at - now })
            } else if let Some(at) = cache.github.reset_at.filter(|at| *at > now)
                .filter(|_| cache.github.remaining.is_some_and(|n| n < 2 + if mode == GithubMode::Bulk { 15 } else { 5 })) {
                Some(Failure { status: InsightStatus::RateLimited, reason: "GitHub API budget reserved; resumes after the hourly reset.",
                    retry_at: Some(at), ttl: at - now })
            } else { None }
        };
        if let Some(error) = blocked {
            if let Some(old) = cached.filter(|entry| repository_good(&entry.value)) {
                notes.push(refresh_note(error.reason, old.saved_at));
                return Some(old);
            }
            // A mode-specific reservation is not a provider failure: Demand may still use its smaller reserve.
            return Some(repository_failure(repo, &error, now));
        }
        let mut ttl = DAY;
        let mut result = RepositoryInsights { url: repo.url(), owner_login: repo.owner.clone(), owner_type: OwnerType::Unknown,
            stars: Failure::unavailable("GitHub star count unavailable.").metric("GitHub"),
            contributors: Failure::unavailable("GitHub contributor count unavailable.").metric("GitHub"), checked_at: now };
        match self.json(Provider::Github, &["repos", &repo.owner, &repo.name], &[], deadline) {
            Ok(value) => {
                // Only accept a canonical identity with an exact GitHub URL and matching full_name.
                let canonical = value["full_name"].as_str().and_then(Repo::from_pair)
                    .zip(value["html_url"].as_str().and_then(normalize_repository))
                    .filter(|(a,b)| a.key() == b.key()).map(|(a,_)| a);
                if let Some(canonical) = canonical {
                    result.url = canonical.url();
                    result.owner_login = canonical.owner;
                    result.owner_type = match value["owner"]["type"].as_str() {
                        Some("User") => OwnerType::User, Some("Organization") => OwnerType::Organization, _ => OwnerType::Unknown,
                    };
                    result.stars = value["stargazers_count"].as_u64().map(|n| measured(n, "GitHub", None))
                        .unwrap_or_else(|| { ttl = ttl.min(TRANSIENT); Failure::error("GitHub did not return a valid star count.").metric("GitHub") });
                } else {
                    ttl = ttl.min(TRANSIENT);
                    result.stars = Failure::error("GitHub returned an invalid repository identity.").metric("GitHub");
                }
            }
            Err(e) => { ttl = ttl.min(e.ttl); result.stars = e.metric("GitHub"); }
        }
        result.contributors = match self.fetch(Provider::Github, &["repos", &repo.owner, &repo.name, "contributors"],
            &[("per_page", "1"), ("anon", "false")], deadline) {
            Ok(response) => {
                let count = if response.status == StatusCode::NO_CONTENT { Ok(0) } else {
                    serde_json::from_slice::<Value>(&response.body).map_err(|_| Failure::error("GitHub returned invalid contributor JSON."))
                        .and_then(|v| contributor_count(&response.headers, &v))
                };
                match count {
                    Ok(n) => measured(n, "GitHub", Some("All-time linked contributor accounts".into())),
                    Err(e) => { ttl = ttl.min(e.ttl); e.metric("GitHub") }
                }
            }
            Err(e) => { ttl = ttl.min(e.ttl); e.metric("GitHub") }
        };
        let saved_at = (self.clock)();
        result.checked_at = saved_at;
        let retry_at = if repository_good(&result) { None } else {
            result.stars.retry_at.into_iter().chain(result.contributors.retry_at).max().or_else(|| Some(saved_at.saturating_add(ttl)))
        };
        let entry = Entry { schema: 2, key: key.clone(), saved_at, retry_at, value: result };
        if let Some(old) = cached.filter(|old| repository_good(&old.value) && !repository_good(&entry.value)) {
            let reason = entry.value.stars.reason.as_deref().or(entry.value.contributors.reason.as_deref()).unwrap_or("GitHub metadata unavailable");
            notes.push(refresh_note(reason, old.saved_at));
            return Some(old);
        }
        self.persist("repositories", &entry);
        self.cache.lock().repositories.insert(key, entry.clone());
        Some(entry)
    }

    fn maven(&self, group: &str, artifact: &str, result: &mut PackageInsights, ttl: &mut u64, deadline: Instant) -> Option<Repo> {
        let path: Vec<&str> = std::iter::once("maven2").chain(group.split('.')).chain([artifact, "maven-metadata.xml"]).collect();
        let metadata = match self.fetch(Provider::Maven, &path, &[], deadline).and_then(|r| xml_text(r.body)) {
            Ok(value) => value,
            Err(e) => { self.apply_metadata(Err(e), result, ttl, |_| (None,None,None)); return None; }
        };
        let doc = match roxmltree::Document::parse(&metadata) {
            Ok(doc) => doc,
            Err(_) => { self.apply_metadata(Err(Failure::error("Maven Central returned invalid XML.")), result, ttl, |_| (None,None,None)); return None; }
        };
        let versioning = child(doc.root_element(), "versioning");
        let release = versioning.and_then(|n| text(n, "release")).filter(|v| valid_maven_part(v, false));
        let latest = versioning.and_then(|n| text(n, "latest")).filter(|v| valid_maven_part(v, false) && !v.to_ascii_uppercase().contains("SNAPSHOT"));
        let Some(version) = release.or(latest) else {
            self.apply_metadata(Err(Failure::unavailable("Maven Central has no recommended non-SNAPSHOT release.")), result, ttl, |_| (None,None,None)); return None;
        };
        result.newest_version = Some(version.clone());
        result.newest_status = InsightStatus::Available;
        let mut coordinate = (group.to_string(), artifact.to_string(), version);
        let mut seen = HashSet::new();
        seen.insert(coordinate.clone());
        // Initial release POM plus at most five distinct parent POMs.
        for depth in 0..=5 {
            let (g,a,v) = &coordinate;
            let filename = format!("{a}-{v}.pom");
            let path: Vec<&str> = std::iter::once("maven2").chain(g.split('.')).chain([a.as_str(), v.as_str(), filename.as_str()]).collect();
            let pom = match self.fetch(Provider::Maven, &path, &[], deadline).and_then(|r| xml_text(r.body)) {
                Ok(pom) => pom,
                Err(e) => { *ttl = (*ttl).min(e.ttl); result.repository_reason = Some(e.reason.into()); return None; }
            };
            let pom = match roxmltree::Document::parse(&pom) {
                Ok(pom) if pom.root_element().tag_name().name() == "project" => pom,
                _ => { *ttl = (*ttl).min(TRANSIENT); result.repository_reason = Some("Maven Central returned an invalid POM.".into()); return None; }
            };
            let project = pom.root_element();
            if depth == 0 { result.description = one_line(child(project, "description").and_then(|n| n.text())); }
            let props = pom_properties(project, &coordinate);
            let scm = child(project, "scm").and_then(|n| text(n, "url"));
            for candidate in [scm, text(project, "url")].into_iter().flatten() {
                if let Some(repo) = expand(&candidate, &props).and_then(|s| normalize_repository(&s)) { return Some(repo); }
            }
            let Some(parent) = child(project, "parent") else { return None; };
            let next = text(parent, "groupId").and_then(|s| expand(&s, &props))
                .zip(text(parent, "artifactId").and_then(|s| expand(&s, &props)))
                .zip(text(parent, "version").and_then(|s| expand(&s, &props)))
                .map(|((g,a),v)| (g,a,v));
            let Some(next) = next.filter(|(g,a,v)| valid_group(g) && valid_maven_part(a, true) && valid_maven_part(v, false)) else {
                result.repository_reason = Some("Maven parent reference could not be resolved safely.".into()); return None;
            };
            if !seen.insert(next.clone()) { result.repository_reason = Some("Maven parent POM cycle detected.".into()); return None; }
            if depth == 5 { result.repository_reason = Some("Maven parent POM lookup stopped after five parents.".into()); return None; }
            coordinate = next;
        }
        None
    }

    fn cached_package(&self, key: &str) -> Option<Entry<PackageInsights>> {
        if let Some(entry) = self.cache.lock().packages.get(key).cloned() { return Some(entry); }
        let entry: Entry<PackageInsights> = self.load("packages", key)?;
        if !valid_package_entry(&entry, key, (self.clock)()) { return None; }
        self.cache.lock().packages.insert(key.into(), entry.clone());
        Some(entry)
    }

    fn cached_repository(&self, key: &str) -> Option<Entry<RepositoryInsights>> {
        if let Some(entry) = self.cache.lock().repositories.get(key).cloned() { return Some(entry); }
        let path = self.entry_path("repositories", key);
        let value: Value = read_json(&path)?;
        let entry: Entry<RepositoryInsights> = if value["schema"].as_u64() == Some(1) {
            let legacy: LegacyRepositoryEntry = serde_json::from_value(value).ok()?;
            if legacy.schema != 1 || legacy.key != key || legacy.value["stars"]["status"] != "available" { return None; }
            let saved_at = legacy.expires_at.saturating_sub(DAY);
            let mut value = legacy.value;
            value.as_object_mut()?.insert("checkedAt".into(), Value::from(saved_at));
            let repository: RepositoryInsights = serde_json::from_value(value).ok()?;
            let entry = Entry { schema: 2, key: key.into(), saved_at, retry_at: None, value: repository };
            if !valid_repository(&entry.value) { return None; }
            self.persist("repositories", &entry);
            entry
        } else { serde_json::from_value(value).ok()? };
        if entry.schema != 2 || entry.key != key || entry.saved_at > (self.clock)()
            || entry.value.checked_at != entry.saved_at || !valid_repository(&entry.value)
            || (repository_good(&entry.value) && entry.retry_at.is_some())
            || (!repository_good(&entry.value) && entry.retry_at.is_none() && entry.value.stars.status != InsightStatus::Available) { return None; }
        self.cache.lock().repositories.insert(key.into(), entry.clone());
        Some(entry)
    }

    fn entry_path(&self, bucket: &str, key: &str) -> PathBuf {
        self.root.join(bucket).join(format!("{:x}.json", Sha256::digest(key.as_bytes())))
    }

    fn load<T: DeserializeOwned>(&self, bucket: &str, key: &str) -> Option<Entry<T>> {
        read_json(&self.entry_path(bucket, key))
    }

    fn persist<T: Serialize>(&self, bucket: &str, entry: &Entry<T>) {
        persist_json(&self.entry_path(bucket, &entry.key), entry);
    }
}

fn read_json<T: DeserializeOwned>(path: &std::path::Path) -> Option<T> {
    let file = fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() > MAX_BODY as u64 { return None; }
    let mut bytes = Vec::new();
    file.take((MAX_BODY + 1) as u64).read_to_end(&mut bytes).ok()?;
    if bytes.len() > MAX_BODY { return None; }
    serde_json::from_slice(&bytes).ok()
}

fn persist_json<T: Serialize>(path: &std::path::Path, value: &T) {
    let Some(parent) = path.parent() else { return; };
    if fs::create_dir_all(parent).is_err() { return; }
    let Ok(bytes) = serde_json::to_vec(value) else { return; };
    let temp = parent.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let written = fs::OpenOptions::new().write(true).create_new(true).open(&temp).and_then(|mut f| {
        f.write_all(&bytes)?;
        f.sync_all()
    });
    if written.is_ok() { let _ = atomic_replace(&temp, path); }
    let _ = fs::remove_file(temp);
}

fn package_good(value: &PackageInsights) -> bool { value.newest_status == InsightStatus::Available }
fn repository_good(value: &RepositoryInsights) -> bool {
    [&value.stars, &value.contributors].iter().all(|metric| !matches!(metric.status, InsightStatus::Error | InsightStatus::RateLimited))
}
fn entry_due<T>(entry: &Entry<T>, now: u64) -> bool { entry.retry_at.is_some_and(|at| at <= now) }
/// An unsuccessful repository entry saved before the credential kind changed is retried at once, whatever its backoff.
fn repository_due(entry: &Entry<RepositoryInsights>, now: u64, since: u64) -> bool {
    entry_due(entry, now) || (!repository_good(&entry.value) && entry.saved_at < since)
}
fn refresh_note(reason: &str, saved_at: u64) -> String {
    format!("Refresh failed ({reason}); showing data saved {saved_at} (UTC epoch ms).")
}
fn repository_failure(repo: &Repo, error: &Failure, now: u64) -> Entry<RepositoryInsights> {
    Entry { schema: 2, key: repo.key(), saved_at: now, retry_at: error.retry_at.or_else(|| Some(now.saturating_add(error.ttl))),
        value: RepositoryInsights { url: repo.url(), owner_login: repo.owner.clone(), owner_type: OwnerType::Unknown,
            stars: error.metric("GitHub"), contributors: error.metric("GitHub"), checked_at: now } }
}

fn valid_token(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty() && value.len() <= 255 && value.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))).then(|| value.to_string())
}
const CREDENTIAL_RETRY: u64 = 30_000;
const GH_DEADLINE: Duration = Duration::from_secs(5);
const GH_NOT_INSTALLED: &str = "GitHub CLI (gh) was not found. Install it (https://cli.github.com), run `gh auth login`, then press Recheck.";
fn gh_not_logged_in(program: &str) -> String {
    let wsl = if cfg!(windows) { " (a login made inside WSL is not shared with this app)" } else { "" };
    format!("GitHub CLI ({program}) is installed but not logged in to github.com. Run `gh auth login` in a normal Windows terminal as this user{wsl}, then press Recheck.")
}
const REJECTED_TOKEN: &str = "GitHub rejected the saved token (401). Run `gh auth login` again or replace GITHUB_TOKEN/GH_TOKEN, then press Recheck.";

/// The token is never printed: `Debug` redacts it.
#[derive(Clone)]
struct Credential { token: Option<String>, source: Option<&'static str>, problem: Option<String> }
impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential").field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("source", &self.source).field("problem", &self.problem).finish()
    }
}

#[derive(Default)]
struct CredentialState { current: Option<Credential>, probed_at: u64 }

enum GhOutcome { Token(String), NotInstalled, NotLoggedIn(String), Unusable(&'static str) }
impl std::fmt::Debug for GhOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Token(_) => f.write_str("Token(<redacted>)"),
            Self::NotInstalled => f.write_str("NotInstalled"),
            Self::NotLoggedIn(program) => f.debug_tuple("NotLoggedIn").field(program).finish(),
            Self::Unusable(message) => f.debug_tuple("Unusable").field(message).finish(),
        }
    }
}

/// `GITHUB_TOKEN`, then `GH_TOKEN`, then the GitHub CLI. Unset or blank variables are skipped; an unusable value is
/// ignored and only reported when nothing else yields a token.
fn resolve_credential(env: impl Fn(&str) -> Option<String>, run_gh: impl FnOnce() -> GhOutcome) -> Credential {
    let mut ignored = None;
    for name in ["GITHUB_TOKEN", "GH_TOKEN"] {
        let Some(raw) = env(name).filter(|value| !value.trim().is_empty()) else { continue; };
        match valid_token(&raw) {
            Some(token) => return Credential { token: Some(token), source: Some(name), problem: None },
            None => { ignored.get_or_insert(name); }
        }
    }
    let problem = match run_gh() {
        GhOutcome::Token(raw) => match valid_token(&raw) {
            Some(token) => return Credential { token: Some(token), source: Some("gh"), problem: None },
            None => "GitHub CLI returned an unusable token.".to_string(),
        },
        GhOutcome::NotInstalled => GH_NOT_INSTALLED.to_string(),
        GhOutcome::NotLoggedIn(program) => gh_not_logged_in(&program),
        GhOutcome::Unusable(message) => message.to_string(),
    };
    let problem = ignored.map(|name| format!("{name} has an unsupported format and was ignored.")).unwrap_or(problem);
    Credential { token: None, source: None, problem: Some(problem) }
}

/// Process environment first; on Windows a variable set after the app was started (Explorer's snapshot is stale)
/// is read from the user's registry environment.
fn credential_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.trim().is_empty()).or_else(|| user_environment(name))
}

#[cfg(windows)]
fn user_environment(name: &str) -> Option<String> {
    let program = std::env::var_os("SystemRoot").map(|root| PathBuf::from(root).join("System32").join("reg.exe"))
        .unwrap_or_else(|| PathBuf::from("reg.exe"));
    let mut command = Command::new(program);
    command.args(["query", r"HKCU\Environment", "/v", name]);
    let (success, bytes) = run_bounded(command, Duration::from_secs(3), 16 * 1024).ok()?;
    if !success { return None; }
    parse_reg_value(&String::from_utf8_lossy(&bytes), name)
}
#[cfg(not(windows))]
fn user_environment(_name: &str) -> Option<String> { None }

/// Value of `name` in `reg query` output: `    NAME    REG_SZ    value`.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn parse_reg_value(output: &str, name: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let line = line.trim();
        let head = line.get(..name.len()).filter(|head| head.eq_ignore_ascii_case(name))?;
        let rest = &line[head.len()..];
        if !rest.starts_with(char::is_whitespace) { return None; }
        let rest = rest.trim_start();
        let rest = rest.strip_prefix("REG_EXPAND_SZ").or_else(|| rest.strip_prefix("REG_SZ"))?;
        if !rest.is_empty() && !rest.starts_with(char::is_whitespace) { return None; }
        let value = rest.trim();
        (!value.is_empty()).then(|| value.to_string())
    })
}

/// `gh` from the inherited PATH first, then the usual per-user and machine-wide install locations, because an app
/// started from the desktop inherits an environment that can predate the install.
fn gh_programs(env: &dyn Fn(&str) -> Option<String>) -> Vec<OsString> {
    let mut programs = vec![OsString::from("gh")];
    #[cfg(windows)]
    {
        let under = |variable: &str, rest: &str| env(variable).filter(|base| !base.is_empty())
            .map(|base| PathBuf::from(base).join(rest).into_os_string());
        programs.extend([
            under("LOCALAPPDATA", r"Programs\GitHub CLI\bin\gh.exe"),
            under("ProgramFiles", r"GitHub CLI\gh.exe"),
            under("ProgramFiles(x86)", r"GitHub CLI\gh.exe"),
            under("LOCALAPPDATA", r"Microsoft\WinGet\Links\gh.exe"),
            under("USERPROFILE", r"scoop\shims\gh.exe"),
            under("ProgramData", r"chocolatey\bin\gh.exe"),
        ].into_iter().flatten());
    }
    #[cfg(not(windows))]
    {
        let _ = env;
        programs.extend(["/usr/local/bin/gh", "/opt/homebrew/bin/gh", "/usr/bin/gh"].map(OsString::from));
    }
    programs
}

enum RunError { NotFound, Spawn, Timeout, Read }

/// Runs a console helper with null stdin/stderr and at most `limit` bytes of stdout; returns (success, stdout).
fn run_bounded(mut command: Command, timeout: Duration, limit: u64) -> Result<(bool, Vec<u8>), RunError> {
    command.stdin(Stdio::null()).stderr(Stdio::null()).stdout(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command.spawn().map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { RunError::NotFound } else { RunError::Spawn })?;
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(RunError::Spawn);
    };
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = (&mut stdout).take(limit).read_to_end(&mut bytes);
        (result, bytes)
    });
    let deadline = Instant::now() + timeout;
    let success = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => { let _ = child.kill(); let _ = child.wait(); return Err(RunError::Timeout); }
            Err(_) => { let _ = child.kill(); let _ = child.wait(); return Err(RunError::Spawn); }
        }
    };
    if !success { return Ok((false, Vec::new())); }
    let (result, bytes) = reader.join().map_err(|_| RunError::Read)?;
    result.map_err(|_| RunError::Read)?;
    Ok((true, bytes))
}

fn run_gh_token() -> GhOutcome {
    for program in gh_programs(&|name| std::env::var(name).ok()) {
        let mut command = Command::new(&program);
        command.args(["auth", "token", "--hostname", "github.com"])
            .env("GH_PROMPT_DISABLED", "1").env("GH_NO_UPDATE_NOTIFIER", "1");
        return match run_bounded(command, GH_DEADLINE, 1024) {
            Err(RunError::NotFound) => continue,
            Err(RunError::Spawn) => GhOutcome::Unusable("GitHub CLI could not be started."),
            Err(RunError::Timeout) => GhOutcome::Unusable("GitHub CLI did not answer within 5 seconds."),
            Err(RunError::Read) => GhOutcome::Unusable("GitHub CLI returned an unusable token."),
            Ok((false, _)) => GhOutcome::NotLoggedIn(program.to_string_lossy().into_owned()),
            Ok((true, bytes)) => match std::str::from_utf8(&bytes).ok().and_then(valid_token) {
                Some(token) => GhOutcome::Token(token),
                None => GhOutcome::Unusable("GitHub CLI returned an unusable token."),
            },
        };
    }
    GhOutcome::NotInstalled
}

fn one_line(value: Option<&str>) -> Option<String> {
    let mut result = String::new();
    let mut whitespace = false;
    let mut scalars = 0;
    for c in value?.chars() {
        if c.is_whitespace() { whitespace = !result.is_empty(); continue; }
        if c.is_control() { continue; }
        if whitespace {
            if scalars == 160 { result.push('…'); return Some(result); }
            result.push(' ');
            scalars += 1;
            whitespace = false;
        }
        if scalars == 160 { result.push('…'); return Some(result); }
        result.push(c);
        scalars += 1;
    }
    (!result.is_empty()).then_some(result)
}

#[cfg(not(windows))]
fn atomic_replace(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> { fs::rename(from, to) }
#[cfg(windows)]
fn atomic_replace(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    // MoveFileExW avoids a delete-then-rename gap and works without additional Windows crate features.
    #[link(name = "kernel32")]
    extern "system" { fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32; }
    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0x1 | 0x8) } == 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

fn epoch_ms() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis().min(u64::MAX as u128) as u64 }
fn measured(value: u64, source: &str, period: Option<String>) -> CountMetric {
    CountMetric { value: Some(value), status: InsightStatus::Available, source: source.into(), period, reason: None, retry_at: None }
}
fn clean_text(value: Option<&str>, max: usize) -> Option<String> {
    value.map(str::trim).filter(|s| !s.is_empty() && s.len() <= max && !s.chars().any(char::is_control)).map(str::to_string)
}
fn valid_date(s: &str) -> bool {
    s.len() == 10 && s.bytes().enumerate().all(|(i,c)| if i == 4 || i == 7 { c == b'-' } else { c.is_ascii_digit() })
        && s[5..7].parse::<u8>().is_ok_and(|n| (1..=12).contains(&n)) && s[8..10].parse::<u8>().is_ok_and(|n| (1..=31).contains(&n))
}

#[derive(Debug)]
enum Package { Npm(String), Pypi(String), Cargo(String), Maven(String,String) }
impl Package {
    fn parse(id: &str) -> Option<Self> {
        if id.len() > 512 || id.chars().any(char::is_control) { return None; }
        let (ecosystem,name) = id.split_once(':')?;
        match ecosystem {
            "npm" => {
                if name.len() > 214 { return None; }
                let valid = |s: &str| !s.is_empty() && s.len() <= 214 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.') && !s.starts_with('.') && !s.starts_with('_');
                if let Some(scoped) = name.strip_prefix('@') {
                    let (scope,pkg) = scoped.split_once('/')?;
                    (valid(scope) && valid(pkg)).then(|| Self::Npm(name.into()))
                } else { valid(name).then(|| Self::Npm(name.into())) }
            }
            "pypi" => {
                if name.is_empty() || name.len() > 214 || !name.as_bytes().first()?.is_ascii_alphanumeric()
                    || !name.as_bytes().last()?.is_ascii_alphanumeric()
                    || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)) { return None; }
                let mut normalized = String::new();
                for c in name.chars() {
                    if matches!(c, '.' | '_' | '-') { if !normalized.ends_with('-') { normalized.push('-'); } }
                    else { normalized.push(c.to_ascii_lowercase()); }
                }
                Some(Self::Pypi(normalized))
            }
            "cargo" => (!name.is_empty() && name.len() <= 64 && name.as_bytes()[0].is_ascii_alphabetic()
                && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')).then(|| Self::Cargo(name.into())),
            "maven" => {
                let (group,artifact) = name.split_once(':')?;
                (valid_group(group) && valid_maven_part(artifact, true)).then(|| Self::Maven(group.into(),artifact.into()))
            }
            _ => None,
        }
    }
    fn source(&self) -> &'static str { match self { Self::Npm(_) => "npm", Self::Pypi(_) => "PyPI", Self::Cargo(_) => "crates.io", Self::Maven(..) => "Maven Central" } }
    fn page(&self) -> String { match self {
        Self::Npm(name) => format!("https://www.npmjs.com/package/{name}"),
        Self::Pypi(name) => format!("https://pypi.org/project/{name}/"),
        Self::Cargo(name) => format!("https://crates.io/crates/{name}"),
        Self::Maven(g,a) => format!("https://central.sonatype.com/artifact/{g}/{a}"),
    } }
    fn deadline_result(&self, id: &str, now: u64) -> PackageInsights {
        PackageInsights { id: id.into(), registry_url: self.page(), newest_version: None, newest_status: InsightStatus::Error,
            description: None, repository_url: None, github_pending: false,
            repository: None, repository_reason: Some("Package metadata lookup deadline exceeded while waiting for another lookup.".into()),
            downloads: Failure::error("Package metadata lookup deadline exceeded.").metric(self.source()), checked_at: now,
            notes: vec!["Package metadata lookup deadline exceeded.".into()] }
    }
}
fn valid_maven_part(s: &str, coordinate: bool) -> bool {
    !s.is_empty() && s.len() <= 200 && s != "." && s != ".." && !s.contains("..")
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b) || (!coordinate && b == b'+'))
}
fn valid_group(s: &str) -> bool { s.len() <= 200 && s.split('.').all(|s| valid_maven_part(s, true)) }

#[derive(Debug, Clone, PartialEq, Eq)]
struct Repo { owner: String, name: String }
impl Repo {
    fn from_pair(s: &str) -> Option<Self> {
        let mut parts = s.split('/');
        let owner = parts.next()?;
        let name = parts.next()?;
        if parts.next().is_some() || !repo_component(owner) || !repo_component(name) { return None; }
        Some(Self { owner: owner.into(), name: name.into() })
    }
    fn key(&self) -> String { format!("{}/{}", self.owner.to_ascii_lowercase(), self.name.to_ascii_lowercase()) }
    fn url(&self) -> String { format!("https://github.com/{}/{}", self.owner,self.name) }
}
fn repo_component(s: &str) -> bool {
    !s.is_empty() && s.len() <= 100 && s != "." && s != ".." && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}
fn normalize_repository(input: &str) -> Option<Repo> {
    if input.len() > 2048 || input.chars().any(char::is_control) || input.contains('\\') || input.contains('%') { return None; }
    let input = input.trim();
    let raw = if let Some(raw) = input.strip_prefix("github:").or_else(|| input.strip_prefix("git@github.com:")) {
        raw.to_string()
    } else {
        let input = input.strip_prefix("git+").unwrap_or(input);
        // Reject traversal before URL's normalization can erase it.
        if input.split(['/', '?', '#']).any(|p| p == "." || p == "..") { return None; }
        let url = Url::parse(input).ok()?;
        if !matches!(url.scheme(), "http" | "https" | "ssh" | "git") || url.host_str()? != "github.com"
            || url.port().is_some() || url.password().is_some() || (!url.username().is_empty() && url.username() != "git") { return None; }
        url.path().strip_prefix('/').unwrap_or(url.path()).to_string()
    };
    let raw = raw.split(['#','?']).next()?;
    if raw.split('/').any(|p| p == "." || p == "..") { return None; }
    let mut parts = raw.split('/');
    let owner = parts.next()?;
    if RESERVED_OWNERS.contains(&owner.to_ascii_lowercase().as_str()) { return None; }
    let name = parts.next()?;
    let name = name.strip_suffix(".git").unwrap_or(name);
    Repo::from_pair(&format!("{owner}/{name}"))
}
const RESERVED_OWNERS: [&str; 21] = [
    "sponsors", "orgs", "apps", "marketplace", "features", "topics", "settings", "login", "about", "pricing",
    "explore", "collections", "enterprise", "organizations", "notifications", "search", "security", "new", "join",
    "pulls", "issues",
];
fn repository_field(value: &Value) -> Option<Repo> {
    value.as_str().or_else(|| value["url"].as_str()).and_then(normalize_repository)
}
fn pypi_repository(info: &Value) -> Option<Repo> {
    if let Some(urls) = info["project_urls"].as_object() {
        for preferred in ["source", "source code", "repository", "code"] {
            for (key,value) in urls {
                if key.eq_ignore_ascii_case(preferred) {
                    if let Some(repo) = value.as_str().and_then(normalize_repository) { return Some(repo); }
                }
            }
        }
        for value in urls.values() { if let Some(repo) = value.as_str().and_then(normalize_repository) { return Some(repo); } }
    }
    info["home_page"].as_str().and_then(normalize_repository)
}

fn contributor_count(headers: &header::HeaderMap, body: &Value) -> Result<u64, Failure> {
    let array = body.as_array().ok_or_else(|| Failure::error("GitHub returned invalid contributor data."))?;
    if array.len() > 1 { return Err(Failure::error("GitHub contributor page did not respect the requested page size.")); }
    let mut next = false;
    let mut last = None;
    for value in headers.get_all(header::LINK).iter() {
        let value = value.to_str().map_err(|_| Failure::error("GitHub returned invalid contributor pagination."))?;
        for link in value.split(',') {
            let mut fields = link.trim().split(';');
            let target = fields.next().unwrap_or("").trim();
            let rel = fields.find_map(|p| p.trim().strip_prefix("rel=").map(|p| p.trim_matches('"')));
            let Some(rel) = rel else { continue; };
            if rel.split_whitespace().any(|s| s == "next") { next = true; }
            if rel.split_whitespace().any(|s| s == "last") {
                let url = target.strip_prefix('<').and_then(|s| s.strip_suffix('>')).and_then(|s| Url::parse(s).ok())
                    .filter(|u| u.scheme() == "https" && u.host_str() == Some("api.github.com") && u.username().is_empty() && u.password().is_none() && u.port().is_none())
                    .ok_or_else(|| Failure::error("GitHub returned invalid contributor pagination."))?;
                let pages: Vec<_> = url.query_pairs().filter(|(k,_)| k == "page").map(|(_,v)| v.into_owned()).collect();
                if pages.len() != 1 { return Err(Failure::error("GitHub returned invalid contributor pagination.")); }
                let page = pages[0].bytes().all(|b| b.is_ascii_digit()).then(|| pages[0].parse::<u64>().ok()).flatten().filter(|p| *p > 0)
                    .ok_or_else(|| Failure::error("GitHub returned invalid contributor pagination."))?;
                if last.replace(page).is_some() { return Err(Failure::error("GitHub returned ambiguous contributor pagination.")); }
            }
        }
    }
    if let Some(last) = last { Ok(last) }
    else if next { Err(Failure::unavailable("GitHub contributor total is unknown: pagination has no calculable last page.")) }
    else { Ok(array.len() as u64) }
}
fn rate_limit_403(headers: &header::HeaderMap, body: &[u8]) -> bool {
    headers.contains_key(header::RETRY_AFTER) || headers.get("x-ratelimit-remaining").is_some_and(|v| v.as_bytes() == b"0")
        || serde_json::from_slice::<Value>(body).ok().and_then(|v| v["message"].as_str().map(|s| s.to_ascii_lowercase()))
            .is_some_and(|s| s.contains("api rate limit") || s.contains("secondary rate limit"))
}
fn retry_at(headers: &header::HeaderMap, now: u64) -> Option<u64> {
    // Retry-After is authoritative when present; an invalid value falls back to the provider default.
    if let Some(value) = headers.get(header::RETRY_AFTER) {
        let s = value.to_str().ok()?.trim();
        return s.parse::<u64>().ok().map(|n| now.saturating_add(n.saturating_mul(1000))).or_else(|| http_date_ms(s));
    }
    // The reset time is meaningful only for an exhausted primary limit.
    if headers.get("x-ratelimit-remaining").is_some_and(|v| v.as_bytes() == b"0") {
        return headers.get("x-ratelimit-reset").and_then(|v| v.to_str().ok()).and_then(|s| s.trim().parse::<u64>().ok()).map(|n| n.saturating_mul(1000));
    }
    None
}
// IMF-fixdate used by Retry-After; invalid dates are ignored in favor of the provider fallback.
fn http_date_ms(s: &str) -> Option<u64> {
    let parts: Vec<_> = s.split_whitespace().collect();
    if parts.len() != 6 || parts[5] != "GMT" { return None; }
    let day: i64 = parts[1].parse().ok()?;
    let month = ["Jan","Feb","Mar","Apr","May","Jun","Jul","Aug","Sep","Oct","Nov","Dec"].iter().position(|m| *m == parts[2])? as i64 + 1;
    let mut year: i64 = parts[3].parse().ok()?;
    let time: Vec<_> = parts[4].split(':').map(str::parse::<u64>).collect::<Result<_,_>>().ok()?;
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = [31,if leap {29} else {28},31,30,31,30,31,31,30,31,30,31][month as usize - 1];
    if !(1970..=9999).contains(&year) || day < 1 || day > days_in_month || time.len() != 3 || time[0] > 23 || time[1] > 59 || time[2] > 59 { return None; }
    year -= if month <= 2 { 1 } else { 0 };
    let era = year / 400;
    let yoe = year - era * 400;
    let mp = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let days = era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468;
    if days < 0 { return None; }
    Some(((days as u64 * 24 + time[0]) * 3600 + time[1] * 60 + time[2]) * 1000)
}

fn xml_text(bytes: Vec<u8>) -> Result<String, Failure> {
    // Only ASCII coordinates and SCM URLs are consumed, so legacy single-byte POM text must not discard the document.
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if text.contains("<!DOCTYPE") || text.contains("<!ENTITY") { return Err(Failure::error("Maven Central XML declarations are not supported.")); }
    Ok(text)
}
fn child<'a,'i>(node: roxmltree::Node<'a,'i>, name: &str) -> Option<roxmltree::Node<'a,'i>> {
    node.children().find(|n| n.is_element() && n.tag_name().name() == name)
}
fn text(node: roxmltree::Node<'_,'_>, name: &str) -> Option<String> {
    child(node,name).and_then(|n| clean_text(n.text(),2048))
}
fn pom_properties(project: roxmltree::Node<'_,'_>, coordinate: &(String,String,String)) -> HashMap<String,String> {
    let mut props = HashMap::new();
    if let Some(properties) = child(project,"properties") {
        for n in properties.children().filter(|n| n.is_element()) {
            if let Some(value) = clean_text(n.text(),2048) { props.insert(n.tag_name().name().into(),value); }
        }
    }
    for (name,value) in [("groupId", &coordinate.0), ("artifactId", &coordinate.1), ("version", &coordinate.2)] {
        props.insert(name.into(),value.clone());
        props.insert(format!("project.{name}"),value.clone());
        props.insert(format!("pom.{name}"),value.clone());
    }
    if let Some(parent) = child(project,"parent") {
        for name in ["groupId","artifactId","version"] {
            if let Some(value) = text(parent,name) {
                props.insert(format!("parent.{name}"),value.clone());
                props.insert(format!("project.parent.{name}"),value.clone());
                props.insert(format!("pom.parent.{name}"),value);
            }
        }
    }
    props
}
fn expand(input: &str, props: &HashMap<String,String>) -> Option<String> {
    let mut value = input.to_string();
    for _ in 0..16 {
        let Some(start) = value.find("${") else { return (!value.contains(['{','}']) && value.len() <= 2048).then_some(value); };
        let end = value[start+2..].find('}')? + start + 2;
        let replacement = props.get(&value[start+2..end])?;
        value.replace_range(start..=end,replacement);
        if value.len() > 2048 { return None; }
    }
    None
}
fn valid_metric(metric: &CountMetric) -> bool {
    (metric.status == InsightStatus::Available) == metric.value.is_some()
        && clean_text(Some(&metric.source),100).is_some()
        && metric.period.as_deref().is_none_or(|s| clean_text(Some(s),256).is_some())
        && metric.reason.as_deref().is_none_or(|s| clean_text(Some(s),1024).is_some())
}
fn valid_repository(repo: &RepositoryInsights) -> bool {
    normalize_repository(&repo.url).is_some_and(|r| r.url() == repo.url && r.owner == repo.owner_login)
        && valid_metric(&repo.stars) && valid_metric(&repo.contributors)
}
fn valid_package_entry(entry: &Entry<PackageInsights>, key: &str, now: u64) -> bool {
    let Some(package) = Package::parse(key) else { return false; };
    entry.schema == 2 && entry.key == key && entry.value.id == key && entry.saved_at <= now
        && entry.value.checked_at == entry.saved_at && (entry.retry_at.is_none() == package_good(&entry.value))
        && entry.value.registry_url == package.page() && valid_metric(&entry.value.downloads)
        && (entry.value.newest_status == InsightStatus::Available) == entry.value.newest_version.is_some()
        && entry.value.newest_version.as_deref().is_none_or(|s| clean_text(Some(s),256).is_some())
        && entry.value.repository.is_none() && !entry.value.github_pending
        && entry.value.repository_url.as_deref().is_none_or(|s| normalize_repository(s).is_some_and(|repo| repo.url() == s))
        && entry.value.description.as_ref().is_none_or(|s| !s.is_empty() && s.chars().count() <= 161 && !s.chars().any(char::is_control))
        && entry.value.repository_reason.as_deref().is_none_or(|s| clean_text(Some(s),1024).is_some())
        && entry.value.notes.len() <= 20 && entry.value.notes.iter().all(|s| clean_text(Some(s),2048).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::thread;

    struct Reply { status: u16, headers: Vec<(String,String)>, body: Vec<u8>, delay: Duration, body_delay: Duration, close_delimited: bool }
    impl Reply {
        fn json(value: Value) -> Self { Self::body(200, serde_json::to_vec(&value).unwrap()) }
        fn xml(value: &str) -> Self { Self::body(200, value.as_bytes().to_vec()) }
        fn body(status: u16, body: Vec<u8>) -> Self { Self { status, headers: vec![], body, delay: Duration::ZERO, body_delay: Duration::ZERO, close_delimited: false } }
        fn header(mut self, key: &str, value: &str) -> Self { self.headers.push((key.into(),value.into())); self }
        fn delayed(mut self, delay: Duration) -> Self { self.delay = delay; self }
        fn slow_body(mut self) -> Self { self.body_delay = Duration::from_millis(200); self }
        fn close_delimited(mut self) -> Self { self.close_delimited = true; self }
    }
    struct Fixture {
        base: String,
        root: PathBuf,
        requests: Arc<Mutex<Vec<String>>>,
        now: Arc<AtomicU64>,
        stop: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }
    impl Fixture {
        fn new(handler: impl Fn(&str) -> Reply + Send + Sync + 'static) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let address = listener.local_addr().unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let (worker_stop, worker_requests) = (stop.clone(), requests.clone());
            let handler = Arc::new(handler);
            let thread = thread::spawn(move || {
                let mut workers = Vec::new();
                while !worker_stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut stream,_)) => {
                            let (handler,requests) = (handler.clone(),worker_requests.clone());
                            workers.push(thread::spawn(move || {
                                stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                                // Windows accepted sockets can inherit the listener's nonblocking mode.
                                stream.set_nonblocking(false).unwrap();
                                let mut bytes = Vec::new();
                                let mut buffer = [0u8;1024];
                                while bytes.len() < 16_384 && !bytes.windows(4).any(|w| w == b"\r\n\r\n") {
                                    match stream.read(&mut buffer) { Ok(0) | Err(_) => return, Ok(n) => bytes.extend_from_slice(&buffer[..n]) }
                                }
                                let request = String::from_utf8(bytes).unwrap();
                                let path = request.lines().next().unwrap().split_whitespace().nth(1).unwrap().to_string();
                                requests.lock().push(request);
                                let reply = handler(&path);
                                thread::sleep(reply.delay);
                                let mut response = format!("HTTP/1.1 {} Fixture\r\nConnection: close\r\n",reply.status);
                                if !reply.close_delimited { response.push_str(&format!("Content-Length: {}\r\n",reply.body.len())); }
                                for (key,value) in reply.headers { response.push_str(&format!("{key}: {value}\r\n")); }
                                response.push_str("\r\n");
                                if stream.write_all(response.as_bytes()).is_ok() {
                                    thread::sleep(reply.body_delay);
                                    let _ = stream.write_all(&reply.body);
                                }
                            }));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(1)),
                        Err(e) => panic!("fixture accept: {e}"),
                    }
                }
                for worker in workers { worker.join().unwrap(); }
            });
            Self { base: format!("http://{address}/"), root: std::env::temp_dir().join(format!("omp-insights-{}",uuid::Uuid::new_v4())),
                requests, now: Arc::new(AtomicU64::new(1_800_000_000_000)), stop, thread: Some(thread) }
        }
        fn service(&self) -> PackageInsightsService {
            self.service_with(None,Duration::from_secs(2),Duration::from_secs(5))
        }
        fn service_with(&self, token: Option<String>, request: Duration, lookup: Duration) -> PackageInsightsService {
            PackageInsightsService::with_test_providers(self.root.clone(),&self.base,token,self.now.clone(),request,lookup).unwrap()
        }
        fn paths(&self) -> Vec<String> { self.requests.lock().iter().map(|r| r.lines().next().unwrap().split_whitespace().nth(1).unwrap().to_string()).collect() }
        fn advance(&self, millis: u64) { self.now.fetch_add(millis,Ordering::SeqCst); }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.stop.store(true,Ordering::SeqCst);
            if let Some(thread) = self.thread.take() { thread.join().unwrap(); }
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    fn github(path: &str) -> Reply {
        if path.ends_with("contributors?per_page=1&anon=false") { Reply::json(json!([{"login":"linked"}])) }
        else { Reply::json(json!({"full_name":"Owner/Repo","html_url":"https://github.com/Owner/Repo","owner":{"login":"Owner","type":"Organization"},"stargazers_count":0})) }
    }
    fn npm(path: &str) -> Reply {
        if path.starts_with("/npmRegistry/") { Reply::json(json!({"version":"3.1.0-rc.1","repository":{"url":"git+https://github.com/Owner/Repo.git"}})) }
        else if path.starts_with("/npmDownloads/") { Reply::json(json!({"downloads":0,"start":"2026-09-01","end":"2026-09-30"})) }
        else if path.starts_with("/github/") { github(path) }
        else { Reply::body(404,vec![]) }
    }

    #[test]
    fn scoped_npm_real_zero_period_recommended_release_and_typed_cache() {
        let f = Fixture::new(npm);
        let service = f.service();
        let result = service.lookup("npm:@scope/pkg", GithubMode::Demand, false).unwrap();
        assert_eq!(result.newest_version.as_deref(),Some("3.1.0-rc.1"));
        assert_eq!(result.downloads.value,Some(0));
        assert!(result.downloads.period.as_ref().unwrap().contains("2026-09-01–2026-09-30"));
        let repo = result.repository.unwrap();
        assert_eq!(repo.stars.value,Some(0));
        assert_eq!(repo.contributors.value,Some(1));
        assert_eq!(repo.owner_type,OwnerType::Organization);
        let paths = f.paths();
        assert_eq!(paths.len(),4);
        assert!(paths[0].eq_ignore_ascii_case("/npmRegistry/@scope%2Fpkg/latest"));
        assert!(paths[1].eq_ignore_ascii_case("/npmDownloads/downloads/point/last-month/@scope%2Fpkg"));
        let again = service.lookup("npm:@scope/pkg", GithubMode::Demand, false).unwrap();
        assert_eq!(again.downloads.value,Some(0));
        assert_eq!(f.paths().len(),4);
        drop(service);
        let disk_hit = f.service().lookup("npm:@scope/pkg", GithubMode::Demand, false).unwrap();
        assert_eq!(disk_hit.newest_status,InsightStatus::Available);
        assert_eq!(f.paths().len(),4);
    }

    #[test]
    fn repository_url_forms_and_boundary_rejections() {
        for input in ["https://github.com/Owner/Repo.git#readme","http://github.com/Owner/Repo/tree/main",
            "git+https://github.com/Owner/Repo.git","git+ssh://git@github.com/Owner/Repo.git",
            "ssh://git@github.com/Owner/Repo.git","git://github.com/Owner/Repo.git",
            "git@github.com:Owner/Repo.git","github:Owner/Repo"] {
            assert_eq!(normalize_repository(input).unwrap().key(),"owner/repo","{input}");
        }
        for input in ["https://github.com.evil/owner/repo","https://evil/github.com/owner/repo",
            "https://github.com/owner/../repo","github:owner/..","github:/repo","github:owner/",
            "https://github.com/owner/%2e%2e/repo","https://github.com/owner/re%2fpo","https://github.com/owner\\repo",
            "https://github.com/owner/repo\n","https://user:secret@github.com/owner/repo","file://github.com/owner/repo"] {
            assert!(normalize_repository(input).is_none(),"{input:?}");
        }
    }

    #[test]
    fn invalid_identifiers_never_reach_network() {
        let f = Fixture::new(|_| panic!("invalid id reached provider"));
        let service = f.service();
        let mut ids = vec!["project".to_string(),"npm:".into(),"npm:../x".into(),"npm:@scope/pkg/extra".into(),
            "npm:https://evil/x".into(),"pypi:a/b".into(),"pypi:-bad".into(),"cargo:serde?x".into(),
            "maven:group:artifact:extra".into(),"maven:group..evil:artifact".into(),"maven:group:../artifact".into(),
            "maven:group:artifact%2fother".into(),"unknown:name".into(),"npm:foo\nbar".into()];
        ids.push(format!("npm:{}","a".repeat(215)));
        for id in ids { assert_eq!(service.lookup(&id, GithubMode::Skip, false).unwrap_err(),"Invalid package id","{id}"); }
        assert!(f.paths().is_empty());
    }

    #[test]
    fn pypi_normalization_source_priority_and_deprecated_downloads_ignored() {
        let f = Fixture::new(|path| {
            if path.starts_with("/pypi/") {
                assert_eq!(path,"/pypi/pypi/my-package/json");
                Reply::json(json!({"info":{"version":"2.0b1","downloads":-1,"home_page":"https://github.com/wrong/home",
                    "project_urls":{"Documentation":"https://github.com/wrong/docs","Source Code":"https://github.com/Owner/Repo"}},"downloads":{"last_month":-1}}))
            } else { github(path) }
        });
        let result = f.service().lookup("pypi:My.__Package", GithubMode::Demand, false).unwrap();
        assert_eq!(result.newest_version.as_deref(),Some("2.0b1"));
        assert_eq!(result.downloads.value,None);
        assert_eq!(result.downloads.reason.as_deref(),Some(PYPI_DOWNLOADS));
        assert_eq!(result.repository.unwrap().url,"https://github.com/Owner/Repo");
        assert_eq!(f.paths().len(),3);
    }

    #[test]
    fn cargo_uses_default_version_recent_90_days_and_does_not_relabel_total() {
        let f = Fixture::new(|path| {
            if path.ends_with("/missing") { Reply::json(json!({"crate":{"default_version":"1.0.0","downloads":9999,"repository":"https://gitlab.com/a/b"}})) }
            else { Reply::json(json!({"crate":{"default_version":"1.9.0","max_version":"2.0.0-beta","recent_downloads":0,"downloads":9999}})) }
        });
        let service = f.service();
        let result = service.lookup("cargo:serde", GithubMode::Demand, false).unwrap();
        assert_eq!(result.newest_version.as_deref(),Some("1.9.0"));
        assert_eq!(result.downloads.value,Some(0));
        assert!(result.downloads.period.unwrap().contains("90 days"));
        let missing = service.lookup("cargo:missing", GithubMode::Demand, false).unwrap();
        assert_eq!(missing.downloads.status,InsightStatus::Unavailable);
        assert_eq!(missing.downloads.value,None);
        assert!(missing.repository.is_none());
        assert_eq!(f.paths().len(),2);
    }

    #[test]
    fn maven_release_parent_properties_and_fixed_central_paths() {
        let f = Fixture::new(|path| match path {
            "/maven/maven2/org/example/lib/maven-metadata.xml" => Reply::xml("<metadata><versioning><release>2.1-RC1</release><latest>9-SNAPSHOT</latest></versioning></metadata>"),
            "/maven/maven2/org/example/lib/2.1-RC1/lib-2.1-RC1.pom" => Reply::xml(r#"<project><properties><parentVersion>4</parentVersion></properties><parent><groupId>${project.groupId}</groupId><artifactId>parent</artifactId><version>${parentVersion}</version></parent><scm><url>https://gitlab.com/other/repo</url></scm><repositories><repository><url>http://evil.invalid/</url></repository></repositories></project>"#),
            "/maven/maven2/org/example/parent/4/parent-4.pom" => Reply::xml("<project><properties><repoOwner>Owner</repoOwner></properties><scm><url>https://github.com/${repoOwner}/Repo.git</url></scm></project>"),
            p if p.starts_with("/github/") => github(p),
            _ => panic!("unexpected Maven path {path}"),
        });
        let result = f.service().lookup("maven:org.example:lib", GithubMode::Demand, false).unwrap();
        assert_eq!(result.newest_version.as_deref(),Some("2.1-RC1"));
        assert_eq!(result.downloads.reason.as_deref(),Some(MAVEN_DOWNLOADS));
        assert_eq!(result.repository.unwrap().url,"https://github.com/Owner/Repo");
        assert_eq!(f.paths().len(),5);
    }

    #[test]
    fn maven_cycle_unresolved_parent_and_latest_snapshot_are_bounded() {
        for case in ["cycle","unresolved","snapshot","limit"] {
            let scenario = case.to_string();
            let f = Fixture::new(move |path| {
                if path.ends_with("maven-metadata.xml") {
                    return if scenario == "snapshot" { Reply::xml("<metadata><versioning><latest>9-SNAPSHOT</latest></versioning></metadata>") }
                        else { Reply::xml("<metadata><versioning><latest>1</latest></versioning></metadata>") };
                }
                let parent = match scenario.as_str() {
                    "cycle" => "lib".into(),
                    "unresolved" => "${unknown}".into(),
                    _ => {
                        let artifact = path.split('/').nth(5).unwrap();
                        if artifact == "lib" { "p1".into() } else { format!("p{}",artifact.trim_start_matches('p').parse::<u8>().unwrap()+1) }
                    }
                };
                Reply::xml(&format!("<project><parent><groupId>org.example</groupId><artifactId>{parent}</artifactId><version>1</version></parent></project>"))
            });
            let result = f.service().lookup("maven:org.example:lib", GithubMode::Demand, false).unwrap();
            assert!(result.repository.is_none());
            match case {
                "snapshot" => { assert_eq!(result.newest_status,InsightStatus::Unavailable); assert_eq!(f.paths().len(),1); }
                "cycle" => { assert!(result.repository_reason.unwrap().contains("cycle")); assert_eq!(f.paths().len(),2); }
                "unresolved" => { assert!(result.repository_reason.unwrap().contains("resolved")); assert_eq!(f.paths().len(),2); }
                _ => { assert!(result.repository_reason.unwrap().contains("five")); assert_eq!(f.paths().len(),7); }
            }
        }
    }

    #[test]
    fn contributors_204_link_last_no_last_and_invalid_link() {
        for case in ["empty","last","next","bad","single","malformed"] {
            let scenario = case.to_string();
            let f = Fixture::new(move |path| {
                if path.contains("contributors?") {
                    match scenario.as_str() {
                        "empty" => Reply::body(204,vec![]),
                        "last" => Reply::json(json!([{"login":"a"}])).header("Link","<https://api.github.com/repos/Owner/Repo/contributors?per_page=1&page=2>; rel=\"next\", <https://api.github.com/repos/Owner/Repo/contributors?per_page=1&page=73>; rel=\"last\""),
                        "next" => Reply::json(json!([{}])).header("Link","<https://api.github.com/repos/Owner/Repo/contributors?page=2>; rel=\"next\""),
                        "bad" => Reply::json(json!([{}])).header("Link","<https://evil.invalid/collect?page=900>; rel=\"last\""),
                        "malformed" => Reply::json(json!({"error":"not an array"})),
                        _ => Reply::json(json!([{"login":"a"}])),
                    }
                } else { npm(path) }
            });
            let metric = f.service().lookup("npm:fixture", GithubMode::Demand, false).unwrap().repository.unwrap().contributors;
            match case {
                "empty" => assert_eq!(metric.value,Some(0)),
                "last" => assert_eq!(metric.value,Some(73)),
                "single" => assert_eq!(metric.value,Some(1)),
                "next" => { assert_eq!(metric.value,None); assert_eq!(metric.status,InsightStatus::Unavailable); }
                _ => { assert_eq!(metric.value,None); assert_eq!(metric.status,InsightStatus::Error); }
            }
            assert_eq!(f.paths().len(),4,"Link URLs must never be fetched");
        }
    }

    #[test]
    fn malformed_oversized_missing_counts_and_timeout_remain_errors_not_zero() {
        for case in ["malformed","oversized","streamed","missing","timeout","bodyTimeout"] {
            let scenario = case.to_string();
            let f = Fixture::new(move |path| {
                if path.starts_with("/npmDownloads/") {
                    match scenario.as_str() {
                        "malformed" => Reply::body(200,b"{broken".to_vec()),
                        "oversized" => Reply::body(200,vec![b'x';MAX_BODY+1]),
                        "streamed" => Reply::body(200,vec![b'x';MAX_BODY+1]).close_delimited(),
                        "bodyTimeout" => Reply::json(json!({"downloads":0,"start":"2026-09-01","end":"2026-09-30"})).slow_body(),
                        "missing" => Reply::json(json!({"start":"2026-09-01","end":"2026-09-30"})),
                        _ => Reply::json(json!({"downloads":0,"start":"2026-09-01","end":"2026-09-30"})).delayed(Duration::from_millis(200)),
                    }
                } else if path.starts_with("/npmRegistry/") { Reply::json(json!({"version":"1.2.3","repository":"https://gitlab.com/a/b"})) }
                else { panic!("unexpected provider {path}") }
            });
            let result = f.service_with(None,Duration::from_millis(70),Duration::from_secs(2)).lookup("npm:fixture", GithubMode::Demand, false).unwrap();
            assert_eq!(result.newest_status,InsightStatus::Available);
            assert_eq!(result.downloads.status,InsightStatus::Error,"{case}");
            assert_eq!(result.downloads.value,None);
            assert!(result.repository.is_none());
        }
    }

    #[test]
    fn malformed_registry_body_is_not_an_empty_success() {
        for body in [b"not json".to_vec(),br#"{"unexpected":true}"#.to_vec()] {
            let f = Fixture::new(move |_| Reply::body(200,body.clone()));
            let result = f.service().lookup("pypi:fixture", GithubMode::Demand, false).unwrap();
            assert_eq!(result.newest_status,InsightStatus::Error);
            assert!(result.newest_version.is_none());
            assert!(!result.notes.is_empty());
        }
    }

    #[test]
    fn credentials_only_reach_github_and_typed_disk_has_no_raw_response() {
        let secret = "isolated-test-token-not-a-real-credential";
        let f = Fixture::new(|path| {
            if path.starts_with("/github/") && !path.contains("contributors?") {
                Reply::json(json!({"full_name":"Owner/Repo","html_url":"https://github.com/Owner/Repo","owner":{"type":"User","email":"private@example.invalid"},"stargazers_count":2,"rawSecret":"never-persist-raw"}))
            } else { npm(path) }
        });
        let result = f.service_with(Some(secret.into()),Duration::from_secs(2),Duration::from_secs(5)).lookup("npm:fixture", GithubMode::Demand, false).unwrap();
        assert_eq!(result.repository.as_ref().unwrap().owner_type,OwnerType::User);
        for request in f.requests.lock().iter() {
            let github = request.starts_with("GET /github/");
            assert_eq!(request.to_ascii_lowercase().contains("authorization: bearer"),github);
            assert_eq!(request.contains(secret),github);
            if github { assert!(request.contains("2026-03-10")); }
            assert!(request.contains(USER_AGENT));
        }
        assert!(!serde_json::to_string(&result).unwrap().contains(secret));
        for bucket in ["packages","repositories"] {
            for entry in fs::read_dir(f.root.join("package-insights").join(bucket)).unwrap() {
                let text = fs::read_to_string(entry.unwrap().path()).unwrap();
                for forbidden in [secret,"private@example.invalid","never-persist-raw","authorization"] { assert!(!text.contains(forbidden)); }
            }
        }
    }

    #[test]
    fn redirects_allow_three_same_provider_but_block_foreign_credentials_and_loops() {
        let trap = TcpListener::bind("127.0.0.1:0").unwrap();
        trap.set_nonblocking(true).unwrap();
        let trap_url = format!("http://{}/capture",trap.local_addr().unwrap());
        let f = Fixture::new(move |path| {
            if path == "/pypi/pypi/foreign/json" { Reply::body(302,vec![]).header("Location",&trap_url) }
            else if path == "/pypi/pypi/credentials/json" { Reply::body(302,vec![]).header("Location","http://user:pass@127.0.0.1/capture") }
            else if path == "/pypi/pypi/loop/json" { Reply::body(302,vec![]).header("Location","/pypi/pypi/loop/json") }
            else if path == "/pypi/pypi/chain/json" { Reply::body(302,vec![]).header("Location","/hop1") }
            else if path == "/hop1" { Reply::body(302,vec![]).header("Location","/hop2") }
            else if path == "/hop2" { Reply::body(302,vec![]).header("Location","/done") }
            else { Reply::json(json!({"info":{"version":"1.0"}})) }
        });
        let service = f.service_with(Some("test-secret".into()),Duration::from_secs(2),Duration::from_secs(5));
        assert_eq!(service.lookup("pypi:chain", GithubMode::Skip, false).unwrap().newest_status,InsightStatus::Available);
        for id in ["pypi:foreign","pypi:credentials","pypi:loop"] {
            let result = service.lookup(id, GithubMode::Demand, false).unwrap();
            assert_eq!(result.newest_status,InsightStatus::Error);
            assert!(result.notes[0].contains("redirect"));
        }
        assert_eq!(f.paths().iter().filter(|p| *p == "/pypi/pypi/loop/json").count(),4);
        assert!(matches!(trap.accept(),Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
    }

    #[test]
    fn shared_repository_deduplication_gate_and_disk_repository_cache() {
        let f = Fixture::new(npm);
        let service = Arc::new(f.service());
        thread::scope(|scope| {
            for i in 0..5 {
                let service = service.clone();
                scope.spawn(move || {
                    assert_eq!(service.lookup(&format!("npm:package-{i}"), GithubMode::Demand, false).unwrap().newest_status,InsightStatus::Available);
                });
            }
        });
        assert_eq!(f.paths().len(),12,"five registry lookups share one serialized GitHub lookup");
        drop(service);
        f.advance(400 * DAY);
        let service = f.service();
        let result = service.lookup("npm:package-0", GithubMode::Skip, false).unwrap();
        assert_eq!(result.repository.unwrap().stars.value,Some(0));
        assert_eq!(f.paths().len(),12,"good package and repository entries never expire");
        service.lookup("npm:third", GithubMode::Demand, false).unwrap();
        assert_eq!(f.paths().len(),14,"repository disk result is shared across packages");
    }

    #[test]
    fn cache_ttls_corruption_and_atomic_replacement() {
        let f = Fixture::new(|path| {
            if path.contains("/missing/") { Reply::body(404,vec![]) }
            else if path.contains("/broken/") { Reply::body(500,vec![]) }
            else { Reply::json(json!({"info":{"version":"1.0"}})) }
        });
        let service = f.service();
        for id in ["pypi:success","pypi:missing","pypi:broken"] { service.lookup(id, GithubMode::Skip, false).unwrap(); }
        assert_eq!(f.paths().len(),3);
        f.advance(TRANSIENT-1);
        for id in ["pypi:success","pypi:missing","pypi:broken"] { service.lookup(id, GithubMode::Skip, false).unwrap(); }
        assert_eq!(f.paths().len(),3);
        f.advance(1);
        service.lookup("pypi:broken", GithubMode::Skip, false).unwrap();
        assert_eq!(f.paths().len(),4);
        f.advance(HOUR-TRANSIENT);
        service.lookup("pypi:missing", GithubMode::Skip, false).unwrap();
        service.lookup("pypi:success", GithubMode::Skip, false).unwrap();
        assert_eq!(f.paths().len(),5);
        f.advance(DAY-HOUR);
        service.lookup("pypi:success", GithubMode::Skip, true).unwrap();
        assert_eq!(f.paths().len(),6);
        let path = service.entry_path("packages","pypi:success");
        let entry: Entry<PackageInsights> = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(entry.value.checked_at,f.now.load(Ordering::SeqCst),"atomic replacement persisted refreshed entry");
        drop(service);
        fs::write(path,b"corrupt").unwrap();
        assert_eq!(f.service().lookup("pypi:success", GithubMode::Skip, false).unwrap().newest_status,InsightStatus::Available);
        assert_eq!(f.paths().len(),7);
    }

    #[test]
    fn disk_write_failure_does_not_discard_fetch() {
        let f = Fixture::new(npm);
        fs::create_dir_all(&f.root).unwrap();
        fs::write(f.root.join("package-insights"),b"not a directory").unwrap();
        let service = f.service();
        assert_eq!(service.lookup("npm:fixture", GithubMode::Demand, false).unwrap().newest_status,InsightStatus::Available);
        service.lookup("npm:fixture", GithubMode::Demand, false).unwrap();
        assert_eq!(f.paths().len(),4);
    }

    #[test]
    fn github_rate_limit_cooldown_shared_across_packages_and_retried_after_expiry() {
        let f = Fixture::new(|path| {
            if path.starts_with("/github/") { Reply::body(403,br#"{"message":"API rate limit exceeded"}"#.to_vec())
                .header("X-RateLimit-Remaining","0").header("Retry-After","120") }
            else { npm(path) }
        });
        let service = f.service();
        let first = service.lookup("npm:first", GithubMode::Demand, false).unwrap();
        let repo = first.repository.unwrap();
        assert_eq!(repo.stars.status,InsightStatus::RateLimited);
        assert_eq!(repo.contributors.status,InsightStatus::RateLimited);
        assert_eq!(repo.stars.retry_at,Some(f.now.load(Ordering::SeqCst)+120_000));
        assert_eq!(f.paths().len(),3,"cooldown prevents contributors network request");
        service.lookup("npm:second", GithubMode::Demand, false).unwrap();
        assert_eq!(f.paths().len(),5);
        f.advance(120_000);
        service.lookup("npm:third", GithubMode::Demand, false).unwrap();
        assert_eq!(f.paths().len(),8);
    }

    #[test]
    fn provider_cooldown_is_separate_from_other_providers_and_ordinary_403_is_error() {
        let f = Fixture::new(|path| {
            if path.starts_with("/npmDownloads/") { Reply::body(429,vec![]) }
            else if path.starts_with("/cargo/") { Reply::body(403,br#"{"message":"Access denied"}"#.to_vec()).header("X-RateLimit-Reset","1900000000") }
            else { npm(path) }
        });
        let service = f.service();
        let first = service.lookup("npm:first", GithubMode::Demand, false).unwrap();
        assert_eq!(first.downloads.status,InsightStatus::RateLimited);
        assert_eq!(first.downloads.retry_at,Some(f.now.load(Ordering::SeqCst)+60_000));
        assert_eq!(first.repository.unwrap().stars.status,InsightStatus::Available);
        let second = service.lookup("npm:second", GithubMode::Demand, false).unwrap();
        assert_eq!(second.downloads.status,InsightStatus::RateLimited);
        assert_eq!(f.paths().iter().filter(|p| p.starts_with("/npmDownloads/")).count(),1);
        assert_eq!(service.lookup("cargo:serde", GithubMode::Skip, false).unwrap().newest_status,InsightStatus::Error);
        assert_eq!(service.lookup("cargo:other", GithubMode::Skip, false).unwrap().newest_status,InsightStatus::Error);
        assert_eq!(f.paths().iter().filter(|p| p.starts_with("/cargo/")).count(),2);
    }

    #[test]
    fn retry_after_http_date_reset_and_fallback_are_deterministic() {
        let mut headers = header::HeaderMap::new();
        headers.insert(header::RETRY_AFTER,"Thu, 01 Jan 1970 00:02:00 GMT".parse().unwrap());
        assert_eq!(retry_at(&headers,0),Some(120_000));
        headers.insert("x-ratelimit-reset","180".parse().unwrap());
        assert_eq!(retry_at(&headers,0),Some(120_000),"reset is ignored when Retry-After exists");
        headers.insert("x-ratelimit-remaining","0".parse().unwrap());
        assert_eq!(retry_at(&headers,0),Some(120_000),"Retry-After outranks an exhausted primary reset");
        headers.remove(header::RETRY_AFTER);
        assert_eq!(retry_at(&headers,0),Some(180_000));
        headers.insert("x-ratelimit-remaining","5".parse().unwrap());
        assert_eq!(retry_at(&headers,0),None,"reset is ignored unless the primary limit is exhausted");
        headers.insert(header::RETRY_AFTER,"30".parse().unwrap());
        assert_eq!(retry_at(&headers,200_000),Some(230_000));
        assert_eq!(http_date_ms("Thu, 31 Feb 2026 00:00:00 GMT"),None);
        let f = Fixture::new(|path| if path.starts_with("/github/") { Reply::body(429,vec![]) } else { npm(path) });
        let result = f.service().lookup("npm:fixture", GithubMode::Demand, false).unwrap();
        assert_eq!(result.repository.unwrap().stars.retry_at,Some(f.now.load(Ordering::SeqCst)+HOUR));
    }

    #[test]
    fn overall_deadline_caps_sequential_requests_and_gate_wait() {
        let f = Fixture::new(|path| npm(path).delayed(Duration::from_millis(100)));
        let service = f.service_with(None,Duration::from_millis(400),Duration::from_millis(160));
        let result = service.lookup("npm:fixture", GithubMode::Demand, false).unwrap();
        assert_eq!(result.newest_status,InsightStatus::Available);
        assert_eq!(result.downloads.status,InsightStatus::Error);
        assert_eq!(f.paths().len(),2,"deadline prevents later GitHub requests");
        let service = Arc::new(f.service_with(None,Duration::from_millis(100),Duration::from_millis(40)));
        let permits: Vec<_> = (0..4).map(|_| service.registry_permit(Instant::now() + Duration::from_secs(1)).unwrap()).collect();
        let other = service.clone();
        let thread = thread::spawn(move || other.lookup("pypi:waiting", GithubMode::Demand, false).unwrap());
        let result = thread.join().unwrap();
        drop(permits);
        assert_eq!(result.newest_status,InsightStatus::Error);
        assert!(result.repository_reason.unwrap().contains("waiting"));
        assert_eq!(f.paths().len(),2);
    }

    #[test]
    fn authenticated_github_cross_host_redirect_never_reaches_target() {
        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        target.set_nonblocking(true).unwrap();
        let url = format!("http://{}/stolen",target.local_addr().unwrap());
        let f = Fixture::new(move |path| {
            if path.starts_with("/github/") { Reply::body(302,vec![]).header("Location",&url) }
            else { npm(path) }
        });
        let result = f.service_with(Some("test-token".into()),Duration::from_secs(1),Duration::from_secs(5)).lookup("npm:fixture", GithubMode::Demand, false).unwrap();
        let repo = result.repository.unwrap();
        assert_eq!(repo.stars.status,InsightStatus::Error);
        assert_eq!(repo.contributors.status,InsightStatus::Error);
        assert!(matches!(target.accept(),Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
    }

    #[test]
    fn cache_mutex_is_available_during_remote_io() {
        let holder: Arc<Mutex<Option<Arc<PackageInsightsService>>>> = Arc::new(Mutex::new(None));
        let worker_holder = holder.clone();
        let f = Fixture::new(move |path| {
            let service = worker_holder.lock().as_ref().unwrap().clone();
            assert!(service.cache.try_lock().is_some(),"cache mutex held across HTTP I/O");
            npm(path)
        });
        let service = Arc::new(f.service());
        *holder.lock() = Some(service.clone());
        service.lookup("npm:fixture", GithubMode::Demand, false).unwrap();
        holder.lock().take();
    }

    #[test]
    fn pypi_repository_fallbacks_never_search_description_prose() {
        assert_eq!(pypi_repository(&json!({"project_urls":{"Source":"https://gitlab.com/a/b",
            "Other":"https://github.com/Owner/Repo"},"home_page":"https://github.com/Other/Home"})).unwrap().key(),"owner/repo");
        assert_eq!(pypi_repository(&json!({"project_urls":{"Docs":"https://docs.example.invalid/"},
            "home_page":"https://github.com/Owner/Repo"})).unwrap().key(),"owner/repo");
        assert!(pypi_repository(&json!({"description":"https://github.com/Owner/Repo",
            "home_page":"https://example.invalid/"})).is_none());
    }

    #[test]
    fn pypi_scans_past_reserved_github_pages_and_reserved_owners_are_rejected() {
        for input in ["https://github.com/sponsors/Owner","https://github.com/orgs/Owner/repo","https://github.com/apps/tool",
            "https://github.com/marketplace/tool","https://github.com/features/actions","https://github.com/topics/rust",
            "https://github.com/settings/profile","https://github.com/login/oauth","https://github.com/about/team",
            "https://github.com/pricing/plans","github:Sponsors/Owner","git@github.com:About/repo.git"] {
            assert!(normalize_repository(input).is_none(),"{input}");
        }
        // serde_json maps are key-ordered, so "Funding" is examined before "Homepage".
        let info = json!({"project_urls":{"Funding":"https://github.com/sponsors/Owner","Homepage":"https://github.com/Owner/Repo"}});
        assert_eq!(pypi_repository(&info).unwrap().key(),"owner/repo");
        let preferred = json!({"project_urls":{"Source":"https://github.com/orgs/Owner/people","Funding":"https://github.com/sponsors/Owner",
            "Zeta":"https://github.com/Owner/Repo"}});
        assert_eq!(pypi_repository(&preferred).unwrap().key(),"owner/repo");
    }

    #[test]
    fn maven_non_utf8_pom_text_still_yields_scm_repository() {
        let f = Fixture::new(|path| match path {
            "/maven/maven2/org/example/lib/maven-metadata.xml" => Reply::xml("<metadata><versioning><release>1.0</release></versioning></metadata>"),
            "/maven/maven2/org/example/lib/1.0/lib-1.0.pom" => Reply::body(200,
                b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?><project><name>Caf\xe9</name><scm><url>https://github.com/Owner/Repo</url></scm></project>".to_vec()),
            p if p.starts_with("/github/") => github(p),
            _ => panic!("unexpected Maven path {path}"),
        });
        let result = f.service().lookup("maven:org.example:lib", GithubMode::Demand, false).unwrap();
        assert_eq!(result.newest_version.as_deref(),Some("1.0"));
        assert_eq!(result.repository.unwrap().url,"https://github.com/Owner/Repo");
    }

    #[test]
    fn skip_defers_github_and_demand_is_saved_for_later_skip() {
        let f = Fixture::new(npm);
        let service = f.service();
        let registry = service.lookup("npm:fixture", GithubMode::Skip, false).unwrap();
        assert!(registry.github_pending);
        assert_eq!(registry.repository_url.as_deref(),Some("https://github.com/Owner/Repo"));
        assert!(registry.repository.is_none());
        assert_eq!(f.paths().len(),2);
        let demand = service.lookup("npm:fixture", GithubMode::Demand, false).unwrap();
        assert!(!demand.github_pending);
        assert_eq!(demand.repository.unwrap().contributors.value,Some(1));
        assert_eq!(f.paths().len(),4);
        let later = service.lookup("npm:fixture", GithubMode::Skip, false).unwrap();
        assert!(!later.github_pending);
        assert!(later.repository.is_some());
        assert_eq!(f.paths().len(),4);
        let stored: Entry<PackageInsights> = service.load("packages","npm:fixture").unwrap();
        assert!(stored.value.repository.is_none());
        assert!(!stored.value.github_pending);
    }

    #[test]
    fn failed_refresh_preserves_good_package_and_repository_bytes() {
        let fail = Arc::new(AtomicBool::new(false));
        let provider_fail = fail.clone();
        let f = Fixture::new(move |path| if provider_fail.load(Ordering::SeqCst) { Reply::body(500,vec![]) } else { npm(path) });
        let service = f.service();
        let original = service.lookup("npm:fixture", GithubMode::Demand, false).unwrap();
        let package_path = service.entry_path("packages","npm:fixture");
        let repo_path = service.entry_path("repositories","owner/repo");
        let package_bytes = fs::read(&package_path).unwrap();
        let repo_bytes = fs::read(&repo_path).unwrap();
        fail.store(true,Ordering::SeqCst);
        f.advance(DAY);
        let refreshed = service.lookup("npm:fixture", GithubMode::Demand, true).unwrap();
        assert_eq!(refreshed.newest_version,original.newest_version);
        assert_eq!(refreshed.checked_at,original.checked_at);
        assert_eq!(refreshed.repository.unwrap().checked_at,original.repository.unwrap().checked_at);
        assert_eq!(refreshed.notes.iter().filter(|note| note.starts_with("Refresh failed (")).count(),2);
        assert_eq!(fs::read(package_path).unwrap(),package_bytes);
        assert_eq!(fs::read(repo_path).unwrap(),repo_bytes);
        assert!(service.lookup("npm:fixture", GithubMode::Skip, false).unwrap().notes.is_empty());
    }

    #[test]
    fn github_cooldown_and_budget_survive_service_restart() {
        let f = Fixture::new(|path| if path.starts_with("/github/") {
            Reply::body(429,vec![]).header("Retry-After","120").header("X-RateLimit-Limit","60")
                .header("X-RateLimit-Remaining","0").header("X-RateLimit-Reset","1800000120")
        } else { npm(path) });
        let service = f.service();
        service.lookup("npm:first", GithubMode::Demand, false).unwrap();
        drop(service);
        let service = f.service();
        let status = service.status();
        assert!(!status.github_authenticated);
        assert_eq!(status.github_limit,Some(60));
        assert_eq!(status.github_remaining,Some(0));
        assert_eq!(status.github_reset_at,Some(1_800_000_120_000));
        assert_eq!(status.github_cooldown_until,status.github_reset_at);
        let result = service.lookup("npm:second", GithubMode::Demand, false).unwrap();
        assert_eq!(result.repository.unwrap().stars.retry_at,status.github_reset_at);
        assert_eq!(f.paths().iter().filter(|p| p.starts_with("/github/")).count(),1);
        f.advance(120_000);
        service.lookup("npm:second", GithubMode::Demand, false).unwrap();
        assert_eq!(f.paths().iter().filter(|p| p.starts_with("/github/")).count(),2);
    }

    #[test]
    fn github_reserves_differ_by_mode_and_persist_from_success_headers() {
        for remaining in [7,6,3] {
            let f = Fixture::new(move |path| if path.starts_with("/github/") {
                github(path).header("X-RateLimit-Limit","60").header("X-RateLimit-Remaining",&remaining.to_string())
                    .header("X-RateLimit-Reset","1800003600")
            } else if path.starts_with("/npmRegistry/") {
                let name = path.split('/').nth(2).unwrap();
                Reply::json(json!({"version":"1","repository":format!("https://github.com/Owner/{name}")}))
            } else { npm(path) });
            let service = f.service();
            service.lookup("npm:seed", GithubMode::Demand, false).unwrap();
            drop(service);
            let service = f.service();
            let bulk = service.lookup("npm:bulk", GithubMode::Bulk, true).unwrap();
            let metric = bulk.repository.unwrap().stars;
            assert_eq!(metric.status,InsightStatus::RateLimited);
            assert_eq!(metric.retry_at,Some(1_800_003_600_000));
            assert_eq!(metric.reason.as_deref(),Some("GitHub API budget reserved; resumes after the hourly reset."));
            assert_eq!(f.paths().iter().filter(|p| p.starts_with("/github/")).count(),2);
            service.lookup("npm:demand", GithubMode::Demand, true).unwrap();
            let count = f.paths().iter().filter(|p| p.starts_with("/github/")).count();
            assert_eq!(count,if remaining >= 7 { 4 } else { 2 });
            assert_eq!(service.status().github_remaining,Some(remaining));
        }
    }

    #[test]
    fn imports_only_successful_legacy_repositories_and_ignores_legacy_packages() {
        let f = Fixture::new(npm);
        let service = f.service();
        let result = service.lookup("npm:fixture", GithubMode::Demand, false).unwrap();
        let now = f.now.load(Ordering::SeqCst);
        let path = service.entry_path("repositories","owner/repo");
        let mut value = serde_json::to_value(result.repository.as_ref().unwrap()).unwrap();
        value.as_object_mut().unwrap().remove("checkedAt");
        fs::write(&path,serde_json::to_vec(&json!({"schema":1,"key":"owner/repo","expiresAt":now+DAY,"value":value})).unwrap()).unwrap();
        let package_path = service.entry_path("packages","npm:fixture");
        let mut package = serde_json::to_value(result.clone()).unwrap();
        package.as_object_mut().unwrap().remove("description");
        fs::write(package_path,serde_json::to_vec(&json!({"schema":1,"key":"npm:fixture","expiresAt":now+DAY,"value":package})).unwrap()).unwrap();
        drop(service);
        f.advance(400 * DAY);
        let service = f.service();
        let imported = service.cached_repository("owner/repo").unwrap();
        assert_eq!(imported.saved_at,now);
        assert_eq!(imported.value.checked_at,now);
        assert!(service.cached_package("npm:fixture").is_none());
        assert_eq!(f.paths().len(),4);
        assert_eq!(read_json::<Value>(&path).unwrap()["schema"],2);
        let mut bad = value;
        bad["stars"] = serde_json::to_value(Failure::error("Fixture failure.").metric("GitHub")).unwrap();
        fs::write(&path,serde_json::to_vec(&json!({"schema":1,"key":"owner/repo","expiresAt":now+DAY,"value":bad})).unwrap()).unwrap();
        assert!(f.service().cached_repository("owner/repo").is_none());
        fs::write(&path,b"{corrupt").unwrap();
        assert!(f.service().cached_repository("owner/repo").is_none());
    }

    #[test]
    fn descriptions_are_sanitized_from_each_registry_without_extra_requests() {
        assert_eq!(one_line(Some(" \n hello\t world\u{0000}!\r\n ")).as_deref(),Some("hello world!"));
        assert!(one_line(Some(" \t\u{0000}\n")).is_none());
        assert!(one_line(None).is_none());
        let raw = format!("  Useful\n\tpackage\u{0000} {}","界".repeat(170));
        let expected = format!("Useful package {}…","界".repeat(145));
        assert_eq!(one_line(Some(&raw)).as_deref(),Some(expected.as_str()));
        let f = Fixture::new(move |path| {
            if path.starts_with("/npmRegistry/") { Reply::json(json!({"version":"1","description":raw})) }
            else if path.starts_with("/npmDownloads/") { npm(path) }
            else if path.starts_with("/pypi/") { Reply::json(json!({"info":{"version":"1","summary":raw}})) }
            else if path.starts_with("/cargo/") { Reply::json(json!({"crate":{"default_version":"1","description":raw}})) }
            else if path.ends_with("maven-metadata.xml") { Reply::xml("<metadata><versioning><release>1</release></versioning></metadata>") }
            else { Reply::xml(&format!("<project><description>{}</description></project>",raw.replace('\0',""))) }
        });
        let service = f.service();
        for id in ["npm:fixture","pypi:fixture","cargo:fixture","maven:org.example:fixture"] {
            let result = service.lookup(id, GithubMode::Skip, false).unwrap();
            assert_eq!(result.description.as_deref(),Some(expected.as_str()),"{id}");
            assert_eq!(result.description.unwrap().chars().count(),161);
        }
        assert_eq!(f.paths().len(),6);
    }

    #[test]
    fn registry_lookups_have_four_permits_and_github_is_serialized() {
        use std::sync::atomic::AtomicUsize;
        let registry_live = Arc::new(AtomicUsize::new(0));
        let registry_max = Arc::new(AtomicUsize::new(0));
        let github_live = Arc::new(AtomicUsize::new(0));
        let github_max = Arc::new(AtomicUsize::new(0));
        let (rl,rm,gl,gm) = (registry_live.clone(),registry_max.clone(),github_live.clone(),github_max.clone());
        let f = Fixture::new(move |path| {
            let (live,max) = if path.starts_with("/github/") { (&gl,&gm) } else { (&rl,&rm) };
            let current = live.fetch_add(1,Ordering::SeqCst)+1;
            max.fetch_max(current,Ordering::SeqCst);
            thread::sleep(Duration::from_millis(30));
            let reply = if path.starts_with("/npmRegistry/") {
                let name = path.split('/').nth(2).unwrap();
                Reply::json(json!({"version":"1","repository":format!("https://github.com/Owner/{name}")}))
            } else { npm(path) };
            live.fetch_sub(1,Ordering::SeqCst);
            reply
        });
        let service = Arc::new(f.service_with(None,Duration::from_secs(2),Duration::from_secs(10)));
        let barrier = Arc::new(std::sync::Barrier::new(8));
        thread::scope(|scope| for i in 0..8 {
            let (service,barrier) = (service.clone(),barrier.clone());
            scope.spawn(move || {
                barrier.wait();
                assert_eq!(service.lookup(&format!("npm:fixture-{i}"), GithubMode::Demand, false).unwrap().newest_status,InsightStatus::Available);
            });
        });
        assert!((2..=4).contains(&registry_max.load(Ordering::SeqCst)));
        assert_eq!(github_max.load(Ordering::SeqCst),1);
        assert_eq!(f.paths().iter().filter(|p| p.starts_with("/github/")).count(),16);
    }

    fn github_count(f: &Fixture) -> usize { f.paths().iter().filter(|p| p.starts_with("/github/")).count() }
    fn github_requests(f: &Fixture) -> Vec<String> {
        f.requests.lock().iter().filter(|r| r.starts_with("GET /github/")).cloned().collect()
    }
    fn carries_authorization(request: &str) -> bool { request.to_ascii_lowercase().contains("authorization:") }

    #[test]
    fn credential_precedence_is_github_token_then_gh_token_then_cli() {
        let both = |name: &str| match name {
            "GITHUB_TOKEN" => Some("github-token".to_string()), "GH_TOKEN" => Some("gh-env-token".to_string()), _ => None };
        let credential = resolve_credential(both, || panic!("must not run gh"));
        assert_eq!((credential.token.as_deref(), credential.source, credential.problem), (Some("github-token"), Some("GITHUB_TOKEN"), None));
        let gh_only = |name: &str| (name == "GH_TOKEN").then(|| "gh-env-token".to_string());
        let credential = resolve_credential(gh_only, || panic!("must not run gh"));
        assert_eq!((credential.token.as_deref(), credential.source), (Some("gh-env-token"), Some("GH_TOKEN")));
        let blank = |name: &str| match name { "GITHUB_TOKEN" => Some("  ".to_string()), "GH_TOKEN" => Some(String::new()), _ => None };
        let credential = resolve_credential(blank, || GhOutcome::Token(" cli.token_123- \r\n".into()));
        assert_eq!((credential.token.as_deref(), credential.source, credential.problem), (Some("cli.token_123-"), Some("gh"), None));
    }

    #[test]
    fn unusable_values_are_ignored_and_problems_are_exact() {
        for bad in ["a b","a\nb","a\tb","é","token:secret"] {
            let env = move |name: &str| (name == "GITHUB_TOKEN" || name == "GH_TOKEN").then(|| bad.to_string());
            let credential = resolve_credential(env, || GhOutcome::NotInstalled);
            assert!(credential.token.is_none());
            assert_eq!(credential.problem.as_deref(), Some("GITHUB_TOKEN has an unsupported format and was ignored."));
        }
        let only_gh_token = |name: &str| (name == "GH_TOKEN").then(|| "a b".to_string());
        assert_eq!(resolve_credential(only_gh_token, || GhOutcome::NotLoggedIn("gh".into())).problem.as_deref(),
            Some("GH_TOKEN has an unsupported format and was ignored."));
        // An ignored variable is not a problem when the CLI supplies a token.
        let credential = resolve_credential(only_gh_token, || GhOutcome::Token("cli-token".into()));
        assert_eq!((credential.token.as_deref(), credential.source, credential.problem), (Some("cli-token"), Some("gh"), None));
        let none = |_: &str| None;
        assert_eq!(resolve_credential(none, || GhOutcome::NotInstalled).problem.as_deref(),
            Some("GitHub CLI (gh) was not found. Install it (https://cli.github.com), run `gh auth login`, then press Recheck."));
        let not_logged_in = resolve_credential(none, || GhOutcome::NotLoggedIn("gh".into())).problem.unwrap();
        assert!(not_logged_in.starts_with("GitHub CLI (gh) is installed but not logged in to github.com. Run `gh auth login`"), "{not_logged_in}");
        assert!(not_logged_in.ends_with("then press Recheck."), "{not_logged_in}");
        let full = resolve_credential(none, || GhOutcome::NotLoggedIn(r"C:\Tools\gh.exe".into())).problem.unwrap();
        assert!(full.starts_with(r"GitHub CLI (C:\Tools\gh.exe) is installed"), "{full}");
        assert_eq!(full.contains("WSL"), cfg!(windows));
        assert_eq!(resolve_credential(none, || GhOutcome::Unusable("GitHub CLI could not be started.")).problem.as_deref(),
            Some("GitHub CLI could not be started."));
        assert_eq!(resolve_credential(none, || GhOutcome::Token("a".repeat(256))).problem.as_deref(),
            Some("GitHub CLI returned an unusable token."));
        let credential = Credential { token: Some("secret-token-value".into()), source: Some("gh"), problem: None };
        assert!(!format!("{credential:?}").contains("secret-token-value"));
        assert!(!format!("{:?}", GhOutcome::Token("secret-token-value".into())).contains("secret-token-value"));
    }

    #[test]
    fn gh_candidates_start_with_path_then_known_install_locations() {
        let env = |name: &str| match name {
            "LOCALAPPDATA" => Some(r"C:\Users\fixture\AppData\Local".to_string()),
            "ProgramFiles" => Some(r"C:\Program Files".to_string()), _ => None };
        let programs = gh_programs(&env);
        #[cfg(windows)]
        assert_eq!(programs, vec![
            OsString::from("gh"),
            PathBuf::from(r"C:\Users\fixture\AppData\Local").join(r"Programs\GitHub CLI\bin\gh.exe").into_os_string(),
            PathBuf::from(r"C:\Program Files").join(r"GitHub CLI\gh.exe").into_os_string(),
            PathBuf::from(r"C:\Users\fixture\AppData\Local").join(r"Microsoft\WinGet\Links\gh.exe").into_os_string(),
        ]);
        #[cfg(not(windows))]
        assert_eq!(programs, ["gh","/usr/local/bin/gh","/opt/homebrew/bin/gh","/usr/bin/gh"].map(OsString::from).to_vec());
    }

    #[test]
    fn reg_query_output_is_parsed_by_value_name_and_type() {
        let output = "\r\nHKEY_CURRENT_USER\\Environment\r\n    OTHER    REG_SZ    nope\r\n    GITHUB_TOKEN_EXTRA    REG_SZ    wrong\r\n    github_token    REG_SZ    ghp_abc123\r\n\r\n";
        assert_eq!(parse_reg_value(output,"GITHUB_TOKEN").as_deref(),Some("ghp_abc123"));
        assert_eq!(parse_reg_value("    GH_TOKEN    REG_EXPAND_SZ    tok-en_1\r\n","GH_TOKEN").as_deref(),Some("tok-en_1"));
        assert_eq!(parse_reg_value("    GH_TOKEN    REG_SZ    \r\n","GH_TOKEN"),None);
        assert_eq!(parse_reg_value("    GH_TOKEN    REG_DWORD    0x1\r\n","GH_TOKEN"),None);
        assert_eq!(parse_reg_value("ERROR: The system was unable to find the specified registry key or value.\r\n","GH_TOKEN"),None);
        assert_eq!(parse_reg_value("","GH_TOKEN"),None);
    }

    #[test]
    fn missing_credential_is_reprobed_after_thirty_seconds_or_on_demand_but_a_token_is_kept() {
        use std::sync::atomic::AtomicUsize;
        let f = Fixture::new(npm);
        let calls = Arc::new(AtomicUsize::new(0));
        let has_token = Arc::new(AtomicBool::new(false));
        let (counter, token_ready) = (calls.clone(), has_token.clone());
        let service = f.service().with_credentials(|_| None, move || {
            counter.fetch_add(1,Ordering::SeqCst);
            if token_ready.load(Ordering::SeqCst) { GhOutcome::Token("gh-token".into()) } else { GhOutcome::NotLoggedIn("gh".into()) }
        });
        let status = service.status();
        assert!(!status.github_authenticated);
        assert_eq!(status.github_problem.as_deref(),Some(gh_not_logged_in("gh").as_str()));
        service.status();
        assert_eq!(calls.load(Ordering::SeqCst),1);
        f.advance(CREDENTIAL_RETRY-1);
        service.status();
        assert_eq!(calls.load(Ordering::SeqCst),1,"a missing credential is not re-resolved within 30 s");
        f.advance(1);
        service.status();
        assert_eq!(calls.load(Ordering::SeqCst),2);
        service.credential(true);
        assert_eq!(calls.load(Ordering::SeqCst),3,"force always resolves");
        has_token.store(true,Ordering::SeqCst);
        let status = service.recheck();
        assert_eq!(calls.load(Ordering::SeqCst),4);
        assert!(status.github_authenticated);
        assert_eq!(status.github_source.as_deref(),Some("gh"));
        assert!(status.github_problem.is_none());
        f.advance(DAY);
        assert!(service.status().github_authenticated);
        assert_eq!(calls.load(Ordering::SeqCst),4,"a token is kept until GitHub rejects it");
        assert_eq!(f.paths().len(),0,"credential probing makes no provider request");
    }

    fn budget_fixture(mode: Arc<std::sync::atomic::AtomicU8>) -> Fixture {
        Fixture::new(move |path| {
            if path.starts_with("/npmRegistry/") {
                let name = path.split('/').nth(2).unwrap();
                Reply::json(json!({"version":"1","repository":format!("https://github.com/Owner/{name}")}))
            } else if path.starts_with("/github/") {
                match mode.load(Ordering::SeqCst) {
                    0 => github(path).header("X-RateLimit-Limit","60").header("X-RateLimit-Remaining","48").header("X-RateLimit-Reset","1800003600"),
                    1 => Reply::body(429,vec![]).header("Retry-After","3600").header("X-RateLimit-Limit","60")
                        .header("X-RateLimit-Remaining","48").header("X-RateLimit-Reset","1800003600"),
                    _ => github(path).header("X-RateLimit-Limit","5000").header("X-RateLimit-Remaining","4999").header("X-RateLimit-Reset","1800003600"),
                }
            } else { npm(path) }
        })
    }

    #[test]
    fn switching_to_a_token_resets_the_anonymous_budget_and_refetches_only_rate_limited_repositories() {
        let mode = Arc::new(std::sync::atomic::AtomicU8::new(0));
        let f = budget_fixture(mode.clone());
        let service = f.service();
        service.lookup("npm:good", GithubMode::Demand, false).unwrap();
        mode.store(1,Ordering::SeqCst);
        let limited = service.lookup("npm:limited", GithubMode::Demand, false).unwrap();
        assert_eq!(limited.repository.unwrap().stars.status,InsightStatus::RateLimited);
        let status = service.status();
        assert_eq!((status.github_limit,status.github_remaining),(Some(60),Some(48)));
        assert!(status.github_cooldown_until.is_some());
        assert_eq!(github_count(&f),3);
        drop(service);

        mode.store(2,Ordering::SeqCst);
        f.advance(1_000);
        let service = f.service().with_credentials(|_| None, || GhOutcome::Token("gh-token-1".into()));
        let status = service.status();
        assert!(status.github_authenticated);
        assert_eq!(status.github_source.as_deref(),Some("gh"));
        assert_eq!((status.github_limit,status.github_remaining,status.github_reset_at,status.github_cooldown_until),(None,None,None,None),
            "the anonymous 48/60 budget and cooldown do not survive the switch");
        assert_eq!(service.lookup("npm:limited", GithubMode::Demand, false).unwrap().repository.unwrap().stars.status,InsightStatus::Available);
        assert_eq!(github_count(&f),5,"the rate-limited repository is fetched once");
        service.lookup("npm:good", GithubMode::Demand, false).unwrap();
        service.lookup("npm:limited", GithubMode::Demand, false).unwrap();
        assert_eq!(github_count(&f),5,"untouched and refreshed entries make no request");
        {
            let requests = github_requests(&f);
            assert!(requests[..3].iter().all(|r| !carries_authorization(r)));
            assert!(requests[3..].iter().all(|r| r.to_ascii_lowercase().contains("authorization: bearer gh-token-1")));
        }
        let status = service.status();
        assert_eq!((status.github_limit,status.github_remaining),(Some(5000),Some(4999)));
        drop(service);

        let service = f.service().with_credentials(|_| None, || GhOutcome::Token("gh-token-1".into()));
        assert_eq!(service.status().github_limit,Some(5000),"the same credential kind keeps its budget across a restart");
        let state: Value = read_json(&f.root.join("package-insights").join("provider-state.json")).unwrap();
        assert_eq!(state["credential"],"authenticated");
        assert_eq!(github_count(&f),5);
    }

    #[test]
    fn provider_state_without_a_credential_kind_is_reset_only_by_a_token() {
        let seed = json!({"schema":1,"cooldowns":{},"github":{"limit":60,"remaining":48,"resetAt":1_800_003_600_000u64}});
        let anonymous = Fixture::new(npm);
        fs::create_dir_all(anonymous.root.join("package-insights")).unwrap();
        fs::write(anonymous.root.join("package-insights").join("provider-state.json"),serde_json::to_vec(&seed).unwrap()).unwrap();
        let status = anonymous.service().with_credentials(|_| None, || GhOutcome::NotLoggedIn("gh".into())).status();
        assert_eq!((status.github_remaining,status.github_authenticated),(Some(48),false));
        let token = Fixture::new(npm);
        fs::create_dir_all(token.root.join("package-insights")).unwrap();
        fs::write(token.root.join("package-insights").join("provider-state.json"),serde_json::to_vec(&seed).unwrap()).unwrap();
        let status = token.service().with_credentials(|_| None, || GhOutcome::Token("gh-token".into())).status();
        assert_eq!((status.github_limit,status.github_remaining),(None,None));
    }

    #[test]
    fn rejected_token_gets_one_anonymous_retry_and_is_reported() {
        use std::sync::atomic::AtomicUsize;
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let f = Fixture::new(move |path| {
            if path.starts_with("/github/") && counter.fetch_add(1,Ordering::SeqCst) == 0 { Reply::body(401,vec![]) } else { npm(path) }
        });
        let service = f.service_with(Some("rejected-token".into()),Duration::from_secs(2),Duration::from_secs(5));
        let result = service.lookup("npm:fixture", GithubMode::Demand, false).unwrap();
        assert_eq!(result.repository.unwrap().stars.value,Some(0));
        let requests = github_requests(&f);
        assert_eq!(requests.len(),3,"rejected request, one anonymous retry, then the contributors request");
        assert!(carries_authorization(&requests[0]));
        assert!(!carries_authorization(&requests[1]) && !carries_authorization(&requests[2]));
        let status = service.status();
        assert!(!status.github_authenticated);
        assert!(status.github_source.is_none());
        assert_eq!(status.github_problem.as_deref(),Some(REJECTED_TOKEN));

        let f = Fixture::new(|path| if path.starts_with("/github/") { Reply::body(401,vec![]) } else { npm(path) });
        let service = f.service();
        let result = service.lookup("npm:fixture", GithubMode::Demand, false).unwrap();
        assert_eq!(result.repository.unwrap().stars.status,InsightStatus::Error);
        assert_eq!(github_count(&f),2,"a 401 without a token is a plain failure with no retry");
        assert!(service.status().github_problem.is_none());
    }

    #[test]
    fn expired_budget_is_not_reported_but_the_limit_is_kept() {
        let f = Fixture::new(|path| if path.starts_with("/github/") {
            github(path).header("X-RateLimit-Limit","60").header("X-RateLimit-Remaining","5").header("X-RateLimit-Reset","1800000060")
        } else { npm(path) });
        let service = f.service();
        service.lookup("npm:fixture", GithubMode::Demand, false).unwrap();
        f.advance(59_999);
        let status = service.status();
        assert_eq!((status.github_limit,status.github_remaining,status.github_reset_at),(Some(60),Some(5),Some(1_800_000_060_000)));
        f.advance(1);
        let status = service.status();
        assert_eq!((status.github_limit,status.github_remaining,status.github_reset_at),(Some(60),None,None));
    }

    #[test]
    fn cached_only_composes_saved_entries_without_credentials_or_requests() {
        let f = Fixture::new(npm);
        let service = f.service();
        let saved = service.lookup("npm:fixture", GithubMode::Demand, false).unwrap();
        let requests = f.paths().len();
        drop(service);
        f.advance(400 * DAY);
        let offline = f.service().with_credentials(|_| panic!("must not read credentials"), || panic!("must not run gh"));
        let cached = offline.cached_only("npm:fixture").unwrap().unwrap();
        assert_eq!(cached.newest_version,saved.newest_version);
        assert!(!cached.github_pending);
        assert_eq!(cached.repository.unwrap().stars.value,Some(0));
        assert!(offline.cached_only("npm:never-looked-up").unwrap().is_none());
        assert!(offline.cached_only("not a package").is_err());
        assert_eq!(f.paths().len(),requests);
    }
}
