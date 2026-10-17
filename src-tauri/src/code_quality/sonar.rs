//! Sonar Web API adapter; scanners and container ownership deliberately live elsewhere.
//! Shapes checked against SonarSource/sonarqube's official Web API action definitions and
//! response examples (2026-10-17): server/sonar-webserver-webapi/src/main/{java,resources}/
//! org/sonar/server/{platform,authentication,component,project,ce,issue,hotspot,projectanalysis,qualitygate}/ws.
//! https://github.com/SonarSource/sonarqube/tree/master/server/sonar-webserver-webapi
//! https://docs.sonarsource.com/sonarqube-server/extension-guide/web-api
use super::runtime::Ctl;
use super::types::*;
use reqwest::blocking::Client;
use reqwest::{Method, Url};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::Read;
use std::time::{Duration, Instant};

const RESPONSE_LIMIT: u64 = 64 * 1024 * 1024;
// Sonar's issue search cannot page beyond 10,000 results. Never report a capped set as clean.
const SEARCH_LIMIT: usize = 10_000;
type SonarResult<T> = std::result::Result<T, SonarError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SonarKind { Community, Enterprise }

#[derive(Debug, PartialEq, Eq)]
pub enum SonarError {
    Redirect { location: String },
    Unauthorized,
    Forbidden(String),
    NotFound(String),
    Unreachable(String),
    Tls(String),
    BadResponse(String),
    Other(String),
}

impl fmt::Display for SonarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Redirect { .. } => f.write_str("SonarQube returned an SSO redirect or HTML login page. Open SSO login in your browser, then create a user token in My Account → Security and save it here; browser cookies cannot authenticate analysis."),
            Self::Unauthorized => f.write_str("SonarQube rejected the user token (401 or valid=false). Replace the missing, expired or invalid token in My Account → Security after browser login."),
            Self::Forbidden(detail) => write!(f, "SonarQube denied access (403). {detail}"),
            Self::NotFound(detail) => write!(f, "SonarQube could not find the requested resource (404). {detail}"),
            Self::Unreachable(detail) => write!(f, "SonarQube is unreachable. Check its URL, server readiness, network and proxy settings. {detail}"),
            Self::Tls(detail) => write!(f, "SonarQube TLS certificate validation failed. Install the organization's trusted CA or correct the server certificate; TLS validation will not be disabled. {detail}"),
            Self::BadResponse(detail) => write!(f, "SonarQube returned a malformed or incompatible API response. Check the configured context path and server version. {detail}"),
            Self::Other(detail) => f.write_str(detail),
        }
    }
}
impl std::error::Error for SonarError {}
impl From<SonarError> for String {
    fn from(error: SonarError) -> Self { error.to_string() }
}

pub fn community_url(port: u16) -> String { format!("http://127.0.0.1:{port}") }

fn normalized_url(text: &str, allow_loopback_http: bool) -> Result<Url> {
    if text.is_empty() || text.chars().any(|c| c.is_whitespace() || c.is_control()) || text.contains('\\') {
        return Err("Enter a SonarQube URL without whitespace, control characters or backslashes.".into());
    }
    let authority = text.split_once("://").map(|(_, rest)| rest.split('/').next().unwrap_or(""));
    if authority.is_none_or(|host| host.is_empty() || host.contains('@')) {
        return Err("Enter a complete credential-free SonarQube URL with an explicit host.".into());
    }
    let mut url = Url::parse(text).map_err(|_| "Enter a complete SonarQube HTTPS URL including its optional context path.".to_string())?;
    if !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() {
        return Err("SonarQube URL must not contain credentials, a query or a fragment; save a user token separately.".into());
    }
    let loopback = literal_loopback(&url);
    if url.host_str().is_none() || (url.scheme() != "https" && !(allow_loopback_http && url.scheme() == "http" && loopback)) {
        return Err("Enterprise SonarQube requires HTTPS; HTTP is permitted only for the managed Community literal-loopback URL.".into());
    }
    let path = url.path().trim_end_matches('/').to_string();
    // Reject encoded separators/dot segments so proxies cannot reinterpret the token's context.
    let lower = text.to_ascii_lowercase();
    if path.contains("//") || lower.contains("%2f") || lower.contains("%5c")
        || lower.contains("%2e") || text.split('/').any(|p| p == "." || p == "..")
    {
        return Err("SonarQube context path must not contain ambiguous separators or dot segments.".into());
    }
    url.set_path(&path);
    Ok(url)
}
pub fn normalize_enterprise_url(text: &str) -> Result<String> {
    Ok(normalized_url(text, false)?.as_str().trim_end_matches('/').to_string())
}

pub fn identity_of(base_url: &str) -> String {
    normalized_url(base_url, true)
        .map(|u| u.as_str().trim_end_matches('/').to_string())
        .unwrap_or_else(|_| base_url.to_string())
}

pub struct SonarClient {
    kind: SonarKind,
    base: Url,
    http: Client,
    token: Option<String>,
}
impl fmt::Debug for SonarClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SonarClient").field("kind", &self.kind)
            .field("base", &self.base).field("token", &self.token.as_ref().map(|_| "<redacted>")).finish()
    }
}

fn literal_loopback(url: &Url) -> bool {
    url.host_str().and_then(|host| host.trim_matches(['[', ']']).parse::<std::net::IpAddr>().ok())
        .is_some_and(|ip| ip.is_loopback())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CeTask {
    pub id: String,
    pub status: String,
    pub analysis_id: Option<String>,
    pub error_message: Option<String>,
}
#[derive(Clone, Debug)]
pub struct SonarFindings { pub findings: Vec<QualityFinding>, pub total: usize, pub truncated: bool }
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportTask { pub task_id: String, pub project_key: String, pub dashboard_url: Option<String> }

impl SonarClient {
    pub fn new(kind: SonarKind, base_url: &str, token: Option<&str>) -> Result<Self> {
        let base = normalized_url(base_url, kind == SonarKind::Community)?;
        if kind == SonarKind::Community {
            let loopback = literal_loopback(&base);
            if base.scheme() != "http" || !loopback || base.path() != "/" {
                return Err("Managed Community must use the app-derived literal-loopback HTTP URL without a context path.".into());
            }
        }
        if token.is_some_and(|t| t.is_empty() || t.chars().any(char::is_control)) {
            return Err("Enter a nonempty SonarQube user token without control characters.".into());
        }
        let http = Client::builder().redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5)).timeout(Duration::from_secs(15))
            .build().map_err(|_| "Could not initialize the SonarQube HTTPS client.".to_string())?;
        Ok(Self { kind, base, http, token: token.map(str::to_owned) })
    }

    fn sanitize(&self, value: &str) -> String {
        match &self.token { Some(token) => value.replace(token, "[redacted]"), None => value.to_string() }
    }

    fn endpoint(&self, path: &str) -> SonarResult<Url> {
        if !path.starts_with("api/") || path.split('/').any(|s| s.is_empty() || s == "." || s == "..")
            || !path.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'/' || c == b'_') {
            return Err(SonarError::Other("Refused a SonarQube request outside the configured API context.".into()));
        }
        let url = Url::parse(&format!("{}/{path}", self.base.as_str().trim_end_matches('/')))
            .map_err(|_| malformed("Invalid API endpoint."))?;
        let prefix = format!("{}/api/", self.base.path().trim_end_matches('/'));
        if url.origin() != self.base.origin() || !url.path().starts_with(&prefix) {
            return Err(SonarError::Other("Refused to send a token outside its SonarQube origin and context.".into()));
        }
        Ok(url)
    }

    fn request(&self, method: Method, path: &str, params: &[(&str, &str)], empty_ok: bool) -> SonarResult<Value> {
        let mut url = self.endpoint(path)?;
        if !params.is_empty() { url.query_pairs_mut().extend_pairs(params.iter().copied()); }
        // Url's serializer handles query and form encoding without reqwest's optional query/form
        // features. POST identifiers stay in the form body, not in proxy/access-log URLs.
        let mut request = if method == Method::POST {
            let body = url.query().unwrap_or("").to_string();
            url.set_query(None);
            self.http.request(method, url)
                .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded").body(body)
        } else { self.http.request(method, url) };
        request = request.header(reqwest::header::ACCEPT, "application/json");
        if let Some(token) = &self.token { request = request.basic_auth(token, Some("")); }
        let response = request.send().map_err(transport_error)?;
        let status = response.status();
        if status.is_redirection() {
            // The destination is untrusted and may contain SSO state or reflected credentials.
            return Err(SonarError::Redirect { location: "[redirect not followed]".into() });
        }
        if status.as_u16() == 401 { return Err(SonarError::Unauthorized); }
        if status.as_u16() == 403 {
            let permission = match path {
                "api/projects/create" => "Grant this user Create Projects, Browse and Execute Analysis permissions for the local project.",
                "api/ce/cancel" => "Task cancellation requires project Administer or Administer System; in-progress tasks cannot be cancelled. Keep waiting for this exact task to finish.",
                "api/ce/task" => "Grant the user Execute Analysis on this project (or project/global administration).",
                _ => "Grant the user Browse and Execute Analysis on the selected project; use a user token, not an analysis-only token.",
            };
            return Err(SonarError::Forbidden(permission.into()));
        }
        if status.as_u16() == 404 { return Err(SonarError::NotFound("Verify the project/task identifier and Browse permission; private projects can be hidden from this user.".into())); }
        let is_html = response.headers().get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()).is_some_and(|v| v.to_ascii_lowercase().contains("text/html"));
        let mut bytes = Vec::new();
        response.take(RESPONSE_LIMIT + 1).read_to_end(&mut bytes)
            .map_err(|_| malformed("Could not read the complete API response."))?;
        if bytes.len() as u64 > RESPONSE_LIMIT { return Err(malformed("API response exceeds the 64 MiB limit; narrow the project scope.")); }
        let first = String::from_utf8_lossy(&bytes[..bytes.len().min(512)]).trim_start().to_ascii_lowercase();
        if is_html || first.starts_with("<!doctype html") || first.starts_with("<html") {
            return Err(SonarError::Redirect { location: "[HTML login page]".into() });
        }
        if !status.is_success() {
            return Err(SonarError::Other(format!("SonarQube API {path} returned HTTP {}. Check server logs, project permissions and server readiness.", status.as_u16())));
        }
        if empty_ok && bytes.is_empty() { return Ok(Value::Null); }
        let mut data: Value = serde_json::from_slice(&bytes)
            .map_err(|_| malformed("Expected JSON, not a login page or truncated response."))?;
        if let Some(token) = &self.token { redact_api_messages(&mut data, token); }
        Ok(data)
    }

    pub fn system_status(&self) -> SonarResult<String> {
        let data = self.request(Method::GET, "api/system/status", &[], false)?;
        Ok(required_str(&data, "status")?.to_string())
    }
    pub fn validate_token(&self) -> SonarResult<()> {
        if self.token.is_none() { return Err(SonarError::Unauthorized); }
        match self.request(Method::GET, "api/authentication/validate", &[], false)?.get("valid").and_then(Value::as_bool) {
            Some(true) => Ok(()), Some(false) => Err(SonarError::Unauthorized), None => Err(malformed("authentication/validate has no boolean valid field.")),
        }
    }
    pub fn ensure_project(&self, key: &str, name: &str, create_if_missing: bool) -> SonarResult<()> {
        validate_identifier(key)?;
        let check = self.request(Method::GET, "api/components/show", &[("component", key)], false);
        match check {
            Ok(data) => {
                if required_str(&data["component"], "key")? != key { return Err(malformed("Project lookup returned a different project key.")); }
                return Ok(());
            }
            Err(SonarError::NotFound(_)) if self.kind == SonarKind::Community && create_if_missing => {},
            Err(SonarError::NotFound(_)) => return Err(SonarError::NotFound(if self.kind == SonarKind::Enterprise {
                "Enterprise requires an existing project; OMP never creates projects there. Ask its administrator to confirm the key and grant Browse + Execute Analysis.".into()
            } else { "Create this local project or allow project creation, and grant Browse + Execute Analysis.".into() })),
            Err(error) => return Err(error),
        }
        let data = self.request(Method::POST, "api/projects/create", &[("project", key), ("name", name)], false)?;
        if required_str(&data["project"], "key")? != key { return Err(malformed("Project creation returned a different project key.")); }
        self.ensure_project(key, name, false)
    }

    pub fn task_status(&self, task_id: &str) -> SonarResult<CeTask> {
        validate_identifier(task_id)?;
        let data = self.request(Method::GET, "api/ce/task", &[("id", task_id)], false)?;
        let task = &data["task"];
        let id = required_str(task, "id")?;
        if id != task_id { return Err(malformed("Compute Engine returned another task's identifier.")); }
        let status = required_str(task, "status")?;
        if !matches!(status, "SUCCESS" | "FAILED" | "CANCELED" | "PENDING" | "IN_PROGRESS") { return Err(malformed("Unknown Compute Engine task status.")); }
        let analysis_id = optional_string(task, "analysisId")?;
        if status == "SUCCESS" && analysis_id.is_none() { return Err(malformed("Successful Compute Engine task has no analysisId.")); }
        Ok(CeTask { id: id.into(), status: status.into(), analysis_id, error_message: optional_string(task, "errorMessage")?.map(|s| self.sanitize(&s)) })
    }
    pub fn cancel_task(&self, task_id: &str) -> SonarResult<()> {
        validate_identifier(task_id)?;
        self.request(Method::POST, "api/ce/cancel", &[("id", task_id)], true)?;
        Ok(())
    }
    pub fn submit_wait_task(&self, task_id: &str, ctl: &Ctl, timeout: Duration) -> SonarResult<CeTask> {
        let started = Instant::now();
        let mut cancellation_attempted = false;
        loop {
            if started.elapsed() >= timeout {
                return Err(SonarError::Other("SonarQube task is unresolved after the wait timeout. Retain its task ID and the busy gate; reconcile this task before another scan.".into()));
            }
            if ctl.cancelled() && !cancellation_attempted {
                cancellation_attempted = true;
                if let Err(error) = self.cancel_task(task_id) { ctl.say(&error.to_string()); }
                ctl.say("Waiting for SonarQube task to finish");
            }
            let task = self.task_status(task_id)?;
            if matches!(task.status.as_str(), "SUCCESS" | "FAILED" | "CANCELED") { return Ok(task); }
            let delay = Duration::from_secs(1).min(timeout.saturating_sub(started.elapsed()));
            let until = Instant::now() + delay;
            while Instant::now() < until {
                if ctl.cancelled() && !cancellation_attempted { break; }
                std::thread::sleep(Duration::from_millis(50).min(until.saturating_duration_since(Instant::now())));
            }
        }
    }
    pub fn quality_gate(&self, analysis_id: &str) -> SonarResult<QualityGate> {
        validate_identifier(analysis_id)?;
        let data = self.request(Method::GET, "api/qualitygates/project_status", &[("analysisId", analysis_id)], false)?;
        match required_str(&data["projectStatus"], "status")? {
            "OK" => Ok(QualityGate::Passed), "ERROR" => Ok(QualityGate::Failed),
            "NONE" | "WARN" => Ok(QualityGate::Unknown), _ => Err(malformed("Unknown quality gate status.")),
        }
    }
    pub fn latest_analysis_id(&self, key: &str) -> SonarResult<Option<String>> {
        validate_identifier(key)?;
        let data = self.request(Method::GET, "api/project_analyses/search", &[("project", key), ("ps", "1"), ("p", "1")], false)?;
        let analyses = data.get("analyses").and_then(Value::as_array).ok_or_else(|| malformed("Missing analyses array."))?;
        analyses.first().map(|a| required_str(a, "key").map(str::to_string)).transpose()
    }
    pub fn issues(&self, key: &str, max: usize) -> SonarResult<SonarFindings> { self.collect(key, max, false) }
    pub fn hotspots(&self, key: &str, max: usize) -> SonarResult<SonarFindings> { self.collect(key, max, true) }
    pub fn dashboard_url(&self, key: &str) -> String {
        let mut url = self.base.clone();
        url.set_path(&format!("{}/dashboard", self.base.path().trim_end_matches('/')));
        url.query_pairs_mut().append_pair("id", key);
        url.to_string()
    }

    fn collect(&self, key: &str, max: usize, hotspot: bool) -> SonarResult<SonarFindings> {
        validate_identifier(key)?;
        let cap = max.min(SEARCH_LIMIT);
        let page_size = cap.clamp(1, 500).to_string();
        let mut findings = Vec::new();
        let mut seen = HashSet::new();
        let mut consumed = 0usize;
        let mut total = 0usize;
        for page in 1..=SEARCH_LIMIT {
            let page_text = page.to_string();
            // `resolved=false` remains supported alongside modern issueStatus/impacts response fields.
            // No issueStatuses filter: do not accidentally hide a new unresolved status on newer servers.
            // Hotspots' projectKey parameter was removed in 2026.5; `project` works since 10.2.
            let (endpoint, params, field) = if hotspot {
                ("api/hotspots/search", [("project", key), ("status", "TO_REVIEW"), ("ps", page_size.as_str()), ("p", page_text.as_str())], "hotspots")
            } else {
                ("api/issues/search", [("componentKeys", key), ("resolved", "false"), ("ps", page_size.as_str()), ("p", page_text.as_str())], "issues")
            };
            let data = self.request(Method::GET, endpoint, &params, false)?;
            let rows = data.get(field).and_then(Value::as_array).ok_or_else(|| malformed("Missing findings array."))?;
            let paging = &data["paging"];
            let reported_total = required_usize(paging, "total")?;
            let actual_size = required_usize(paging, "pageSize")?;
            if required_usize(paging, "pageIndex")? != page || actual_size == 0 || actual_size > 500 || rows.len() > actual_size {
                return Err(malformed("Inconsistent findings pagination."));
            }
            total = total.max(reported_total);
            let components = component_paths(&data, key)?;
            if rows.is_empty() && consumed < reported_total { return Err(malformed("Findings page is empty before the advertised total.")); }
            for row in rows {
                let row_id = required_str(row, "key")?;
                if !seen.insert(row_id.to_string()) { return Err(malformed("Findings pagination repeated a result; collection is not a consistent snapshot.")); }
                consumed += 1;
                if findings.len() < cap { findings.push(map_finding(row, key, &components, hotspot)?); }
            }
            if findings.len() >= cap || consumed >= reported_total {
                return Ok(SonarFindings { truncated: consumed < total || consumed > findings.len(), findings, total });
            }
        }
        Ok(SonarFindings { findings, total, truncated: true })
    }
}

fn malformed(message: &str) -> SonarError { SonarError::BadResponse(message.into()) }
fn required_str<'a>(data: &'a Value, key: &str) -> SonarResult<&'a str> {
    data.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or_else(|| malformed(&format!("Missing or invalid {key} field.")))
}
fn optional_string(data: &Value, key: &str) -> SonarResult<Option<String>> {
    match data.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if !value.is_empty() => Ok(Some(value.clone())),
        _ => Err(malformed(&format!("Invalid {key} field."))),
    }
}
fn required_usize(data: &Value, key: &str) -> SonarResult<usize> {
    data.get(key).and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok()).ok_or_else(|| malformed(&format!("Missing or invalid {key} pagination field.")))
}
fn validate_identifier(value: &str) -> SonarResult<()> {
    if value.is_empty() || value.len() > 2048 || value.chars().any(char::is_control) {
        Err(SonarError::Other("Enter a nonempty SonarQube project/task identifier without control characters (maximum 2048 bytes).".into()))
    } else { Ok(()) }
}
fn transport_error(error: reqwest::Error) -> SonarError {
    // reqwest wraps rustls certificate errors in several source layers; never expose raw URLs/auth.
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&error);
    let mut tls = false;
    while let Some(cause) = source {
        let text = cause.to_string().to_ascii_lowercase();
        tls |= text.contains("certificate") || text.contains("tls") || text.contains("ssl")
            || text.contains("unknownissuer") || text.contains("invalidcontenttype")
            || text.contains("corrupt message") || text.contains("invalid peer");
        source = cause.source();
    }
    if tls { SonarError::Tls("No API credentials were forwarded to another server.".into()) }
    else if error.is_timeout() { SonarError::Unreachable("The API request timed out; any submitted task remains unresolved until reconciliation.".into()) }
    else { SonarError::Unreachable("The connection could not be completed.".into()) }
}

fn redact_api_messages(value: &mut Value, token: &str) {
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                if matches!(key.as_str(), "message" | "errorMessage") {
                    if let Value::String(text) = value {
                        if text.contains(token) { *text = text.replace(token, "[redacted]"); }
                    }
                } else { redact_api_messages(value, token); }
            }
        }
        Value::Array(values) => { for value in values { redact_api_messages(value, token); } }
        _ => {}
    }
}

fn safe_path(path: &str) -> Option<String> {
    let path = path.replace('\\', "/");
    if path.is_empty() || path.starts_with('/') || path.contains(':') || path.chars().any(char::is_control)
        || path.split('/').any(|p| p == ".." || p.is_empty()) { return None; }
    Some(path)
}
fn component_paths(data: &Value, project: &str) -> SonarResult<HashMap<String, String>> {
    let mut paths = HashMap::new();
    if let Some(components) = data.get("components") {
        let rows = components.as_array().ok_or_else(|| malformed("Invalid components array."))?;
        let prefix = format!("{project}:");
        for component in rows {
            let key = required_str(component, "key")?;
            if key.starts_with(&prefix) {
                if let Some(path) = component.get("path").and_then(Value::as_str).and_then(safe_path) { paths.insert(key.to_string(), path); }
            }
        }
    }
    Ok(paths)
}
fn classic_severity(value: &str) -> SonarResult<Severity> {
    match value {
        "BLOCKER" => Ok(Severity::Critical), "CRITICAL" => Ok(Severity::High), "MAJOR" => Ok(Severity::Medium),
        "MINOR" => Ok(Severity::Low), "INFO" => Ok(Severity::Info), _ => Err(malformed("Unknown classic issue severity.")),
    }
}
fn impact_severity(value: &str) -> SonarResult<Severity> {
    match value {
        "BLOCKER" => Ok(Severity::Critical), "HIGH" => Ok(Severity::High), "MEDIUM" => Ok(Severity::Medium),
        "LOW" => Ok(Severity::Low), "INFO" => Ok(Severity::Info), _ => Err(malformed("Unknown impact/hotspot severity.")),
    }
}
fn category(value: &str) -> String {
    match value {
        "BUG" => "Bug", "VULNERABILITY" => "Vulnerability", "CODE_SMELL" => "Code smell",
        "MAINTAINABILITY" => "Maintainability", "RELIABILITY" => "Reliability", "SECURITY" => "Security",
        _ => value,
    }.into()
}
fn position(data: &Value, key: &str, offset: bool) -> SonarResult<Option<u32>> {
    match data.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let n = value.as_u64().and_then(|n| u32::try_from(n).ok()).ok_or_else(|| malformed("Invalid source range."))?;
            if offset { n.checked_add(1).map(Some).ok_or_else(|| malformed("Source column overflow.")) }
            else if n == 0 { Err(malformed("Source line must be positive.")) } else { Ok(Some(n)) }
        }
    }
}
fn map_finding(row: &Value, project: &str, components: &HashMap<String, String>, hotspot: bool) -> SonarResult<QualityFinding> {
    if row.get("project").and_then(Value::as_str).is_some_and(|p| p != project) { return Err(malformed("Findings include a different project.")); }
    let rule_id = required_str(row, if hotspot { "ruleKey" } else { "rule" })?.to_string();
    let (severity, category) = if hotspot {
        (impact_severity(required_str(row, "vulnerabilityProbability")?)?, "Security hotspot".to_string())
    } else if let Some(impacts) = row.get("impacts") {
        let impacts = impacts.as_array().ok_or_else(|| malformed("Invalid impacts array."))?;
        if impacts.is_empty() {
            (classic_severity(required_str(row, "severity")?)?, category(required_str(row, "type")?))
        } else {
            let mut best = Severity::Info;
            let mut qualities = Vec::new();
            for impact in impacts {
                best = best.min(impact_severity(required_str(impact, "severity")?)?);
                qualities.push(category(required_str(impact, "softwareQuality")?));
            }
            qualities.sort(); qualities.dedup();
            (best, qualities.join(", "))
        }
    } else { (classic_severity(required_str(row, "severity")?)?, category(required_str(row, "type")?)) };
    let raw_message = required_str(row, "message")?;
    let message = if hotspot { format!("Review needed: {raw_message}") } else { raw_message.to_string() };
    let component = row.get("component").and_then(Value::as_str);
    let prefix = format!("{project}:");
    let path = component.and_then(|key| components.get(key).cloned().or_else(|| key.strip_prefix(&prefix).and_then(safe_path)));
    let range = &row["textRange"];
    let line = position(range, "startLine", false)?.or(position(row, "line", false)?);
    let column = position(range, "startOffset", true)?;
    let end_line = position(range, "endLine", false)?;
    if line.zip(end_line).is_some_and(|(start, end)| start > end) { return Err(malformed("Source range ends before its start.")); }
    let hash = Sha256::digest(format!("SonarQube|{rule_id}|{}|{}|{message}", path.as_deref().unwrap_or(""), line.map(|l| l.to_string()).unwrap_or_default()).as_bytes());
    let id = format!("{hash:x}")[..16].to_string();
    Ok(QualityFinding { id, tools: vec!["SonarQube".into()], rule_id, category, severity, message, path, line, column, end_line, help_url: None, hotspot })
}

pub fn parse_report_task(text: &str, trusted_internal_url: &str, expected_project_key: &str) -> Result<ReportTask> {
    if text.len() > 1024 * 1024 { return Err("SonarQube report-task.txt exceeds 1 MiB.".into()); }
    let mut fields = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') { continue; }
        let (key, value) = line.split_once('=').ok_or_else(|| "Malformed SonarQube report-task.txt: expected key=value.".to_string())?;
        if key.is_empty() || fields.insert(key, value).is_some() || value.chars().any(char::is_control) {
            return Err("Malformed SonarQube report-task.txt: duplicate/empty key or control character.".into());
        }
    }
    if fields.get("serverUrl").map(|s| s.trim_end_matches('/')) != Some(trusted_internal_url.trim_end_matches('/')) {
        return Err("SonarQube report-task serverUrl does not equal this job's trusted scanner URL; no tool-provided URL was followed.".into());
    }
    if fields.get("projectKey").copied() != Some(expected_project_key) { return Err("SonarQube report-task projectKey does not match this job.".into()); }
    let task_id = fields.get("ceTaskId").copied().ok_or_else(|| "SonarQube report-task.txt has no ceTaskId.".to_string())?;
    if task_id.is_empty() || !task_id.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_')) {
        return Err("SonarQube report-task.txt has an invalid ceTaskId.".into());
    }
    // dashboardUrl/ceTaskUrl are untrusted scanner output. The backend rebuilds every URL from its
    // saved public URL; do not let even a display-only tool URL enter the native dashboard opener.
    Ok(ReportTask { task_id: task_id.to_string(), project_key: expected_project_key.to_string(), dashboard_url: None })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;
    use std::net::TcpListener;
    use std::sync::Arc;
    use parking_lot::Mutex;
    use std::thread::{self, JoinHandle};

    struct Reply { status: u16, headers: Vec<(&'static str, String)>, body: String }
    impl Reply {
        fn json(body: Value) -> Self { Self { status: 200, headers: vec![("Content-Type", "application/json".into())], body: body.to_string() } }
        fn status(status: u16) -> Self { Self { status, headers: vec![], body: String::new() } }
    }
    struct Fake {
        url: String,
        requests: Arc<Mutex<Vec<String>>>,
        worker: Option<JoinHandle<()>>,
    }
    impl Fake {
        fn new(replies: Vec<Reply>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let saved = requests.clone();
            let worker = thread::spawn(move || {
                // Nonblocking admission prevents an assertion failure from leaving a stuck test thread.
                listener.set_nonblocking(true).unwrap();
                for reply in replies {
                    let deadline = Instant::now() + Duration::from_secs(10);
                    let (mut socket, bytes) = 'conn: loop {
                        let mut socket = loop {
                            match listener.accept() {
                                Ok((socket, _)) => break socket,
                                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                    if Instant::now() >= deadline { return; }
                                    thread::sleep(Duration::from_millis(5));
                                }
                                Err(e) => panic!("fake server accept: {e}"),
                            }
                        };
                        socket.set_nonblocking(false).unwrap();
                        socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                        let mut bytes = Vec::new();
                        let mut buffer = [0u8; 1024];
                        loop {
                            // A reset or empty connection (pooled/aborted client socket) is not a request.
                            let n = socket.read(&mut buffer).unwrap_or(0);
                            if n == 0 { break; }
                            bytes.extend_from_slice(&buffer[..n]);
                            if let Some(end) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                                let head = String::from_utf8_lossy(&bytes[..end]);
                                let length = head.lines().find_map(|line| {
                                    let (name, value) = line.split_once(':')?;
                                    name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().unwrap())
                                }).unwrap_or(0);
                                if bytes.len() >= end + 4 + length { break; }
                            }
                            assert!(bytes.len() < 64 * 1024);
                        }
                        if bytes.is_empty() { continue 'conn; }
                        break (socket, bytes);
                    };
                    saved.lock().push(String::from_utf8(bytes).unwrap());
                    let reason = match reply.status { 200 => "OK", 204 => "No Content", 302 => "Found", 401 => "Unauthorized", 403 => "Forbidden", 404 => "Not Found", _ => "Error" };
                    let mut response = format!("HTTP/1.1 {} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n", reply.status, reply.body.len());
                    for (name, value) in reply.headers { response.push_str(&format!("{name}: {value}\r\n")); }
                    response.push_str("\r\n");
                    response.push_str(&reply.body);
                    let _ = socket.write_all(response.as_bytes());
                }
            });
            Self { url, requests, worker: Some(worker) }
        }
        fn client(&self) -> SonarClient {
            let mut client = SonarClient::new(SonarKind::Community, &self.url, Some("private-token")).unwrap();
            client.http = Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(2)).timeout(Duration::from_secs(3)).build().unwrap();
            client
        }
        fn enterprise(&self, context: &str) -> SonarClient {
            // Private test-only construction permits a loopback HTTP Enterprise fixture. The public
            // constructor remains strictly HTTPS and cannot be influenced by frontend inputs.
            let mut client = self.client();
            client.kind = SonarKind::Enterprise;
            client.base = Url::parse(&format!("{}{context}", self.url)).unwrap();
            client
        }
        fn finish(mut self) -> Vec<String> {
            self.worker.take().unwrap().join().unwrap();
            let requests = self.requests.lock().clone();
            requests
        }
    }
    fn page(field: &str, index: usize, size: usize, total: usize, rows: Vec<Value>) -> Reply {
        let mut body = json!({"paging":{"pageIndex":index,"pageSize":size,"total":total}});
        body[field] = Value::Array(rows);
        Reply::json(body)
    }
    fn issue(id: usize, severity: &str) -> Value {
        json!({"key":format!("i{id}"),"project":"my:project","component":"my:project:src/main.rs",
            "rule":"rust:S1","severity":severity,"type":"BUG","message":format!("Review {id}"),"line":id+1})
    }

    #[test]
    fn url_normalization_and_credential_identity_table() {
        for (input, expected) in [
            ("https://SONAR.Example:443/", "https://sonar.example"),
            ("https://SONAR.Example/sonar///", "https://sonar.example/sonar"),
            ("https://sonar.example/a/B/", "https://sonar.example/a/B"),
            ("https://sonar.example:9443/sonar", "https://sonar.example:9443/sonar"),
            ("https://[::1]/", "https://[::1]"),
        ] { assert_eq!(normalize_enterprise_url(input).unwrap(), expected); }
        for input in ["http://sonar.example", "http://127.0.0.1:9000", "http://", "https://",
            "https://u:p@sonar.example", "https://u@sonar.example", "https://sonar.example?token=x",
            "https://sonar.example#x", " https://sonar.example", "https://sonar.example/\n",
            "https://sonar.example/a/../b", "https://sonar.example/a%2fb", "https://sonar.example/a//b",
            "https://sonar.example/a\\b", "file:///sonar"] {
            assert!(normalize_enterprise_url(input).is_err(), "accepted {input}");
        }
        assert_eq!(community_url(9123), "http://127.0.0.1:9123");
        assert_eq!(identity_of("https://SONAR.example:443/sonar/"), "https://sonar.example/sonar");
        assert_ne!(identity_of("https://sonar.example/a"), identity_of("https://sonar.example/b"));
        assert_eq!(identity_of("http://127.0.0.1:9000/"), "http://127.0.0.1:9000");
        assert!(SonarClient::new(SonarKind::Community, "http://localhost:9000", None).is_err());
        assert!(SonarClient::new(SonarKind::Community, "http://127.0.0.1:9000/sonar", None).is_err());
        assert!(SonarClient::new(SonarKind::Enterprise, "http://127.0.0.1:9000", None).is_err());
    }

    #[test]
    fn redirects_and_html_are_sso_errors_without_following_or_cookie_authentication() {
        let fake = Fake::new(vec![Reply { status: 302, headers: vec![
            ("Location", "https://other.example/login?secret=private-token".into()),
            ("Set-Cookie", "SESSION=must-not-be-reused".into())], body: String::new() },
            Reply { status: 200, headers: vec![("Content-Type", "text/html".into())], body: "<html>SSO login</html>".into() },
            Reply::json(json!({"valid":true}))]);
        let client = fake.enterprise("/sonar");
        for _ in 0..2 {
            let error = client.validate_token().unwrap_err();
            assert!(matches!(error, SonarError::Redirect { .. }));
            assert!(error.to_string().contains("My Account"));
            assert!(!format!("{error:?}").contains("private-token"));
        }
        client.validate_token().unwrap();
        let requests = fake.finish();
        assert_eq!(requests.len(), 3);
        for request in requests {
            assert!(request.starts_with("GET /sonar/api/authentication/validate HTTP/1.1"));
            let lower = request.to_ascii_lowercase();
            assert!(lower.contains("authorization: basic "));
            assert!(!lower.contains("\r\ncookie:"));
        }
    }

    #[test]
    fn token_rejection_for_false_401_and_missing_token_is_distinct_from_forbidden() {
        let fake = Fake::new(vec![Reply::json(json!({"valid":false})), Reply::status(401), Reply::status(403)]);
        let client = fake.client();
        assert_eq!(client.validate_token().unwrap_err(), SonarError::Unauthorized);
        assert_eq!(client.validate_token().unwrap_err(), SonarError::Unauthorized);
        let error = client.validate_token().unwrap_err();
        assert!(matches!(error, SonarError::Forbidden(_)));
        assert!(error.to_string().contains("Browse and Execute Analysis"));
        let mut absent = fake.client();
        absent.token = None;
        assert_eq!(absent.validate_token().unwrap_err(), SonarError::Unauthorized);
        assert_eq!(fake.finish().len(), 3);
    }

    #[test]
    fn permission_diagnostics_identify_task_creation_and_cancellation_requirements() {
        let fake = Fake::new(vec![Reply::status(403), Reply::status(403), Reply::status(404), Reply::status(403)]);
        let client = fake.client();
        assert!(client.task_status("task").unwrap_err().to_string().contains("Execute Analysis"));
        assert!(client.cancel_task("task").unwrap_err().to_string().contains("Administer"));
        assert!(client.ensure_project("p", "Project", true).unwrap_err().to_string().contains("Create Projects"));
        assert_eq!(fake.finish().len(), 4);
    }

    #[test]
    fn context_path_is_preserved_and_foreign_endpoints_are_refused_before_auth() {
        let fake = Fake::new(vec![Reply::json(json!({"status":"UP"})), Reply::json(json!({"analyses":[{"key":"analysis-2"}]}))]);
        let client = fake.enterprise("/sonar");
        assert_eq!(client.system_status().unwrap(), "UP");
        assert_eq!(client.latest_analysis_id("a:b").unwrap().as_deref(), Some("analysis-2"));
        assert!(client.dashboard_url("a:b").starts_with(&format!("{}/sonar/dashboard?id=a%3Ab", fake.url)));
        for path in ["https://evil.example/api/ce/task", "../api/ce/task", "/api/ce/task", "api/../evil", "api/%2f/evil"] {
            assert!(client.endpoint(path).is_err());
        }
        let requests = fake.finish();
        assert!(requests[0].starts_with("GET /sonar/api/system/status "));
        assert!(requests[1].starts_with("GET /sonar/api/project_analyses/search?project=a%3Ab&ps=1&p=1 "));
    }

    #[test]
    fn three_issue_pages_preserve_classic_impacts_ranges_and_truncation() {
        let first = issue(0, "BLOCKER");
        let mut impacted = issue(1, "INFO");
        impacted["impacts"] = json!([{"softwareQuality":"MAINTAINABILITY","severity":"LOW"},{"softwareQuality":"SECURITY","severity":"HIGH"}]);
        impacted["textRange"] = json!({"startLine":7,"endLine":8,"startOffset":0});
        let fake = Fake::new(vec![
            page("issues",1,2,8,vec![first, impacted]),
            page("issues",2,2,8,vec![issue(2,"MAJOR"),issue(3,"MINOR")]),
            page("issues",3,2,8,vec![issue(4,"CRITICAL"),issue(5,"INFO")]),
        ]);
        let result = fake.client().issues("my:project", 5).unwrap();
        assert_eq!(result.total, 8);
        assert!(result.truncated);
        assert_eq!(result.findings.len(), 5);
        assert_eq!(result.findings.iter().map(|f| f.severity).collect::<Vec<_>>(),
            vec![Severity::Critical,Severity::High,Severity::Medium,Severity::Low,Severity::High]);
        assert_eq!(result.findings[1].category, "Maintainability, Security");
        assert_eq!(result.findings[1].line, Some(7));
        assert_eq!(result.findings[1].column, Some(1));
        assert_eq!(result.findings[1].end_line, Some(8));
        assert_eq!(result.findings[0].path.as_deref(), Some("src/main.rs"));
        assert_eq!(result.findings[0].id.len(), 16);
        let requests = fake.finish();
        for (i, request) in requests.iter().enumerate() {
            assert!(request.contains("componentKeys=my%3Aproject&resolved=false&ps=5"));
            assert!(request.contains(&format!("&p={} ",i+1)));
        }
    }

    #[test]
    fn component_paths_only_strip_the_known_project_prefix_and_hotspots_need_review() {
        let fake = Fake::new(vec![Reply::json(json!({
            "paging":{"pageIndex":1,"pageSize":100,"total":2},
            "hotspots":[
                {"key":"h1","project":"p","component":"p:opaque","ruleKey":"python:S1","message":"Review TLS","vulnerabilityProbability":"HIGH","line":4,"status":"TO_REVIEW"},
                {"key":"h2","project":"p","component":"foreign:private.py","ruleKey":"python:S2","message":"Review crypto","vulnerabilityProbability":"LOW","status":"TO_REVIEW"}],
            "components":[{"key":"p:opaque","path":"src/test.py"}]
        }))]);
        let result = fake.client().hotspots("p", 100).unwrap();
        assert!(!result.truncated);
        assert_eq!(result.total, 2);
        assert!(result.findings.iter().all(|f| f.hotspot && f.message.starts_with("Review needed: ")));
        assert_eq!(result.findings[0].severity, Severity::High);
        assert_eq!(result.findings[1].severity, Severity::Low);
        assert_eq!(result.findings[0].path.as_deref(), Some("src/test.py"));
        assert!(result.findings[1].path.is_none());
        let requests = fake.finish();
        assert!(requests[0].contains("/api/hotspots/search?project=p&status=TO_REVIEW&"));
        assert!(!requests[0].contains("projectKey="));
    }

    #[test]
    fn failed_and_canceled_ce_tasks_are_terminal_not_successful_analysis() {
        for status in ["FAILED", "CANCELED"] {
            let fake = Fake::new(vec![Reply::json(json!({"task":{"id":"task-1","status":status,"errorMessage":"Analysis did not finish"}}))]);
            let task = fake.client().submit_wait_task("task-1", &Ctl::silent(), Duration::from_secs(5)).unwrap();
            assert_eq!(task.status, status);
            assert!(task.analysis_id.is_none());
            fake.finish();
        }
    }

    #[test]
    fn cancellation_attempts_exact_ce_task_then_waits_for_terminal_state() {
        let fake = Fake::new(vec![Reply::status(403), Reply::json(json!({"task":{"id":"exact-task","status":"CANCELED"}}))]);
        let ctl = Ctl::silent();
        ctl.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(fake.client().submit_wait_task("exact-task", &ctl, Duration::from_secs(5)).unwrap().status, "CANCELED");
        let requests = fake.finish();
        assert!(requests[0].starts_with("POST /api/ce/cancel "));
        assert!(requests[0].ends_with("id=exact-task"));
        assert!(requests[1].starts_with("GET /api/ce/task?id=exact-task "));
    }

    #[test]
    fn successful_ce_and_failed_quality_gate_are_separate_from_execution_errors() {
        let fake = Fake::new(vec![Reply::json(json!({"task":{"id":"t","status":"SUCCESS","analysisId":"a"}})),
            Reply::json(json!({"projectStatus":{"status":"ERROR"}})),
            Reply::json(json!({"analyses":[]}))]);
        let client = fake.client();
        assert_eq!(client.task_status("t").unwrap().analysis_id.as_deref(), Some("a"));
        assert_eq!(client.quality_gate("a").unwrap(), QualityGate::Failed);
        assert_eq!(client.latest_analysis_id("p").unwrap(), None);
        let requests = fake.finish();
        assert!(requests[1].contains("analysisId=a"));
    }

    #[test]
    fn community_creates_missing_project_but_enterprise_never_creates() {
        let fake = Fake::new(vec![Reply::status(404), Reply::json(json!({"project":{"key":"p"}})),
            Reply::json(json!({"component":{"key":"p"}}))]);
        fake.client().ensure_project("p", "Project name", true).unwrap();
        let requests = fake.finish();
        assert_eq!(requests.len(), 3);
        assert!(requests[1].starts_with("POST /api/projects/create "));
        assert!(requests[1].ends_with("project=p&name=Project+name"));
        let enterprise = Fake::new(vec![Reply::status(404)]);
        let error = enterprise.enterprise("/sonar").ensure_project("p","Project",true).unwrap_err();
        assert!(matches!(error,SonarError::NotFound(_)));
        assert!(error.to_string().contains("OMP never creates"));
        assert_eq!(enterprise.finish().len(),1);
    }

    #[test]
    fn malformed_responses_missing_reports_and_foreign_tasks_are_not_zero_findings() {
        let fake = Fake::new(vec![
            Reply { status:200,headers:vec![],body:"not json private-token".into() },
            Reply::json(json!({})),
            Reply::json(json!({"task":{"id":"foreign","status":"SUCCESS","analysisId":"a"}})),
            Reply::json(json!({"task":{"id":"t","status":"SUCCESS"}})),
            Reply::json(json!({"paging":{"pageIndex":1,"pageSize":1,"total":2},"issues":[]})),
        ]);
        let client = fake.client();
        assert!(matches!(client.system_status(),Err(SonarError::BadResponse(_))));
        assert!(matches!(client.validate_token(),Err(SonarError::BadResponse(_))));
        assert!(matches!(client.task_status("t"),Err(SonarError::BadResponse(_))));
        assert!(matches!(client.task_status("t"),Err(SonarError::BadResponse(_))));
        assert!(matches!(client.issues("p",10),Err(SonarError::BadResponse(_))));
        assert!(!format!("{client:?}").contains("private-token"));
        fake.finish();
    }

    #[test]
    fn unreachable_and_certificate_errors_have_distinct_actionable_text() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}",listener.local_addr().unwrap());
        drop(listener);
        let client = SonarClient::new(SonarKind::Community,&url,None).unwrap();
        assert!(matches!(client.system_status(),Err(SonarError::Unreachable(_))));
        let tls = SonarError::Tls("Untrusted certificate chain.".into()).to_string();
        assert!(tls.contains("trusted CA"));
        assert!(!tls.contains("unreachable"));
        assert!(SonarError::Other("Installer unrelated".into()).to_string().contains("Installer"));
    }

    #[test]
    fn tls_handshake_failure_is_not_reported_as_an_invalid_token_or_http_login() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("https://{}", listener.local_addr().unwrap());
        let worker = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            let mut hello = [0; 4096];
            let _ = socket.read(&mut hello).unwrap();
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        let mut client = SonarClient::new(SonarKind::Enterprise, &url, Some("private-token")).unwrap();
        client.http = Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(2)).timeout(Duration::from_secs(3)).build().unwrap();
        let error = client.system_status().unwrap_err();
        assert!(matches!(error, SonarError::Tls(_)), "{error:?}");
        assert!(error.to_string().contains("TLS"));
        assert!(!format!("{error:?}").contains("private-token"));
        worker.join().unwrap();
    }

    #[test]
    fn ce_timeout_is_unresolved_and_messages_redact_the_managed_token() {
        let fake = Fake::new(vec![Reply::json(json!({"task":{"id":"t","status":"FAILED","errorMessage":"Rejected private-token"}})),
            page("issues",1,1,1,vec![json!({"key":"i1","project":"p","component":"p:a.py","rule":"python:S1",
                "severity":"MAJOR","type":"BUG","message":"Check private-token"})])]);
        let client = fake.client();
        let task = client.task_status("t").unwrap();
        assert_eq!(task.error_message.as_deref(), Some("Rejected [redacted]"));
        assert_eq!(client.issues("p",1).unwrap().findings[0].message,"Check [redacted]");
        let timeout = client.submit_wait_task("t",&Ctl::silent(),Duration::ZERO).unwrap_err();
        assert!(timeout.to_string().contains("unresolved"));
        assert!(timeout.to_string().contains("busy gate"));
        fake.finish();
    }

    #[test]
    fn report_task_accepts_only_exact_server_and_project_and_never_returns_tool_urls() {
        let valid = "projectKey=p\nserverUrl=http://owned-sonar:9000/\nceTaskId=AX_task-1\nceTaskUrl=https://evil.example/task\ndashboardUrl=https://evil.example/dashboard\n";
        let task = parse_report_task(valid,"http://owned-sonar:9000","p").unwrap();
        assert_eq!(task.task_id,"AX_task-1");
        assert_eq!(task.project_key,"p");
        assert_eq!(task.dashboard_url,None);
        assert!(parse_report_task(valid,"http://other:9000","p").is_err());
        assert!(parse_report_task(valid,"http://owned-sonar:9000","other").is_err());
        assert!(parse_report_task(&valid.replace("owned-sonar","owned-sonar.evil"),"http://owned-sonar:9000","p").is_err());
        assert!(parse_report_task(&format!("{valid}ceTaskId=second\n"),"http://owned-sonar:9000","p").is_err());
        assert!(parse_report_task(&valid.replace("AX_task-1","../foreign"),"http://owned-sonar:9000","p").is_err());
        assert!(parse_report_task("projectKey=p\nserverUrl=http://owned-sonar:9000","http://owned-sonar:9000","p").is_err());
    }

    #[test]
    fn severity_tables_unknown_severity_and_unsafe_paths_are_explicit() {
        for (raw, expected) in [("BLOCKER",Severity::Critical),("CRITICAL",Severity::High),("MAJOR",Severity::Medium),("MINOR",Severity::Low),("INFO",Severity::Info)] {
            assert_eq!(classic_severity(raw).unwrap(),expected);
        }
        for (raw, expected) in [("BLOCKER",Severity::Critical),("HIGH",Severity::High),("MEDIUM",Severity::Medium),("LOW",Severity::Low),("INFO",Severity::Info)] {
            assert_eq!(impact_severity(raw).unwrap(),expected);
        }
        assert!(classic_severity("NEW").is_err());
        for path in ["../private","/etc/passwd","C:/secrets","dir/../../secret","\\\\server\\share"] { assert!(safe_path(path).is_none()); }
        assert_eq!(safe_path("src/main.rs").as_deref(),Some("src/main.rs"));
    }
}
